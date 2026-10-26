//! flux for agents (docs/AGENT-NATIVE.md): the operations of the agent
//! contract ([`flux_core::agent`]), defined once and served three ways.
//!
//! ```text
//!   GET  /api/v1/agent              the manifest: operations, schemas, skills, policy
//!   POST /api/v1/ops/{name}         run one (JSON in, JSON out; errors {"error": {code, message}})
//!   GET  /api/v1/schemas/{name}     one JSON Schema (`items.edit.input`, `lens`, …)
//!   GET  /api/v1/agent/skills/{n}   a skill's SKILL.md
//!   POST /mcp                       MCP (Streamable HTTP): the operations as tools
//! ```
//!
//! The `flux` command line (crate `flux-cli`) calls the same routes; the web
//! client too. Who may run what follows the caller's role ([`Ctx::may`]);
//! a caller that names an agent (MCP, `X-Flux-Agent`) never sees what a
//! provider's terms keep from AI ([`policy`]).

pub mod fetch;
pub mod files;
pub mod lens;
pub mod mcp;
pub mod ops;
pub mod policy;
pub mod skills;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use axum::extract::{Path as UrlPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use flux_core::agent::{ErrorBody, ErrorCode, Manifest, May, OpError, OpInfo, ServerAbout, CONTRACT};
use flux_core::api::{Id, Role};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::state::{AppState, Caller};

/// What an operation runs with: the server, the caller and the agent
/// acting for it.
#[derive(Clone)]
pub struct Ctx {
    pub st: Arc<AppState>,
    pub caller: Caller,
    /// The agent's name, when the caller is one (MCP, `X-Flux-Agent`).
    pub agent: Option<String>,
}

impl Ctx {
    pub fn role(&self) -> Option<Role> {
        self.caller.identity.as_ref().map(|i| i.role)
    }

    /// The caller's login (`anonymous` without one).
    pub fn login(&self) -> String {
        self.caller
            .identity
            .as_ref()
            .map(|i| i.login_name.clone())
            .unwrap_or_else(|| "anonymous".into())
    }

    pub fn may(&self) -> May {
        let admin = matches!(self.role(), Some(Role::Owner | Role::Admin));
        May {
            apply: admin,
            propose: admin || self.role() == Some(Role::Viewer),
            admin,
        }
    }

    pub fn require_admin(&self) -> Result<(), OpError> {
        if self.may().admin {
            Ok(())
        } else {
            Err(err(
                ErrorCode::Forbidden,
                "Only the server's Owner or an Admin can do that (status says what you may do)",
            ))
        }
    }

    /// Providers this caller must not be shown.
    pub fn hide(&self) -> Vec<String> {
        policy::hidden(self.agent.as_deref())
    }

    pub fn actor(&self) -> flux_catalog::changes::Actor {
        flux_catalog::changes::Actor {
            login: self.login(),
            agent: self.agent.clone(),
        }
    }

    /// The caller's user id (anonymous callers share one).
    pub async fn user_id(&self) -> Result<Id, OpError> {
        self.st.user_id(&self.caller).await.map_err(api_err)
    }
}

/// An operation's error.
pub fn err(code: ErrorCode, message: impl Into<String>) -> OpError {
    OpError {
        code,
        message: message.into(),
    }
}

/// A catalog error as an operation's.
pub fn cat_err(e: flux_catalog::CatalogError) -> OpError {
    use flux_catalog::CatalogError as E;
    match e {
        E::NotFound(what) => err(ErrorCode::NotFound, format!("No {what}")),
        E::Invalid(why) => err(ErrorCode::Invalid, why),
        other => err(ErrorCode::Internal, other.to_string()),
    }
}

/// An HTTP handler error as an operation's.
pub fn api_err(e: crate::error::ApiError) -> OpError {
    let code = match e.status {
        StatusCode::BAD_REQUEST => ErrorCode::Invalid,
        StatusCode::NOT_FOUND | StatusCode::GONE => ErrorCode::NotFound,
        StatusCode::FORBIDDEN => ErrorCode::Forbidden,
        StatusCode::CONFLICT => ErrorCode::Conflict,
        StatusCode::PAYLOAD_TOO_LARGE => ErrorCode::TooLarge,
        s if s == StatusCode::SERVICE_UNAVAILABLE || s == StatusCode::BAD_GATEWAY => ErrorCode::Unavailable,
        _ => ErrorCode::Internal,
    };
    err(code, e.message)
}

type BoxFut = Pin<Box<dyn Future<Output = Result<Value, OpError>> + Send>>;
type Runner = Box<dyn Fn(Ctx, Value) -> BoxFut + Send + Sync>;

/// One operation.
pub struct Op {
    pub info: OpInfo,
    run: Runner,
}

/// The JSON Schema of `T` (2020-12), as a value: self-contained (no
/// `$ref`s, which some agents' tool loaders don't follow), with each
/// description's wrapped lines joined (paragraphs stay).
pub fn schema_of<T: JsonSchema>() -> Value {
    let mut settings = schemars::generate::SchemaSettings::draft2020_12();
    settings.inline_subschemas = true;
    let s = settings.into_generator().into_root_schema_for::<T>();
    let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
    tidy_descriptions(&mut v);
    v
}

/// Joins wrapped description lines, and drops `default`s that say nothing
/// (`null`, empty, an object of nulls).
fn tidy_descriptions(v: &mut Value) {
    match v {
        Value::Object(m) => {
            let empty = |d: &Value| match d {
                Value::Null => true,
                Value::String(s) => s.is_empty(),
                Value::Array(a) => a.is_empty(),
                Value::Object(o) => o.values().all(Value::is_null),
                _ => false,
            };
            if m.get("default").is_some_and(empty) {
                m.remove("default");
            }
            for (k, x) in m.iter_mut() {
                match x {
                    Value::String(s) if k == "description" || k == "title" => {
                        *s = s
                            .split("\n\n")
                            .map(|p| p.split('\n').map(str::trim).collect::<Vec<_>>().join(" "))
                            .collect::<Vec<_>>()
                            .join("\n\n");
                    }
                    other => tidy_descriptions(other),
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(tidy_descriptions),
        _ => {}
    }
}

/// Who may call an operation, in the manifest's words.
pub const ANYONE: &str = "anyone";
pub const SIGNED_IN: &str = "anyone signed in (a tailnet login, or this machine)";
pub const ADMIN: &str = "the Owner or an Admin";
pub const EDITORS: &str = "the Owner or an Admin applies; a Viewer proposes";

/// Define an operation from a typed async function.
#[allow(clippy::too_many_arguments)]
fn op<I, O, F, Fut>(
    name: &str,
    title: &str,
    description: &str,
    effect: flux_core::agent::Effect,
    who: &str,
    idempotent: bool,
    destructive: bool,
    f: F,
) -> Op
where
    I: DeserializeOwned + JsonSchema + Send + 'static,
    O: Serialize + JsonSchema + 'static,
    F: Fn(Ctx, I) -> Fut + Send + Sync + Copy + 'static,
    Fut: Future<Output = Result<O, OpError>> + Send + 'static,
{
    let info = OpInfo {
        name: name.into(),
        title: title.into(),
        description: description.into(),
        effect,
        who: who.into(),
        idempotent,
        destructive,
        input: schema_of::<I>(),
        output: schema_of::<O>(),
    };
    let run: Runner = Box::new(move |ctx, input| {
        let parsed = serde_json::from_value::<I>(normalize_input(input));
        Box::pin(async move {
            let input = parsed.map_err(|e| {
                err(
                    ErrorCode::Invalid,
                    format!("The input doesn't fit the schema: {e}"),
                )
            })?;
            let out = f(ctx, input).await?;
            serde_json::to_value(out).map_err(|e| err(ErrorCode::Internal, e.to_string()))
        })
    });
    Op { info, run }
}

/// `null` or nothing means "no input": an empty object.
fn normalize_input(v: Value) -> Value {
    match v {
        Value::Null => Value::Object(Default::default()),
        v => v,
    }
}

/// Every operation, in the manifest's order.
pub fn all() -> &'static [Op] {
    static OPS: OnceLock<Vec<Op>> = OnceLock::new();
    OPS.get_or_init(ops::define)
}

pub fn find(name: &str) -> Option<&'static Op> {
    let name = name.replace('_', ".");
    all().iter().find(|o| o.info.name == name)
}

/// Run operation `name` for `ctx`.
pub async fn run(ctx: Ctx, name: &str, input: Value) -> Result<Value, OpError> {
    let Some(op) = find(name) else {
        return Err(err(
            ErrorCode::NotFound,
            format!("No operation {name:?}: GET /api/v1/agent lists them"),
        ));
    };
    (op.run)(ctx, input).await
}

/// The agent a request names (`X-Flux-Agent`), tidied: lowercase letters,
/// digits, `.`, `_` and `-`, at most 40.
pub fn agent_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(flux_core::agent::AGENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(agent_name)
}

/// `s` as an agent name, or `None` if nothing of it is usable.
pub fn agent_name(s: &str) -> Option<String> {
    let n: String = s
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let n = n.trim_matches('-').to_string();
    (!n.is_empty()).then_some(n)
}

fn status_of(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::Invalid => StatusCode::BAD_REQUEST,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::Forbidden => StatusCode::FORBIDDEN,
        ErrorCode::Conflict => StatusCode::CONFLICT,
        ErrorCode::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// An operation's error as an HTTP response (JSON, unlike the rest of the
/// API: programs read it).
pub fn error_response(e: OpError) -> Response {
    if e.code == ErrorCode::Internal {
        tracing::warn!("agent operation failed: {}", e.message);
    }
    let status = status_of(e.code);
    let mut r = (status, Json(ErrorBody { error: e })).into_response();
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    r
}

/// `POST /api/v1/ops/{name}`.
pub async fn http_op(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    headers: HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: axum::body::Bytes,
) -> Response {
    let input = if body.iter().all(u8::is_ascii_whitespace) {
        Value::Null
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(v) => v,
            Err(e) => return error_response(err(ErrorCode::Invalid, format!("The body isn't JSON: {e}"))),
        }
    };
    let ctx = Ctx {
        st,
        caller,
        agent: agent_of(&headers),
    };
    match run(ctx, &name, input).await {
        Ok(v) => {
            let mut r = Json(v).into_response();
            r.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-store"),
            );
            r
        }
        Err(e) => error_response(e),
    }
}

/// The manifest.
pub fn manifest(st: &AppState) -> Manifest {
    Manifest {
        contract: CONTRACT.into(),
        server: ServerAbout {
            name: st.cfg.name.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        ops: all().iter().map(|o| o.info.clone()).collect(),
        mcp: "/mcp".into(),
        skills: skills::list(),
        schemas: schema_names(),
        policy: policy::policy(),
    }
}

/// `GET /api/v1/agent`.
pub async fn http_manifest(State(st): State<Arc<AppState>>) -> Json<Manifest> {
    Json(manifest(&st))
}

/// The schemas served by name, besides each operation's input and output.
fn extra_schemas() -> Vec<(&'static str, Value)> {
    vec![
        ("lens", schema_of::<flux_core::agent::LensSpec>()),
        ("fields", schema_of::<flux_core::agent::Fields>()),
        ("item", schema_of::<flux_core::agent::AgentItemDetail>()),
        ("changeset", schema_of::<flux_core::agent::Changeset>()),
        ("sidecar", schema_of::<flux_core::agent::Sidecar>()),
        ("collection_rule", schema_of::<flux_core::agent::CollectionRule>()),
        ("error", schema_of::<ErrorBody>()),
        ("manifest", schema_of::<Manifest>()),
    ]
}

pub fn schema_names() -> Vec<String> {
    let mut v: Vec<String> = extra_schemas().into_iter().map(|(n, _)| n.to_string()).collect();
    for o in all() {
        v.push(format!("{}.input", o.info.name));
        v.push(format!("{}.output", o.info.name));
    }
    v
}

/// A schema by name (`lens`, `items.edit.input`, …).
pub fn schema(name: &str) -> Option<Value> {
    let name = name.strip_suffix(".json").unwrap_or(name);
    if let Some((_, s)) = extra_schemas().into_iter().find(|(n, _)| *n == name) {
        return Some(s);
    }
    let (op, part) = name.rsplit_once('.')?;
    let o = find(op)?;
    match part {
        "input" => Some(o.info.input.clone()),
        "output" => Some(o.info.output.clone()),
        _ => None,
    }
}

/// `GET /api/v1/schemas/{name}`.
pub async fn http_schema(UrlPath(name): UrlPath<String>) -> Response {
    match schema(&name) {
        Some(s) => (
            [(header::CONTENT_TYPE, "application/schema+json")],
            serde_json::to_string_pretty(&s).unwrap_or_default(),
        )
            .into_response(),
        None => error_response(err(ErrorCode::NotFound, format!("No schema {name:?}"))),
    }
}

/// `GET /api/v1/agent/skills/{name}`.
pub async fn http_skill(UrlPath(name): UrlPath<String>) -> Response {
    let name = name.strip_suffix(".md").unwrap_or(&name);
    match skills::get(name) {
        Some(s) => ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], s.text).into_response(),
        None => error_response(err(ErrorCode::NotFound, format!("No skill {name:?}"))),
    }
}

/// Tell the app (and other agents) what a commit changed: the changeset,
/// and the items per library.
pub fn announce(st: &AppState, c: &flux_catalog::changes::Committed) {
    if let Some(cs) = &c.changeset {
        st.events.send(flux_core::api::Event::Changes {
            changeset: cs.clone(),
        });
    }
    type Lists = (Vec<Id>, Vec<Id>, Vec<Id>);
    let mut by_lib: std::collections::BTreeMap<Id, Lists> = Default::default();
    for (item, lib) in &c.added {
        by_lib.entry(*lib).or_default().0.push(*item);
    }
    for (item, lib) in &c.items {
        by_lib.entry(*lib).or_default().1.push(*item);
    }
    for (item, lib) in &c.removed {
        by_lib.entry(*lib).or_default().2.push(*item);
    }
    for (library_id, (added, updated, removed)) in by_lib {
        st.events.send(flux_core::api::Event::ItemsChanged {
            library_id,
            added,
            updated,
            removed,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_are_self_contained_with_joined_descriptions() {
        let s = schema_of::<flux_core::agent::EditInput>();
        let text = s.to_string();
        assert!(!text.contains("$ref"), "{text}");
        let d = s["properties"]["force"]["description"].as_str().unwrap();
        assert!(!d.contains('\n'), "{d}");
    }

    #[test]
    fn agent_names_are_tidied() {
        assert_eq!(agent_name("Claude Code").as_deref(), Some("claude-code"));
        assert_eq!(agent_name(" codex/1.2 ").as_deref(), Some("codex-1.2"));
        assert_eq!(agent_name("---").as_deref(), None);
        assert_eq!(agent_name(&"x".repeat(80)).map(|s| s.len()), Some(40));
    }

    #[test]
    fn every_operation_has_object_schemas_and_a_unique_name() {
        let mut names = std::collections::BTreeSet::new();
        for o in all() {
            assert!(names.insert(o.info.name.clone()), "{} twice", o.info.name);
            assert_eq!(o.info.input["type"], "object", "{} input", o.info.name);
            assert!(!o.info.description.is_empty());
            assert!(schema(&format!("{}.input", o.info.name)).is_some());
        }
        assert!(find("items_find").is_some(), "MCP names map back");
        assert!(schema("lens").is_some() && schema("nope").is_none());
    }
}
