//! MCP, the Model Context Protocol, at `/mcp` (Streamable HTTP; every
//! answer is one JSON body, no server-sent stream).
//!
//! - **Tools** are the operations ([`super::all`]), named with `_` for `.`
//!   (`items_find`), with their input and output schemas and hints.
//!   Results carry `structuredContent` and the same JSON as text;
//!   `art_view` returns the image itself.
//! - **Resources** are the skills (`flux://skills/<name>`, and files
//!   beside them) and the main schemas (`flux://schemas/<name>`).
//! - **Prompts** start the common jobs (`fill_gaps`, `find_art`,
//!   `make_lens`, `review_library`).
//!
//! `initialize` opens a session (`Mcp-Session-Id`) that remembers the
//! client's name: it is the agent its changesets are credited to. Every
//! MCP caller is an agent, so provider content the policy withholds stays
//! out. Who may do what is the caller's role, as on every route: on the
//! tailnet the Tailscale identity is the authentication.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use flux_core::agent::{Effect, ErrorCode};
use serde_json::{json, Map, Value};

use super::{agent_name, agent_of, skills, Ctx};
use crate::state::{AppState, Caller};

/// Protocol versions spoken, newest first.
pub const PROTOCOLS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Sessions kept at most (the oldest goes first).
const MAX_SESSIONS: usize = 512;

pub const SESSION_HEADER: &str = "mcp-session-id";

/// What the server's `instructions` tell every client.
pub fn instructions() -> String {
    let mut s = String::from(
        "flux is the user's self-hosted media server (films, shows, home videos). It has no AI of its own: \
         you identify, describe and illustrate the library, and shape how it's browsed. Start with `status` \
         (libraries, your role, the rules) and `library_gaps` (what's missing). `item_get` shows an item's \
         fields, where each came from, and its files' own evidence; `items_edit` changes up to 100 items as \
         one changeset the user can review and undo in the app. Workflows are in the resources \
         flux://skills/flux-metadata, flux://skills/flux-artwork and flux://skills/flux-lens.\n\nRules:\n",
    );
    for r in super::policy::RULES {
        s.push_str("- ");
        s.push_str(r);
        s.push('\n');
    }
    s
}

struct Session {
    agent: String,
    login: String,
    protocol: &'static str,
    at: Instant,
}

/// Open sessions, by id.
#[derive(Default)]
pub struct Sessions {
    map: Mutex<HashMap<String, Session>>,
    counter: std::sync::atomic::AtomicU64,
}

impl Sessions {
    fn open(&self, agent: String, login: String, protocol: &'static str) -> String {
        let n = self.counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seed = format!("{nanos}:{n}:{}:{agent}:{login}", std::process::id());
        let id = format!("{:032x}", xxhash_rust::xxh3::xxh3_128(seed.as_bytes()));
        let mut m = self.map.lock().unwrap_or_else(|p| p.into_inner());
        if m.len() >= MAX_SESSIONS {
            if let Some(oldest) = m.iter().min_by_key(|(_, s)| s.at).map(|(k, _)| k.clone()) {
                m.remove(&oldest);
            }
        }
        m.insert(
            id.clone(),
            Session {
                agent,
                login,
                protocol,
                at: Instant::now(),
            },
        );
        id
    }

    /// The agent and protocol of session `id`, if it is open and `login`'s.
    fn get(&self, id: &str, login: &str) -> Option<(String, &'static str)> {
        let mut m = self.map.lock().unwrap_or_else(|p| p.into_inner());
        let s = m.get_mut(id).filter(|s| s.login == login)?;
        s.at = Instant::now();
        Some((s.agent.clone(), s.protocol))
    }

    fn close(&self, id: &str) -> bool {
        self.map
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id)
            .is_some()
    }
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// `POST /mcp`.
pub async fn post(
    State(st): State<Arc<AppState>>,
    caller: Caller,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                rpc_error(Value::Null, -32700, format!("Parse error: {e}")),
                None,
            )
        }
    };
    let login = caller
        .identity
        .as_ref()
        .map(|i| i.login_name.clone())
        .unwrap_or_else(|| "anonymous".into());
    let batch = msg.is_array();
    let msgs: Vec<Value> = match msg {
        Value::Array(a) => a,
        m => vec![m],
    };
    if msgs.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            rpc_error(Value::Null, -32600, "Empty batch"),
            None,
        );
    }
    let initializing = msgs
        .iter()
        .any(|m| m.get("method").and_then(Value::as_str) == Some("initialize"));
    let session = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let known = match (&session, initializing) {
        (Some(id), false) => match st.mcp.get(id, &login) {
            Some(s) => Some(s),
            // A session this server doesn't know (it restarted, or closed
            // it): the client starts a new one.
            None => return (StatusCode::NOT_FOUND, "No such MCP session: initialize again").into_response(),
        },
        _ => None,
    };
    let (agent, protocol) = match known {
        Some((a, p)) => (a, p),
        None => (agent_of(&headers).unwrap_or_else(|| "mcp".into()), PROTOCOLS[0]),
    };
    let mut ctx = Ctx {
        st: st.clone(),
        caller,
        agent: Some(agent),
    };
    let mut new_session = None;
    let mut out = Vec::new();
    for m in msgs {
        let id = m.get("id").cloned();
        let Some(method) = m.get("method").and_then(Value::as_str).map(str::to_string) else {
            // A response to a request of ours (we send none): nothing to do.
            if m.get("result").is_none() && m.get("error").is_none() {
                out.push(rpc_error(
                    id.unwrap_or(Value::Null),
                    -32600,
                    "Invalid request: no method",
                ));
            }
            continue;
        };
        let params = m.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            // A notification (`notifications/initialized`, `…/cancelled`).
            continue;
        };
        let reply = match method.as_str() {
            "initialize" => {
                let asked = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let version = PROTOCOLS
                    .iter()
                    .copied()
                    .find(|p| *p == asked)
                    .unwrap_or(PROTOCOLS[0]);
                let client = params
                    .pointer("/clientInfo/name")
                    .and_then(Value::as_str)
                    .and_then(agent_name)
                    .or_else(|| agent_of(&headers))
                    .unwrap_or_else(|| "mcp".into());
                ctx.agent = Some(client.clone());
                new_session = Some(st.mcp.open(client, login.clone(), version));
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {
                        "tools": { "listChanged": false },
                        "resources": { "listChanged": false, "subscribe": false },
                        "prompts": { "listChanged": false },
                    },
                    "serverInfo": { "name": "flux", "title": "flux", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": instructions(),
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools(protocol) })),
            "tools/call" => Ok(call(&ctx, &params).await),
            "resources/list" => Ok(json!({ "resources": resources() })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "resources/read" => read_resource(&params),
            "prompts/list" => Ok(json!({ "prompts": prompts() })),
            "prompts/get" => get_prompt(&params),
            other => Err((-32601, format!("Method not found: {other}"))),
        };
        out.push(match reply {
            Ok(r) => rpc_result(id, r),
            Err((code, msg)) => rpc_error(id, code, msg),
        });
    }
    if out.is_empty() {
        return StatusCode::ACCEPTED.into_response();
    }
    let body = if batch {
        Value::Array(out)
    } else {
        out.pop().unwrap_or(Value::Null)
    };
    json_response(StatusCode::OK, body, new_session)
}

/// `DELETE /mcp`: the client is done with its session.
pub async fn delete(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    match headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) {
        Some(id) if st.mcp.close(id) => StatusCode::NO_CONTENT.into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `GET /mcp`: flux opens no server-sent stream.
pub async fn get() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST, DELETE")]).into_response()
}

fn json_response(status: StatusCode, body: Value, session: Option<String>) -> Response {
    let mut r = (status, axum::Json(body)).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(s) = session.and_then(|s| HeaderValue::from_str(&s).ok()) {
        r.headers_mut().insert(SESSION_HEADER, s);
    }
    r
}

/// The tools (`tools/list`). `outputSchema` (and tool titles) came with
/// protocol 2025-06-18.
fn tools(protocol: &str) -> Vec<Value> {
    let modern = protocol >= "2025-06-18";
    super::all()
        .iter()
        .map(|o| {
            let i = &o.info;
            let mut t = Map::new();
            t.insert("name".into(), Value::String(i.name.replace('.', "_")));
            if modern {
                t.insert("title".into(), Value::String(i.title.clone()));
            }
            t.insert(
                "description".into(),
                Value::String(format!("{} Who may call it: {}.", i.description, i.who)),
            );
            t.insert("inputSchema".into(), tool_schema(&i.input));
            if modern && i.name != "art.view" {
                t.insert("outputSchema".into(), tool_schema(&i.output));
            }
            t.insert(
                "annotations".into(),
                json!({
                    "title": i.title,
                    "readOnlyHint": i.effect == Effect::Read,
                    "destructiveHint": i.destructive,
                    "idempotentHint": i.idempotent,
                    "openWorldHint": i.name == "art.add",
                }),
            );
            Value::Object(t)
        })
        .collect()
}

/// A schema as tool loaders take it: without `$schema` (the dialect is
/// MCP's default, 2020-12) and without the numeric `format`s schemars adds
/// (`int64`, `uint32`, …), which JSON Schema leaves undefined and some
/// clients refuse. The ranges (`minimum`) stay.
fn tool_schema(v: &Value) -> Value {
    fn strip(v: &mut Value) {
        match v {
            Value::Object(m) => {
                if m.get("format").and_then(Value::as_str).is_some_and(|f| {
                    matches!(
                        f,
                        "int8"
                            | "int16"
                            | "int32"
                            | "int64"
                            | "uint"
                            | "uint8"
                            | "uint16"
                            | "uint32"
                            | "uint64"
                            | "float"
                            | "double"
                    )
                }) {
                    m.remove("format");
                }
                m.values_mut().for_each(strip);
            }
            Value::Array(a) => a.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let mut v = v.clone();
    if let Value::Object(m) = &mut v {
        m.remove("$schema");
    }
    strip(&mut v);
    v
}

/// `tools/call`: a tool's failure is a result with `isError`, not a
/// protocol error, so the model sees what went wrong.
async fn call(ctx: &Ctx, params: &Value) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    match super::run(ctx.clone(), &name, args).await {
        Ok(v) if name == "art_view" || name == "art.view" => {
            let mime = v
                .get("mime")
                .and_then(Value::as_str)
                .unwrap_or("image/jpeg")
                .to_string();
            let data = v
                .get("data_base64")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let art = v.get("art").cloned().unwrap_or(Value::Null);
            json!({
                "content": [
                    { "type": "image", "data": data, "mimeType": mime },
                    { "type": "text", "text": format!("Image {art} ({mime})") }
                ]
            })
        }
        Ok(v) => json!({
            "content": [{ "type": "text", "text": v.to_string() }],
            "structuredContent": v,
        }),
        Err(e) => json!({
            "content": [{ "type": "text", "text": format!("{}: {}", code_word(e.code), e.message) }],
            "isError": true,
        }),
    }
}

fn code_word(c: ErrorCode) -> &'static str {
    match c {
        ErrorCode::Invalid => "invalid",
        ErrorCode::NotFound => "not_found",
        ErrorCode::Forbidden => "forbidden",
        ErrorCode::Conflict => "conflict",
        ErrorCode::Unavailable => "unavailable",
        ErrorCode::TooLarge => "too_large",
        ErrorCode::Internal => "internal",
    }
}

const SCHEMAS: [(&str, &str); 4] = [
    ("lens", "The lens kit format"),
    ("fields", "The fields an agent can set"),
    ("item", "An item as item_get returns it"),
    ("changeset", "A changeset as changes_get returns it"),
];

fn resources() -> Vec<Value> {
    let mut v = Vec::new();
    for s in &skills::SKILLS {
        v.push(json!({
            "uri": format!("flux://skills/{}", s.name),
            "name": s.name,
            "title": format!("Skill: {}", s.name),
            "description": s.description(),
            "mimeType": "text/markdown",
        }));
        for (file, _) in s.files {
            v.push(json!({
                "uri": format!("flux://skills/{}/{file}", s.name),
                "name": format!("{}/{file}", s.name),
                "mimeType": "text/markdown",
            }));
        }
    }
    for (name, title) in SCHEMAS {
        v.push(json!({
            "uri": format!("flux://schemas/{name}"),
            "name": format!("schema-{name}"),
            "title": title,
            "mimeType": "application/schema+json",
        }));
    }
    v
}

fn read_resource(params: &Value) -> Result<Value, (i64, String)> {
    let uri = params.get("uri").and_then(Value::as_str).unwrap_or_default();
    let (mime, text) = if let Some(rest) = uri.strip_prefix("flux://skills/") {
        let (name, file) = rest.split_once('/').unwrap_or((rest, ""));
        let s = skills::get(name).ok_or((-32002, format!("Resource not found: {uri}")))?;
        let text = if file.is_empty() || file == "SKILL.md" {
            s.text
        } else {
            s.files
                .iter()
                .find(|(f, _)| *f == file)
                .map(|(_, t)| *t)
                .ok_or((-32002, format!("Resource not found: {uri}")))?
        };
        ("text/markdown", text.to_string())
    } else if let Some(name) = uri.strip_prefix("flux://schemas/") {
        let s = super::schema(name).ok_or((-32002, format!("Resource not found: {uri}")))?;
        (
            "application/schema+json",
            serde_json::to_string_pretty(&s).unwrap_or_default(),
        )
    } else {
        return Err((-32002, format!("Resource not found: {uri}")));
    };
    Ok(json!({ "contents": [{ "uri": uri, "mimeType": mime, "text": text }] }))
}

struct Prompt {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    args: &'static [(&'static str, &'static str, bool)],
    skill: &'static str,
}

const PROMPTS: [Prompt; 4] = [
    Prompt {
        name: "review_library",
        title: "Review my library",
        description: "Survey the library and report what's missing, changing nothing.",
        args: &[],
        skill: "flux",
    },
    Prompt {
        name: "fill_gaps",
        title: "Fill in what's missing",
        description: "Identify items and fill in missing information from the open web, with sources.",
        args: &[
            ("library", "A library id (default: all)", false),
            (
                "gap",
                "identity, year, overview or episode_titles (default: identity)",
                false,
            ),
        ],
        skill: "flux-metadata",
    },
    Prompt {
        name: "find_art",
        title: "Find artwork",
        description: "Find and add credited posters and backdrops where flux has only its own.",
        args: &[("item", "One item id (default: items with a poster gap)", false)],
        skill: "flux-artwork",
    },
    Prompt {
        name: "make_lens",
        title: "Make a lens",
        description: "Design a new way of browsing the library and add it to the app.",
        args: &[("idea", "What the lens should look like", true)],
        skill: "flux-lens",
    },
];

fn prompts() -> Vec<Value> {
    PROMPTS
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "title": p.title,
                "description": p.description,
                "arguments": p.args.iter().map(|(n, d, r)| json!({ "name": n, "description": d, "required": r })).collect::<Vec<_>>(),
            })
        })
        .collect()
}

fn get_prompt(params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
    let p = PROMPTS
        .iter()
        .find(|p| p.name == name)
        .ok_or((-32602, format!("No prompt {name:?}")))?;
    let arg = |k: &str| {
        params
            .pointer(&format!("/arguments/{k}"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    for (n, _, required) in p.args {
        if *required && arg(n).is_none() {
            return Err((-32602, format!("The prompt {name} needs {n}")));
        }
    }
    let ask = match p.name {
        "review_library" => "Survey my flux library with the flux tools: status, then library_gaps, then a few items_find calls. \
             Tell me what's missing and what you'd suggest, briefly. Don't change anything yet."
            .to_string(),
        "fill_gaps" => format!(
            "Fill in what's missing in my flux library{}, focusing on the {} gap. Follow the skill below: read each item's evidence, \
             research the open web, cross-check, then edit with sources. Show me your plan for the first few items before a big batch.",
            arg("library").map(|l| format!(" (library {l})")).unwrap_or_default(),
            arg("gap").unwrap_or_else(|| "identity".into())
        ),
        "find_art" => format!(
            "Find proper artwork for {} in my flux library, following the skill below. Credit every image and tell me what you added.",
            arg("item").map(|i| format!("item {i}")).unwrap_or_else(|| "the items whose posters are flux's own frames".into())
        ),
        _ => format!(
            "Design a flux lens: {}. Follow the skill below and its reference (resource flux://skills/flux-lens/reference.md); \
             check it with lens_check, then add it with lens_add and tell me how to open it.",
            arg("idea").unwrap_or_default()
        ),
    };
    let skill = skills::get(p.skill).map(|s| s.text).unwrap_or_default();
    Ok(json!({
        "description": p.description,
        "messages": [
            { "role": "user", "content": { "type": "text", "text": ask } },
            { "role": "user", "content": {
                "type": "resource",
                "resource": { "uri": format!("flux://skills/{}", p.skill), "mimeType": "text/markdown", "text": skill }
            } }
        ]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_carry_hints_and_output_schemas_from_2025_06_18() {
        let new = tools("2025-11-25");
        let find = new.iter().find(|t| t["name"] == "items_find").unwrap();
        assert_eq!(find["annotations"]["readOnlyHint"], true);
        assert!(find.get("outputSchema").is_some());
        let edit = new.iter().find(|t| t["name"] == "items_edit").unwrap();
        assert_eq!(edit["annotations"]["readOnlyHint"], false);
        assert_eq!(edit["annotations"]["destructiveHint"], true);
        let view = new.iter().find(|t| t["name"] == "art_view").unwrap();
        assert!(
            view.get("outputSchema").is_none(),
            "an image, not structured content"
        );
        let old = tools("2025-03-26");
        assert!(old
            .iter()
            .all(|t| t.get("outputSchema").is_none() && t.get("title").is_none()));
        // What tool loaders take: no dialect line, no numeric formats.
        assert_eq!(find["inputSchema"]["type"], "object");
        let text = Value::Array(new.clone()).to_string();
        assert!(!text.contains("$schema") && !text.contains("\"int64\"") && !text.contains("\"uint32\""));
    }

    #[test]
    fn resources_and_prompts_read_back() {
        for r in resources() {
            let uri = r["uri"].as_str().unwrap();
            let got = read_resource(&json!({ "uri": uri })).unwrap();
            assert!(!got["contents"][0]["text"].as_str().unwrap().is_empty(), "{uri}");
        }
        assert!(read_resource(&json!({ "uri": "flux://nope" })).is_err());
        let p = get_prompt(
            &json!({ "name": "make_lens", "arguments": { "idea": "an airport departure board" } }),
        )
        .unwrap();
        assert!(p["messages"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("departure board"));
        assert!(
            get_prompt(&json!({ "name": "make_lens" })).is_err(),
            "idea is required"
        );
        assert_eq!(prompts().len(), 4);
    }

    #[test]
    fn sessions_belong_to_their_login() {
        let s = Sessions::default();
        let id = s.open("claude-code".into(), "a@x".into(), PROTOCOLS[0]);
        assert_eq!(s.get(&id, "a@x").map(|v| v.0), Some("claude-code".into()));
        assert!(s.get(&id, "b@x").is_none());
        assert!(s.close(&id));
        assert!(s.get(&id, "a@x").is_none());
    }
}
