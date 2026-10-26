//! Fetching and checking an image an agent adds (`art.add`).
//!
//! - **Fetch** (`url`): `http` and `https` only, at most [`MAX_REDIRECTS`]
//!   redirects, [`MAX_BYTES`] and [`TIMEOUT`]. Each hop's host is resolved
//!   here and its address pinned for the connection, so a name can't
//!   resolve to a public address for the check and to this machine for the
//!   request. Loopback, link-local, unspecified and multicast addresses are
//!   refused: an agent can't make flux fetch from itself or a cloud
//!   metadata endpoint. Addresses on the LAN are allowed (a NAS's images).
//! - **Check**: the bytes must decode as JPEG, PNG or WebP within
//!   [`MAX_SIDE`]; they are re-encoded (JPEG, or PNG for a logo, which keeps
//!   its transparency) at most [`KEEP_SIDE`] on the longer side, so what
//!   flux stores is an image it made, without the original's metadata.

use std::io::Cursor;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use flux_core::agent::{ArtKind, ErrorCode, OpError};

use super::err;

pub const MAX_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_REDIRECTS: usize = 3;
pub const TIMEOUT: Duration = Duration::from_secs(20);
/// Images larger than this on a side are refused (decoding bombs).
pub const MAX_SIDE: u32 = 12_000;
/// What flux keeps: at most this on the longer side.
pub const KEEP_SIDE: u32 = 3_000;

/// Whether flux may fetch from `ip`.
pub fn allowed(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast())
        }
        IpAddr::V6(v6) => {
            let link_local = (v6.segments()[0] & 0xffc0) == 0xfe80;
            !(v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || link_local)
        }
    }
}

/// The bytes at `url`.
pub async fn fetch(url: &str) -> Result<Vec<u8>, OpError> {
    let mut url = reqwest::Url::parse(url.trim())
        .map_err(|_| err(ErrorCode::Invalid, format!("{url:?} is not a URL")))?;
    for _ in 0..=MAX_REDIRECTS {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(err(
                ErrorCode::Invalid,
                "flux fetches images over http or https only",
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| err(ErrorCode::Invalid, "the URL has no host"))?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let port = url.port_or_known_default().unwrap_or(443);
        let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => tokio::net::lookup_host((host.as_str(), port))
                .await
                .map_err(|e| err(ErrorCode::Unavailable, format!("Can't find {host}: {e}")))?
                .collect(),
        };
        if addrs.is_empty() {
            return Err(err(ErrorCode::Unavailable, format!("Can't find {host}")));
        }
        if let Some(bad) = addrs.iter().find(|a| !allowed(a.ip())) {
            return Err(err(
                ErrorCode::Forbidden,
                format!(
                    "{host} is {}: flux doesn't fetch from this machine or link-local addresses",
                    bad.ip()
                ),
            ));
        }
        let mut b = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT)
            .user_agent(concat!(
                "flux/",
                env!("CARGO_PKG_VERSION"),
                " (artwork for a personal media library)"
            ));
        if host.parse::<IpAddr>().is_err() {
            b = b.resolve(&host, addrs[0]);
        }
        let client = b.build().map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        let mut resp = client
            .get(url.clone())
            .header(
                reqwest::header::ACCEPT,
                "image/avif;q=0.1, image/webp, image/png, image/jpeg, */*;q=0.5",
            )
            .send()
            .await
            .map_err(|e| err(ErrorCode::Unavailable, format!("Fetching the image failed: {e}")))?;
        if resp.status().is_redirection() {
            let next = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|l| url.join(l).ok())
                .ok_or_else(|| err(ErrorCode::Unavailable, "a redirect without a location"))?;
            url = next;
            continue;
        }
        if !resp.status().is_success() {
            return Err(err(
                ErrorCode::Unavailable,
                format!("The image's server answered {}", resp.status()),
            ));
        }
        if resp.content_length().is_some_and(|n| n as usize > MAX_BYTES) {
            return Err(too_large());
        }
        let mut out = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| err(ErrorCode::Unavailable, format!("Fetching the image failed: {e}")))?
        {
            if out.len() + chunk.len() > MAX_BYTES {
                return Err(too_large());
            }
            out.extend_from_slice(&chunk);
        }
        return Ok(out);
    }
    Err(err(
        ErrorCode::Unavailable,
        format!("More than {MAX_REDIRECTS} redirects"),
    ))
}

fn too_large() -> OpError {
    err(
        ErrorCode::TooLarge,
        format!("Images are at most {} MB", MAX_BYTES / (1024 * 1024)),
    )
}

/// An image as flux keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub bytes: Vec<u8>,
    /// `jpg` or `png`.
    pub ext: &'static str,
    pub width: u32,
    pub height: u32,
    pub blurhash: Option<String>,
}

/// Decode `bytes`, scale down to [`KEEP_SIDE`], re-encode. Blocking.
pub fn prepare(bytes: &[u8], kind: ArtKind) -> Result<Prepared, OpError> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| err(ErrorCode::Invalid, format!("That isn't an image: {e}")))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| {
        err(
            ErrorCode::Invalid,
            format!("That isn't a JPEG, PNG or WebP image flux can read: {e}"),
        )
    })?;
    let img = if img.width().max(img.height()) > KEEP_SIDE {
        img.resize(KEEP_SIDE, KEEP_SIDE, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    let (width, height) = (img.width(), img.height());
    if width < 16 || height < 16 {
        return Err(err(ErrorCode::Invalid, "The image is too small to show"));
    }
    let rgb = img.to_rgb8();
    let blurhash = {
        let small = image::imageops::resize(
            &rgb,
            32,
            (32 * height / width).max(1),
            image::imageops::FilterType::Triangle,
        );
        flux_meta::art::encode::blurhash_of(&small).ok()
    };
    let mut out = Vec::new();
    let ext = if kind == ArtKind::Logo {
        img.to_rgba8()
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        "png"
    } else {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
            .encode_image(&rgb)
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        "jpg"
    };
    Ok(Prepared {
        bytes: out,
        ext,
        width,
        height,
        blurhash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_machine_and_link_local_are_refused() {
        for bad in [
            "127.0.0.1",
            "::1",
            "169.254.169.254",
            "fe80::1",
            "0.0.0.0",
            "::ffff:127.0.0.1",
            "224.0.0.1",
        ] {
            assert!(!allowed(bad.parse().unwrap()), "{bad}");
        }
        for ok in [
            "93.184.216.34",
            "192.168.1.20",
            "10.0.0.5",
            "100.64.1.2",
            "2606:4700::1111",
        ] {
            assert!(allowed(ok.parse().unwrap()), "{ok}");
        }
    }

    #[tokio::test]
    async fn refuses_other_schemes_and_this_machine() {
        assert_eq!(
            fetch("file:///etc/passwd").await.unwrap_err().code,
            ErrorCode::Invalid
        );
        assert_eq!(
            fetch("http://127.0.0.1:9/x.jpg").await.unwrap_err().code,
            ErrorCode::Forbidden
        );
        assert_eq!(
            fetch("http://localhost:9/x.jpg").await.unwrap_err().code,
            ErrorCode::Forbidden
        );
        assert_eq!(fetch("not a url").await.unwrap_err().code, ErrorCode::Invalid);
    }

    #[test]
    fn images_are_re_encoded_and_bombs_refused() {
        let img = image::RgbImage::from_fn(200, 300, |x, y| image::Rgb([x as u8, y as u8, 90]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let p = prepare(&png, ArtKind::Poster).unwrap();
        assert_eq!((p.ext, p.width, p.height), ("jpg", 200, 300));
        assert!(p.bytes.starts_with(&[0xFF, 0xD8]));
        assert!(p.blurhash.is_some());
        let logo = prepare(&png, ArtKind::Logo).unwrap();
        assert_eq!(logo.ext, "png");
        assert_eq!(
            prepare(b"GIF89a....", ArtKind::Poster).unwrap_err().code,
            ErrorCode::Invalid
        );
        assert_eq!(
            prepare(b"not an image", ArtKind::Poster).unwrap_err().code,
            ErrorCode::Invalid
        );
    }
}
