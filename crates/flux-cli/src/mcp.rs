//! `flux mcp`: MCP over stdio for clients that start a process, carried to
//! the server's `/mcp` (Streamable HTTP) one message per line. All the MCP
//! is the server's; this keeps the session id, and when the server has
//! restarted (it answers 404 for the old session) it initializes again
//! with the client's own `initialize` and retries, so the client never
//! notices. Logs go to stderr; stdout carries only JSON-RPC (on Windows with
//! non-ASCII characters as `\u` escapes, which parse to the same strings).

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::client::{Client, Failure};

struct Bridge<'a> {
    c: &'a Client,
    session: Option<String>,
    protocol: Option<String>,
    /// The client's `initialize` request, to replay after a restart.
    init: Option<Value>,
}

struct Answer {
    status: u16,
    session: Option<String>,
    body: String,
}

impl Bridge<'_> {
    fn post(&self, msg: &Value) -> Result<Answer, String> {
        let mut r = self
            .c
            .request(reqwest::Method::POST, "/mcp")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
            .body(msg.to_string());
        if let Some(s) = &self.session {
            r = r.header("mcp-session-id", s);
        }
        if let Some(p) = &self.protocol {
            r = r.header("mcp-protocol-version", p);
        }
        let resp = r
            .send()
            .map_err(|e| format!("flux isn't answering at {}: {e}", self.c.base))?;
        let status = resp.status().as_u16();
        let session = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = resp.text().map_err(|e| e.to_string())?;
        Ok(Answer {
            status,
            session,
            body,
        })
    }

    /// Initialize again with the client's own request (after a restart).
    fn reinitialize(&mut self) -> Result<(), String> {
        let Some(init) = self.init.clone() else {
            return Err("the server lost this session".into());
        };
        self.session = None;
        let a = self.post(&init)?;
        if a.status != 200 {
            return Err(format!(
                "initializing again failed: {} {}",
                a.status,
                a.body.trim()
            ));
        }
        self.session = a.session;
        eprintln!("flux mcp: the server restarted; started a new session");
        let _ = self.post(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        Ok(())
    }

    fn handle(&mut self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let initializing = msg.get("method").and_then(Value::as_str) == Some("initialize");
        if initializing {
            self.init = Some(msg.clone());
            self.session = None;
            self.protocol = None;
        }
        let answer = match self.post(&msg) {
            Ok(a) if a.status == 404 && !initializing && self.session.is_some() => {
                match self.reinitialize() {
                    Ok(()) => self.post(&msg),
                    Err(e) => Err(e),
                }
            }
            other => other,
        };
        let a = match answer {
            Ok(a) => a,
            Err(e) => {
                eprintln!("flux mcp: {e}");
                return id.map(
                    |id| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": e } }),
                );
            }
        };
        if initializing {
            self.session = a.session.clone();
        }
        if a.status == 202 || a.body.trim().is_empty() {
            return None;
        }
        match serde_json::from_str::<Value>(&a.body) {
            Ok(v) => {
                if initializing {
                    self.protocol = v
                        .pointer("/result/protocolVersion")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                Some(v)
            }
            Err(_) => {
                let message = format!("flux answered {}: {}", a.status, a.body.trim());
                eprintln!("flux mcp: {message}");
                id.map(|id| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": message } }))
            }
        }
    }
}

pub fn bridge(c: &Client) -> Result<(), Failure> {
    let mut b = Bridge {
        c,
        session: None,
        protocol: None,
        init: None,
    };
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| Failure::error(format!("reading stdin: {e}")))?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => b.handle(msg),
            Err(e) => Some(
                json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("Parse error: {e}") } }),
            ),
        };
        if let Some(r) = reply {
            // On Windows, \u escapes: a client that decodes our output with a
            // legacy code page still gets the same JSON (the parsed strings
            // are identical, so nothing changes for the others).
            let line = if cfg!(windows) {
                crate::ascii_json(&r.to_string())
            } else {
                r.to_string()
            };
            writeln!(out, "{line}").map_err(|e| Failure::error(e.to_string()))?;
            out.flush().map_err(|e| Failure::error(e.to_string()))?;
        }
    }
    Ok(())
}
