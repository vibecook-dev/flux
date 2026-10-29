//! What GitHub says about flux's releases, and which one this server may
//! update to.
//!
//! A release is `v<version>` with one package per target,
//! `flux-<version>-<target>.tar.gz` (`.zip` for Windows), the Windows
//! installer `flux-<version>-x86_64-pc-windows-msvc.msi`, the macOS app's
//! disk image `flux-<version>-universal-apple-darwin.dmg`, and `SHA256SUMS`
//! (docs/RELEASING.md). Those names are what the updater looks for, so they
//! are part of the release contract.

use semver::Version;
use serde::Deserialize;

/// Where the releases are listed, newest first.
pub const RELEASES_URL: &str = "https://api.github.com/repos/vibecook-dev/flux/releases?per_page=20";

/// A release as the GitHub API lists it.
#[derive(Debug, Clone, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    html_url: String,
    published_at: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// A published release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// Its page, with the full notes.
    pub url: String,
    /// Unix epoch milliseconds.
    pub published_at: Option<i64>,
    pub notes: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// The published releases in a GitHub API answer: drafts and tags that
/// aren't `v<semver>` are left out.
pub fn parse(json: &[u8]) -> anyhow::Result<Vec<Release>> {
    let list: Vec<GhRelease> = serde_json::from_slice(json)?;
    Ok(list
        .into_iter()
        .filter(|r| !r.draft)
        .filter_map(|r| {
            let version = Version::parse(r.tag_name.strip_prefix('v')?).ok()?;
            Some(Release {
                version,
                url: r.html_url,
                published_at: r.published_at.as_deref().and_then(rfc3339_ms),
                notes: r.body.unwrap_or_default(),
                assets: r
                    .assets
                    .into_iter()
                    .map(|a| Asset {
                        name: a.name,
                        url: a.browser_download_url,
                        size: a.size,
                    })
                    .collect(),
            })
        })
        .collect())
}

/// The newest release newer than `current` that it may move to: a stable
/// version follows stable releases only, a pre-release (`0.1.0-alpha.4`)
/// follows pre-releases too.
pub fn newest_for<'a>(releases: &'a [Release], current: &Version) -> Option<&'a Release> {
    releases
        .iter()
        .filter(|r| !current.pre.is_empty() || r.version.pre.is_empty())
        .filter(|r| r.version > *current)
        .max_by(|a, b| a.version.cmp(&b.version))
}

/// The package of `version` for `target`.
pub fn package_name(version: &Version, target: &str) -> String {
    let ext = if target.contains("-windows-") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("flux-{version}-{target}.{ext}")
}

/// The Windows installer of `version` for `target`
/// (`flux-<version>-x86_64-pc-windows-msvc.msi`).
pub fn installer_name(version: &Version, target: &str) -> String {
    format!("flux-{version}-{target}.msi")
}

/// The macOS app's disk image of `version`, for both Mac architectures
/// (`flux-<version>-universal-apple-darwin.dmg`).
pub fn app_name(version: &Version) -> String {
    format!("flux-{version}-{APP_TARGET}.dmg")
}

/// The target the app's disk image is named for: one bundle for Apple
/// Silicon and Intel Macs.
pub const APP_TARGET: &str = "universal-apple-darwin";

/// The checksum asset's name.
pub const SUMS: &str = "SHA256SUMS";

/// The SHA-256 that `SHA256SUMS` (`<hex>  <file>` lines, as `sha256sum`
/// writes them) gives `file`.
pub fn sum_of(sums: &str, file: &str) -> Option<[u8; 32]> {
    sums.lines().find_map(|line| {
        let (hex, name) = line.trim().split_once(char::is_whitespace)?;
        // `*name` marks binary mode in sha256sum's output.
        let name = name.trim().trim_start_matches('*');
        (name == file).then(|| unhex(hex)).flatten()
    })
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// The first `max` entries of a release's notes, as plain text: Release
/// Please writes `* **web:** what changed ([#42](…)) ([52e0003](…))`.
pub fn highlights(notes: &str, max: usize) -> Vec<String> {
    notes
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix("* ")
                .or_else(|| l.trim().strip_prefix("- "))
        })
        .map(plain)
        .filter(|s| !s.is_empty())
        .take(max)
        .collect()
}

/// A note line without its Markdown: links keep their text, references in
/// parentheses at the end (`([#42](…))`) go, and so do emphasis marks.
fn plain(line: &str) -> String {
    let mut s = line.trim().to_string();
    // Trailing "([#42](url))" and "([52e0003](url))".
    loop {
        let t = s.trim_end();
        let Some(open) = t.rfind(" ([") else { break };
        if t.ends_with("))") && t[open..].contains("](") {
            s = t[..open].to_string();
        } else {
            break;
        }
    }
    // [text](url) → text
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(i) = rest.find('[') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after
            .find("](")
            .and_then(|j| after[j + 2..].find(')').map(|k| (j, j + 2 + k)))
        {
            Some((j, end)) => {
                out.push_str(&after[..j]);
                rest = &after[end + 1..];
            }
            None => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.replace("**", "").replace('`', "").trim().to_string()
}

/// `2026-10-27T00:03:04Z` (GitHub's timestamps) as Unix epoch milliseconds.
pub fn rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Only UTC ("Z", after optional fractions): what the API returns.
    let tail = s[19..].trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if tail != "Z" {
        return None;
    }
    // Days from the civil date (H. Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + hh) * 60 + mm) * 60_000 + ss * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn release(version: &str) -> Release {
        Release {
            version: v(version),
            url: format!("https://github.com/vibecook-dev/flux/releases/tag/v{version}"),
            published_at: None,
            notes: String::new(),
            assets: vec![],
        }
    }

    const API: &str = r###"[
      {"tag_name":"v0.1.0-alpha.6","draft":true,"prerelease":true,"html_url":"https://x/6","published_at":null,"body":"","assets":[]},
      {"tag_name":"v0.1.0-alpha.5","draft":false,"prerelease":true,"html_url":"https://x/5","published_at":"2026-10-28T09:30:00Z",
       "body":"## [0.1.0-alpha.5](https://github.com/vibecook-dev/flux/compare/v0.1.0-alpha.4...v0.1.0-alpha.5) (2026-10-28)\n\n\n### Features\n\n* **web:** flux installs as an app and knows when it was updated ([#43](https://github.com/vibecook-dev/flux/issues/43)) ([9561e52](https://github.com/vibecook-dev/flux/commit/9561e52))\n",
       "assets":[{"name":"SHA256SUMS","browser_download_url":"https://x/SHA256SUMS","size":676},
                 {"name":"flux-0.1.0-alpha.5-aarch64-apple-darwin.tar.gz","browser_download_url":"https://x/mac.tgz","size":25185736}]},
      {"tag_name":"nightly","draft":false,"html_url":"https://x/n","assets":[]},
      {"tag_name":"v0.1.0-alpha.4","draft":false,"prerelease":true,"html_url":"https://x/4","published_at":"2026-10-27T00:03:04Z","body":null,"assets":[]}
    ]"###;

    #[test]
    fn reads_published_releases_from_the_api() {
        let list = parse(API.as_bytes()).unwrap();
        let versions: Vec<String> = list.iter().map(|r| r.version.to_string()).collect();
        assert_eq!(
            versions,
            ["0.1.0-alpha.5", "0.1.0-alpha.4"],
            "no draft, no tag that isn't a version"
        );
        let r = &list[0];
        assert_eq!(r.url, "https://x/5");
        assert_eq!(r.published_at, Some(1_793_179_800_000));
        let pkg = r.asset("flux-0.1.0-alpha.5-aarch64-apple-darwin.tar.gz").unwrap();
        assert_eq!((pkg.url.as_str(), pkg.size), ("https://x/mac.tgz", 25_185_736));
        assert_eq!(
            highlights(&r.notes, 6),
            ["web: flux installs as an app and knows when it was updated"]
        );
        assert!(parse(b"{\"message\":\"API rate limit exceeded\"}").is_err());
    }

    #[test]
    fn a_pre_release_follows_pre_releases_and_a_stable_one_stable_releases() {
        let all = [
            release("0.1.0-alpha.4"),
            release("0.1.0-alpha.5"),
            release("0.1.0-beta.1"),
            release("0.1.0"),
            release("0.2.0-alpha.1"),
        ];
        let pick = |current: &str| newest_for(&all, &v(current)).map(|r| r.version.to_string());
        assert_eq!(pick("0.1.0-alpha.4").as_deref(), Some("0.2.0-alpha.1"));
        assert_eq!(
            pick("0.1.0").as_deref(),
            None,
            "0.2.0-alpha.1 isn't for a stable install"
        );
        assert_eq!(pick("0.0.9").as_deref(), Some("0.1.0"));
        assert_eq!(pick("0.2.0-alpha.1").as_deref(), None, "nothing newer");
        // Stable beats its own pre-releases.
        let some = [release("0.1.0-alpha.9"), release("0.1.0")];
        assert_eq!(
            newest_for(&some, &v("0.1.0-alpha.4"))
                .map(|r| r.version.to_string())
                .as_deref(),
            Some("0.1.0")
        );
        assert_eq!(newest_for(&[], &v("0.1.0")), None);
    }

    #[test]
    fn names_the_package_of_each_target() {
        let ver = v("0.1.0-alpha.5");
        assert_eq!(
            package_name(&ver, "aarch64-apple-darwin"),
            "flux-0.1.0-alpha.5-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            package_name(&ver, "x86_64-unknown-linux-gnu"),
            "flux-0.1.0-alpha.5-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            package_name(&ver, "x86_64-pc-windows-msvc"),
            "flux-0.1.0-alpha.5-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(
            installer_name(&ver, "x86_64-pc-windows-msvc"),
            "flux-0.1.0-alpha.5-x86_64-pc-windows-msvc.msi"
        );
        assert_eq!(app_name(&ver), "flux-0.1.0-alpha.5-universal-apple-darwin.dmg");
    }

    #[test]
    fn finds_a_files_sum_in_sha256sums() {
        let sums = "\
6d0f5c0fd4b5e8fd9a6b7c1d2e3f405162738495a6b7c8d9e0f1a2b3c4d5e6f7  flux-0.1.0-alpha.5-aarch64-apple-darwin.tar.gz
00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff *flux-0.1.0-alpha.5-x86_64-pc-windows-msvc.zip
nothex  bad.tar.gz
";
        let mac = sum_of(sums, "flux-0.1.0-alpha.5-aarch64-apple-darwin.tar.gz").unwrap();
        assert_eq!(mac[0], 0x6d);
        assert_eq!(mac[31], 0xf7);
        assert_eq!(
            sum_of(sums, "flux-0.1.0-alpha.5-x86_64-pc-windows-msvc.zip").unwrap()[1],
            0x11
        );
        assert_eq!(sum_of(sums, "bad.tar.gz"), None);
        assert_eq!(sum_of(sums, "flux-0.1.0-alpha.5"), None, "whole names only");
    }

    #[test]
    fn notes_become_plain_lines() {
        let notes = "### Features\n\n* **web:** a lens bar that comes back ([#42](https://x/42)) ([52e0003](https://x/c))\n* plain entry\n\n### Bug Fixes\n\n* **scan:** see [the docs](https://x/d) for `--no-watch`\n- dash entry\n";
        assert_eq!(
            highlights(notes, 10),
            [
                "web: a lens bar that comes back",
                "plain entry",
                "scan: see the docs for --no-watch",
                "dash entry"
            ]
        );
        assert_eq!(highlights(notes, 2).len(), 2);
        assert_eq!(plain("a [broken link"), "a [broken link");
    }

    #[test]
    fn reads_githubs_timestamps() {
        assert_eq!(rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_ms("2026-10-27T00:03:04Z"), Some(1_793_059_384_000));
        assert_eq!(rfc3339_ms("2000-02-29T12:00:00.5Z"), Some(951_825_600_000));
        assert_eq!(rfc3339_ms("2026-10-27T00:03:04+02:00"), None);
        assert_eq!(rfc3339_ms("2026-13-01T00:00:00Z"), None);
        assert_eq!(rfc3339_ms("soon"), None);
    }
}
