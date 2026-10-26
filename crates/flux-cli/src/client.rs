//! Talking to the server: where it is, the operations, errors.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

pub const DEFAULT_SERVER: &str = "http://127.0.0.1:8096";

/// A failure, as the CLI reports it: a code word, a message, an exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub exit: u8,
}

impl Failure {
    pub fn new(code: &str, message: impl Into<String>, exit: u8) -> Failure {
        Failure {
            code: code.into(),
            message: message.into(),
            exit,
        }
    }
    pub fn usage(m: impl Into<String>) -> Failure {
        Failure::new("usage", m, 2)
    }
    pub fn error(m: impl Into<String>) -> Failure {
        Failure::new("error", m, 1)
    }
    pub fn not_found(m: impl Into<String>) -> Failure {
        Failure::new("not_found", m, 3)
    }

    /// An operation's error body (`{"error": {code, message}}`).
    fn from_body(status: u16, body: &str) -> Failure {
        let v: Option<Value> = serde_json::from_str(body).ok();
        let code = v
            .as_ref()
            .and_then(|v| v.pointer("/error/code"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| match status {
                403 => "forbidden".into(),
                404 => "not_found".into(),
                409 => "conflict".into(),
                _ => "error".into(),
            });
        let message = v
            .as_ref()
            .and_then(|v| v.pointer("/error/message"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| body.trim().to_string());
        let exit = match code.as_str() {
            "not_found" => 3,
            "conflict" => 4,
            "forbidden" => 5,
            _ => 1,
        };
        Failure { code, message, exit }
    }
}

/// The server to talk to: `explicit` (`--server`, `FLUX_SERVER`), else the
/// one a running server named in `server.json` of the data dir (`data_dir`,
/// then the default one) if it answers, else [`DEFAULT_SERVER`].
pub fn server_url(explicit: Option<&str>, data_dir: Option<&Path>) -> String {
    if let Some(s) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return normalize(s);
    }
    let dirs = data_dir
        .map(Path::to_path_buf)
        .into_iter()
        .chain(default_data_dir());
    for d in dirs {
        let Some(url) = std::fs::read_to_string(d.join("server.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v["url"].as_str().map(str::to_string))
        else {
            continue;
        };
        if answers(&url) {
            return normalize(&url);
        }
    }
    DEFAULT_SERVER.into()
}

/// `host:port` as `http://host:port`; no trailing slash.
pub fn normalize(s: &str) -> String {
    let s = s.trim().trim_end_matches('/');
    if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else {
        format!("http://{s}")
    }
}

/// Something listens at `url`'s host and port (a quick TCP connect).
fn answers(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split('/').next().unwrap_or_default();
    let with_port = if authority
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.bytes().all(|b| b.is_ascii_digit()))
    {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    with_port
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .is_some_and(|a| TcpStream::connect_timeout(&a, Duration::from_millis(400)).is_ok())
}

/// The data dir a flux server uses by default here.
pub fn default_data_dir() -> Option<PathBuf> {
    let var = |n: &str| std::env::var_os(n).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        return var("LOCALAPPDATA").map(|d| d.join("flux"));
    }
    let home = var("HOME");
    if cfg!(target_os = "macos") {
        return home.map(|h| h.join("Library/Application Support/flux"));
    }
    var("XDG_DATA_HOME")
        .map(|x| x.join("flux"))
        .or_else(|| home.map(|h| h.join(".local/share/flux")))
}

pub struct Client {
    pub base: String,
    pub agent: Option<String>,
    http: reqwest::blocking::Client,
}

impl Client {
    pub fn new(base: String, agent: Option<String>) -> Client {
        Client {
            base,
            agent,
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(120))
                .user_agent(concat!("flux-cli/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
        }
    }

    fn unreachable(&self, e: &reqwest::Error) -> Failure {
        Failure::new(
            "unreachable",
            format!(
                "flux isn't answering at {} ({e}). Is it running? Pass --server URL (or set FLUX_SERVER) for another one.",
                self.base
            ),
            6,
        )
    }

    pub fn request(&self, method: reqwest::Method, path: &str) -> reqwest::blocking::RequestBuilder {
        let mut r = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header(flux_core::api::REQUEST_HEADER, "1");
        if let Some(a) = &self.agent {
            r = r.header(flux_core::agent::AGENT_HEADER, a);
        }
        r
    }

    /// Run operation `name` with `input`.
    pub fn op(&self, name: &str, input: Value) -> Result<Value, Failure> {
        let resp = self
            .request(reqwest::Method::POST, &format!("/api/v1/ops/{name}"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(input.to_string())
            .send()
            .map_err(|e| self.unreachable(&e))?;
        let status = resp.status();
        let body = resp.text().map_err(|e| self.unreachable(&e))?;
        if !status.is_success() {
            return Err(Failure::from_body(status.as_u16(), &body));
        }
        serde_json::from_str(&body)
            .map_err(|e| Failure::error(format!("the server's answer isn't JSON: {e}")))
    }

    pub fn get_json(&self, path: &str) -> Result<Value, Failure> {
        let resp = self
            .request(reqwest::Method::GET, path)
            .send()
            .map_err(|e| self.unreachable(&e))?;
        let status = resp.status();
        let body = resp.text().map_err(|e| self.unreachable(&e))?;
        if !status.is_success() {
            return Err(Failure::from_body(status.as_u16(), &body));
        }
        serde_json::from_str(&body)
            .map_err(|e| Failure::error(format!("the server's answer isn't JSON: {e}")))
    }

    /// Print the event stream as JSON lines until it ends.
    pub fn events(&self) -> Result<(), Failure> {
        let http = reqwest::blocking::Client::builder()
            .timeout(None)
            .build()
            .map_err(|e| Failure::error(e.to_string()))?;
        let resp = http
            .get(format!("{}/api/v1/events", self.base))
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .map_err(|e| self.unreachable(&e))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(Failure::from_body(status, &resp.text().unwrap_or_default()));
        }
        let mut out = std::io::stdout().lock();
        for line in BufReader::new(resp).lines() {
            let line =
                line.map_err(|e| Failure::new("unreachable", format!("the event stream ended: {e}"), 6))?;
            if let Some(data) = line.strip_prefix("data:") {
                let _ = writeln!(out, "{}", data.trim());
                let _ = out.flush();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_are_named_and_found() {
        assert_eq!(
            normalize("flux.tail1234.ts.net:8096/"),
            "http://flux.tail1234.ts.net:8096"
        );
        assert_eq!(normalize("https://x:1"), "https://x:1");
        assert_eq!(server_url(Some("https://a:1"), None), "https://a:1");
        // A server.json naming a server that doesn't answer is skipped.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.json"), r#"{"url": "http://127.0.0.1:9"}"#).unwrap();
        let got = server_url(None, Some(dir.path()));
        assert_ne!(got, "http://127.0.0.1:9");
        // One that answers is used.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        std::fs::write(dir.path().join("server.json"), format!(r#"{{"url": "{url}"}}"#)).unwrap();
        assert_eq!(server_url(None, Some(dir.path())), url);
    }

    #[test]
    fn errors_keep_the_servers_code() {
        let f = Failure::from_body(
            409,
            r#"{"error": {"code": "conflict", "message": "changeset 3 is reverted"}}"#,
        );
        assert_eq!((f.code.as_str(), f.exit), ("conflict", 4));
        let f = Failure::from_body(403, "This request needs the header X-Flux-Request: 1");
        assert_eq!((f.code.as_str(), f.exit), ("forbidden", 5));
        assert!(f.message.contains("X-Flux-Request"));
    }
}
