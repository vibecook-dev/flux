//! Is flux up, and where: `server.json` in the data dir (written by
//! flux-server once it listens: its address, process and version) and
//! `GET /api/v1/server`, over a plain HTTP/1.0 request on loopback.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

/// flux-server's address when nothing says otherwise (its default `--port`).
pub const DEFAULT_URL: &str = "http://127.0.0.1:8096";

/// What `server.json` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFile {
    pub url: String,
    pub pid: Option<u32>,
}

pub fn read_server_file(data_dir: &Path) -> Option<ServerFile> {
    let text = std::fs::read_to_string(data_dir.join("server.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(ServerFile {
        url: v["url"].as_str()?.to_string(),
        pid: v["pid"].as_u64().and_then(|p| u32::try_from(p).ok()),
    })
}

/// The `host:port` of an `http://` URL.
fn address(url: &str) -> Option<SocketAddr> {
    let rest = url.strip_prefix("http://")?;
    let host_port = rest.split('/').next()?;
    host_port.to_socket_addrs().ok()?.next()
}

/// The version of the flux answering at `url`, if one does within `timeout`.
pub fn version_at(url: &str, timeout: Duration) -> Option<String> {
    let addr = address(url)?;
    let mut s = TcpStream::connect_timeout(&addr, timeout).ok()?;
    s.set_read_timeout(Some(timeout)).ok()?;
    s.set_write_timeout(Some(timeout)).ok()?;
    let request =
        format!("GET /api/v1/server HTTP/1.0\r\nHost: {addr}\r\nAccept: application/json\r\nConnection: close\r\n\r\n");
    s.write_all(request.as_bytes()).ok()?;
    let mut answer = Vec::new();
    s.take(1 << 20).read_to_end(&mut answer).ok()?;
    version_in(&String::from_utf8_lossy(&answer))
}

/// The version in an HTTP answer to `GET /api/v1/server`.
fn version_in(answer: &str) -> Option<String> {
    let (head, body) = answer.split_once("\r\n\r\n")?;
    let status = head.lines().next()?;
    if status.split_whitespace().nth(1) != Some("200") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    v["version"].as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn reads_server_json() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(read_server_file(d.path()), None);
        std::fs::write(
            d.path().join("server.json"),
            r#"{"url":"http://127.0.0.1:8096","pid":4242,"version":"0.1.0"}"#,
        )
        .unwrap();
        assert_eq!(
            read_server_file(d.path()),
            Some(ServerFile {
                url: "http://127.0.0.1:8096".into(),
                pid: Some(4242)
            })
        );
    }

    #[test]
    fn reads_the_version_from_an_answer() {
        let ok = "HTTP/1.0 200 OK\r\ncontent-type: application/json\r\n\r\n{\"name\":\"flux\",\"version\":\"0.1.0-alpha.5\"}";
        assert_eq!(version_in(ok).as_deref(), Some("0.1.0-alpha.5"));
        assert_eq!(version_in("HTTP/1.0 503 Service Unavailable\r\n\r\n{}"), None);
        assert_eq!(version_in("HTTP/1.0 200 OK\r\n\r\n<html>"), None);
        assert_eq!(version_in("garbage"), None);
    }

    #[test]
    fn asks_a_server() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let t = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 256];
            while !req.ends_with(b"\r\n\r\n") {
                let n = c.read(&mut buf).unwrap();
                assert!(n > 0, "the request ended early");
                req.extend_from_slice(&buf[..n]);
            }
            assert!(String::from_utf8_lossy(&req).starts_with("GET /api/v1/server HTTP/1.0\r\n"));
            c.write_all(b"HTTP/1.0 200 OK\r\n\r\n{\"version\":\"9.9.9\"}")
                .unwrap();
        });
        assert_eq!(version_at(&url, Duration::from_secs(5)).as_deref(), Some("9.9.9"));
        t.join().unwrap();
        // Nothing listens there any more.
        assert_eq!(version_at(&url, Duration::from_millis(500)), None);
    }
}
