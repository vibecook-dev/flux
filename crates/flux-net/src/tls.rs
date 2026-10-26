//! HTTPS on the tailnet listener with this machine's Tailscale (system mode).
//!
//! Browsers offer some features only to secure pages (WebGPU among them), and
//! plain HTTP from another device is not one, even inside a WireGuard tunnel.
//! So flux can also serve HTTPS, with the MagicDNS certificate tailscaled
//! gets from Let's Encrypt (`tailscale cert`; needs HTTPS certificates
//! enabled for the tailnet).
//!
//! - **One port, both protocols.** The first byte of a connection tells them
//!   apart: a TLS ClientHello starts with a handshake record (`0x16`), an
//!   HTTP request with a method name. Plain HTTP keeps working for links and
//!   clients that use it.
//! - **Identity is unchanged.** TLS ends in flux itself, so the peer address
//!   (and with it WhoIs) is the caller's own, as over plain HTTP.
//! - **Renewal.** tailscaled renews the certificate itself; flux asks again
//!   every [`REFRESH_EVERY`] and serves the new one from the next handshake
//!   ([`CertStore`]).

use std::sync::{Arc, RwLock};
use std::time::Duration;

use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;
use tokio::net::TcpStream;

/// The first byte of every TLS connection: a handshake record.
const HANDSHAKE_RECORD: u8 = 0x16;

/// How long a client may take to finish the TLS handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the certificate is fetched again (tailscaled renews it about a
/// month before it expires and hands out the new one from then on).
pub const REFRESH_EVERY: Duration = Duration::from_secs(12 * 3600);

/// The crypto every flux TLS listener uses: *ring*, as reqwest does (aws-lc-rs
/// needs CMake and NASM to build on Windows).
pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A certificate and its key from PEM: every `CERTIFICATE` block (the chain,
/// leaf first) and the private key, in any order. `tailscale cert
/// --cert-file - --key-file -` prints exactly this.
pub fn certified_key_from_pem(pem: &[u8]) -> Result<CertifiedKey, String> {
    let chain = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("reading the certificate: {e}"))?;
    if chain.is_empty() {
        return Err("no certificate in the output".into());
    }
    let key = PrivateKeyDer::from_pem_slice(pem).map_err(|e| format!("reading the private key: {e}"))?;
    CertifiedKey::from_der(chain, key, &provider()).map_err(|e| format!("certificate and key: {e}"))
}

/// The certificate being served. It can be replaced while connections are
/// open: each handshake takes the current one.
#[derive(Debug, Default)]
pub struct CertStore {
    current: RwLock<Option<Arc<CertifiedKey>>>,
}

impl CertStore {
    /// Serve `key` from the next handshake on.
    pub fn set(&self, key: CertifiedKey) {
        *self.current.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(key));
    }

    /// The certificate being served, if one is loaded.
    pub fn current(&self) -> Option<Arc<CertifiedKey>> {
        self.current.read().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        // One name per server: whatever the client asked for, this is it.
        self.current()
    }
}

/// The rustls settings of a flux TLS listener: TLS 1.2 and 1.3, HTTP/1.1
/// (flux serves no HTTP/2), certificates from `certs`.
pub fn server_config(certs: Arc<CertStore>) -> Result<Arc<ServerConfig>, String> {
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS settings: {e}"))?
        .with_no_client_auth()
        .with_cert_resolver(certs);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// What a new connection speaks, from its first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opening {
    /// A TLS handshake.
    Tls,
    /// Anything else: plain HTTP (or junk hyper will refuse).
    Plain,
    /// Closed, failed, or silent for longer than the wait.
    Gone,
}

/// Wait up to `wait` for the first byte of `stream`, without consuming it.
pub async fn opening(stream: &TcpStream, wait: Duration) -> Opening {
    let mut first = [0u8; 1];
    match tokio::time::timeout(wait, stream.peek(&mut first)).await {
        Ok(Ok(n)) if n > 0 => {
            if first[0] == HANDSHAKE_RECORD {
                Opening::Tls
            } else {
                Opening::Plain
            }
        }
        _ => Opening::Gone,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A self-signed certificate for `name`, as `tailscale cert` prints it:
    /// the certificate, then the key.
    pub(crate) fn self_signed(name: &str) -> (String, CertificateDer<'static>) {
        let c = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
        (
            format!("{}{}", c.cert.pem(), c.signing_key.serialize_pem()),
            c.cert.der().clone(),
        )
    }

    #[test]
    fn pem_from_tailscale_cert_loads_in_either_order() {
        let (pem, der) = self_signed("flux.tail1234.ts.net");
        let key = certified_key_from_pem(pem.as_bytes()).unwrap();
        assert_eq!(key.cert, vec![der]);
        // Key first (LocalAPI's own `type=pair` order) works too.
        let (cert, key_pem) = pem.split_at(pem.find("-----BEGIN PRIVATE KEY").unwrap());
        assert!(certified_key_from_pem(format!("{key_pem}{cert}").as_bytes()).is_ok());
    }

    #[test]
    fn mismatched_or_missing_parts_are_refused() {
        let (a, _) = self_signed("a.example");
        let (b, _) = self_signed("b.example");
        let cert_a = &a[..a.find("-----BEGIN PRIVATE KEY").unwrap()];
        let key_b = &b[b.find("-----BEGIN PRIVATE KEY").unwrap()..];
        let err = certified_key_from_pem(format!("{cert_a}{key_b}").as_bytes()).unwrap_err();
        assert!(err.contains("certificate and key"), "{err}");
        assert!(certified_key_from_pem(key_b.as_bytes())
            .unwrap_err()
            .contains("no certificate"));
        assert!(certified_key_from_pem(cert_a.as_bytes())
            .unwrap_err()
            .contains("private key"));
        assert!(certified_key_from_pem(b"").is_err());
    }

    #[test]
    fn the_store_serves_the_latest_certificate() {
        let store = CertStore::default();
        assert!(store.current().is_none());
        let (a, der_a) = self_signed("a.example");
        store.set(certified_key_from_pem(a.as_bytes()).unwrap());
        assert_eq!(store.current().unwrap().cert[0], der_a);
        let (b, der_b) = self_signed("a.example");
        store.set(certified_key_from_pem(b.as_bytes()).unwrap());
        assert_eq!(store.current().unwrap().cert[0], der_b);
        assert!(server_config(Arc::new(store)).is_ok());
    }

    #[tokio::test]
    async fn the_first_byte_tells_tls_from_http() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        for (bytes, want) in [
            (&[0x16u8, 0x03, 0x01][..], Opening::Tls),
            (b"GET / HTTP/1.1\r\n", Opening::Plain),
            (b"", Opening::Gone),
        ] {
            let mut client = TcpStream::connect(addr).await.unwrap();
            let (mut server, _) = listener.accept().await.unwrap();
            if bytes.is_empty() {
                client.shutdown().await.unwrap();
            } else {
                client.write_all(bytes).await.unwrap();
            }
            assert_eq!(opening(&server, Duration::from_secs(5)).await, want);
            // Peeking consumed nothing.
            if !bytes.is_empty() {
                let mut buf = vec![0u8; bytes.len()];
                tokio::io::AsyncReadExt::read_exact(&mut server, &mut buf)
                    .await
                    .unwrap();
                assert_eq!(buf, bytes);
            }
        }
        // A client that never speaks is dropped after the wait.
        let _quiet = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        assert_eq!(opening(&server, Duration::from_millis(50)).await, Opening::Gone);
    }
}
