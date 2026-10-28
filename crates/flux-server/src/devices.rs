//! Owner devices: browsers made owners with a one-time owner link, where flux
//! can't tell who is calling by itself (docs/INSTALL.md § The owner link).
//!
//! flux knows its owner by where a call comes from: this machine
//! (`localhost_role`) or a tailnet login. A browser elsewhere on the LAN, or
//! on a Docker host (which reaches the container through Docker's network),
//! is anonymous, so it couldn't set flux up. An owner link fixes that:
//!
//! - `flux-server owner-link` or the Libraries screen makes one, and flux
//!   prints one in its log while it has no libraries, where this machine's
//!   callers are its owner anyway (whoever reads the log could open it).
//!   Each works once, within a day, and makes devices of its maker's role:
//!   an admin's link makes admins.
//! - Opening it (`/own#<token>`: the token stays in the fragment, out of
//!   request logs and `Referer` headers) makes that browser an owner device
//!   with a random key. The page keeps the key in its own storage and sends
//!   it as `X-Flux-Device` with every API call. Storage belongs to flux's
//!   address and port; a cookie would go to every server on the same host.
//!   The data dir keeps only the key's SHA-256 (`devices.json`), with a name,
//!   who added the device and when it was last seen. A device unseen for 180
//!   days stops working.
//! - A request with no identity of its own (the LAN, Docker's network, an
//!   anonymous localhost) that carries a device's key is that device's. A
//!   local or tailnet identity always wins. The Libraries screen lists owner
//!   devices and removes them.
//! - `flux-server owner-link` proves itself with the data dir's `owner.key`
//!   (`X-Flux-Owner-Key`), a new one at each start that only the account
//!   flux runs as can read: whoever can read the data dir owns flux anyway.
//!
//! Over plain HTTP, anyone who can watch the network's traffic could copy a
//! device's key; the tailnet encrypts it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::Engine;
use flux_core::api::{DeviceInfo, Identity, LinkPlace, PlaceKind, Role};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The header an owner device sends its key in.
pub const HEADER: &str = "x-flux-device";
/// The header `flux-server owner-link` sends the data dir's owner key in.
pub const KEY_HEADER: &str = "x-flux-owner-key";
/// In the data dir: the owner devices.
pub const FILE: &str = "devices.json";
/// In the data dir: this run's owner key.
pub const KEY_FILE: &str = "owner.key";
/// An owner link works this long, once.
pub const LINK_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Links waiting to be used, at most (older ones go).
const MAX_LINKS: usize = 16;
/// Owner devices, at most.
const MAX_DEVICES: usize = 64;
/// "Last seen" is written to disk at most this often per device.
const SEEN_EVERY_MS: i64 = 10 * 60 * 1000;
/// A device unseen this long stops working, and leaves the list.
const UNSEEN_LIMIT_MS: i64 = 180 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Device {
    id: String,
    name: String,
    role: Role,
    /// SHA-256 of the device's key, hex.
    key_sha256: String,
    /// Who made the link it came from.
    added_by: String,
    created_at: i64,
    last_seen_at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    devices: Vec<Device>,
}

struct Link {
    token_sha256: [u8; 32],
    /// When it stops working, on the monotonic clock (a wall clock that jumps
    /// at boot, on a machine without one, would end it early).
    until: Instant,
    /// What the devices it makes are: its maker's role.
    role: Role,
    by: String,
}

#[derive(Default)]
struct Inner {
    devices: Vec<Device>,
    links: Vec<Link>,
    /// When each device's last-seen was last written.
    written_seen: HashMap<String, i64>,
}

pub struct Devices {
    path: PathBuf,
    inner: Mutex<Inner>,
    /// Saves go one at a time, each with a snapshot taken while it holds
    /// this, so an older list never lands after a newer one.
    writing: Mutex<()>,
    /// SHA-256 of this run's owner key, when `owner.key` could be written.
    owner_key: Option<[u8; 32]>,
}

/// Why a link didn't make a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimError {
    /// Unknown, used or expired: all the same to the caller.
    BadLink,
    /// Too many owner devices; remove one first.
    Full,
    /// The device couldn't be saved, so it wasn't made; the link still works.
    Unsaved(String),
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    // The OS's generator; failing it is failing the machine.
    getrandom::fill(&mut bytes).expect("the OS random number generator");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random_id() -> String {
    let mut bytes = [0u8; 6];
    getrandom::fill(&mut bytes).expect("the OS random number generator");
    hex(&bytes)
}

fn sha256(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unseen_too_long(d: &Device, now: i64) -> bool {
    now - d.last_seen_at > UNSEEN_LIMIT_MS
}

impl Devices {
    /// The owner devices saved in `data_dir`, and a new owner key there.
    pub fn load(data_dir: &Path) -> Devices {
        let path = data_dir.join(FILE);
        let devices = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Saved>(&bytes) {
                Ok(saved) => saved.devices,
                Err(e) => {
                    // Kept aside rather than overwritten by the next save.
                    let aside = path.with_extension("json.bad");
                    let moved = std::fs::rename(&path, &aside);
                    tracing::warn!(
                        error = %e,
                        kept_as = %aside.display(),
                        moved = moved.is_ok(),
                        "{} couldn't be read: flux starts with no owner devices",
                        path.display()
                    );
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                tracing::warn!(error = %e, "couldn't read {}: flux starts with no owner devices", path.display());
                Vec::new()
            }
        };
        Devices {
            path,
            inner: Mutex::new(Inner {
                devices,
                ..Inner::default()
            }),
            writing: Mutex::new(()),
            owner_key: write_owner_key(data_dir),
        }
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Writes the devices to `devices.json` (atomically: a whole new file,
    /// synced, then renamed over the old one).
    pub fn save(&self) -> std::io::Result<()> {
        let _one = self.writing.lock().unwrap_or_else(|p| p.into_inner());
        let saved = {
            let mut inner = self.inner();
            let now = now_ms();
            inner.devices.retain(|d| !unseen_too_long(d, now));
            Saved {
                devices: inner.devices.clone(),
            }
        };
        let bytes = serde_json::to_vec_pretty(&saved).map_err(std::io::Error::other)?;
        let tmp = self.path.with_extension("json.part");
        write_private(&tmp, &bytes)?;
        std::fs::rename(&tmp, &self.path)
    }

    /// A new owner link's token (the part after `/own#`), and when it stops
    /// working (Unix epoch ms). The devices it makes get `role`, its maker's
    /// (`by`, shown as who added them).
    pub fn new_link(&self, role: Role, by: &str) -> (String, i64) {
        let token = random_token();
        let now = Instant::now();
        let expires_at = now_ms() + LINK_TTL.as_millis() as i64;
        let mut inner = self.inner();
        inner.links.retain(|l| l.until > now);
        if inner.links.len() >= MAX_LINKS {
            inner.links.remove(0);
        }
        inner.links.push(Link {
            token_sha256: sha256(&token),
            until: now + LINK_TTL,
            role,
            by: by.to_string(),
        });
        (token, expires_at)
    }

    /// Uses a link: a new owner device called `name`, saved, and the key it
    /// sends from now on. A device that couldn't be saved isn't made (it
    /// would be gone after a restart), and its link keeps working.
    pub fn claim(&self, token: &str, name: &str) -> Result<(DeviceInfo, String), ClaimError> {
        let key = random_token();
        let (info, link) = {
            let mut inner = self.inner();
            let now = Instant::now();
            let want = sha256(token.trim());
            let at = inner
                .links
                .iter()
                .position(|l| l.until > now && l.token_sha256 == want)
                .ok_or(ClaimError::BadLink)?;
            let wall = now_ms();
            inner.devices.retain(|d| !unseen_too_long(d, wall));
            if inner.devices.len() >= MAX_DEVICES {
                return Err(ClaimError::Full);
            }
            let link = inner.links.remove(at);
            let device = Device {
                id: random_id(),
                name: clean_name(name),
                role: link.role,
                key_sha256: hex(&sha256(&key)),
                added_by: link.by.clone(),
                created_at: wall,
                last_seen_at: wall,
            };
            let info = info_of(&device, Some(&device.id));
            inner.devices.push(device);
            (info, link)
        };
        if let Err(e) = self.save() {
            let mut inner = self.inner();
            inner.devices.retain(|d| d.id != info.id);
            inner.links.push(link);
            tracing::warn!(error = %e, "couldn't save a new owner device to {}", self.path.display());
            return Err(ClaimError::Unsaved(e.to_string()));
        }
        tracing::info!(device = %info.name, id = %info.id, role = ?info.role, added_by = %info.added_by, "a browser became an owner device");
        Ok((info, key))
    }

    /// The identity a device's key stands for, the device's id, and whether
    /// its last-seen is due to be saved ([`Devices::save`], off the request's
    /// thread).
    pub fn identify(&self, key: &str) -> Option<(Identity, String, bool)> {
        let want = hex(&sha256(key));
        let mut inner = self.inner();
        let now = now_ms();
        let d = inner.devices.iter_mut().find(|d| d.key_sha256 == want)?;
        if unseen_too_long(d, now) {
            return None;
        }
        d.last_seen_at = now;
        let (id, name, role) = (d.id.clone(), d.name.clone(), d.role);
        let written = inner.written_seen.get(&id).copied().unwrap_or(0);
        let due = now - written >= SEEN_EVERY_MS;
        if due {
            inner.written_seen.insert(id.clone(), now);
        }
        Some((
            Identity {
                login_name: format!("device:{id}"),
                display_name: name,
                profile_pic_url: None,
                node_name: None,
                role,
                tags: Vec::new(),
            },
            id,
            due,
        ))
    }

    /// Every owner device that still works, the caller's own marked.
    pub fn list(&self, current: Option<&str>) -> Vec<DeviceInfo> {
        let now = now_ms();
        self.inner()
            .devices
            .iter()
            .filter(|d| !unseen_too_long(d, now))
            .map(|d| info_of(d, current))
            .collect()
    }

    /// Removes an owner device: its key stops working at once. An error
    /// means it couldn't be saved: it is gone until flux restarts, then back.
    pub fn remove(&self, id: &str) -> std::io::Result<bool> {
        let removed = {
            let mut inner = self.inner();
            let before = inner.devices.len();
            inner.devices.retain(|d| d.id != id);
            inner.written_seen.remove(id);
            inner.devices.len() != before
        };
        if removed {
            self.save()?;
            tracing::info!(id, "owner device removed");
        }
        Ok(removed)
    }

    /// Whether `key` is this run's owner key (`owner.key`).
    pub fn is_owner_key(&self, key: &str) -> bool {
        self.owner_key.is_some_and(|k| k == sha256(key.trim()))
    }
}

fn info_of(d: &Device, current: Option<&str>) -> DeviceInfo {
    DeviceInfo {
        id: d.id.clone(),
        name: d.name.clone(),
        role: d.role,
        added_by: d.added_by.clone(),
        created_at: d.created_at,
        last_seen_at: d.last_seen_at,
        this_device: current == Some(d.id.as_str()),
    }
}

/// A device name as people see it: printable, one line, short, and no
/// characters that reorder or hide text (right-to-left overrides and the
/// like, which could dress one name up as another).
fn clean_name(name: &str) -> String {
    let invisible = |c: char| {
        c.is_control()
            || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
    };
    let s: String = name
        .chars()
        .filter(|c| !invisible(*c))
        .take(60)
        .collect::<String>()
        .trim()
        .to_string();
    if s.is_empty() {
        "A browser".into()
    } else {
        s
    }
}

/// Writes a file only the account flux runs as can read (and SYSTEM and
/// Administrators on Windows), synced to disk: it holds keys or their hashes.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(path)?;
    // Before anything is in it.
    #[cfg(windows)]
    crate::win32::restrict_to_owner(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// This run's owner key: a new random one in `owner.key`, for
/// `flux-server owner-link` to prove itself with. `None` if it couldn't be
/// written (the command then asks as this machine's caller).
fn write_owner_key(data_dir: &Path) -> Option<[u8; 32]> {
    let key = random_token();
    let path = data_dir.join(KEY_FILE);
    let tmp = path.with_extension("key.part");
    match write_private(&tmp, key.as_bytes()).and_then(|()| std::fs::rename(&tmp, &path)) {
        Ok(()) => Some(sha256(&key)),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't write {}", path.display());
            None
        }
    }
}

/// A short header's value, if the request has it.
fn header_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && v.len() <= 128)
}

/// The owner device a request comes from (an extension the [`attach`]
/// layer adds), so lists can mark "this device".
#[derive(Debug, Clone)]
pub struct DeviceId(pub String);

/// Who `flux-server owner-link` is, with the owner key.
fn owner_key_identity() -> Identity {
    Identity {
        login_name: "owner-key".into(),
        display_name: "flux-server owner-link".into(),
        profile_pic_url: None,
        node_name: None,
        role: Role::Owner,
        tags: Vec::new(),
    }
}

/// A layer of [`crate::api::router`], inside the cross-site guard: a request
/// with the data dir's owner key is the Owner's; one without an identity of
/// its own that carries an owner device's key becomes that device's.
pub async fn attach(
    axum::extract::State(st): axum::extract::State<std::sync::Arc<crate::state::AppState>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if header_value(req.headers(), KEY_HEADER).is_some_and(|k| st.devices.is_owner_key(&k)) {
        req.extensions_mut().insert(Some(owner_key_identity()));
        return next.run(req).await;
    }
    let anonymous = req
        .extensions()
        .get::<Option<Identity>>()
        .cloned()
        .flatten()
        .is_none();
    if anonymous {
        if let Some((who, id, due)) =
            header_value(req.headers(), HEADER).and_then(|k| st.devices.identify(&k))
        {
            req.extensions_mut().insert(Some(who));
            req.extensions_mut().insert(DeviceId(id));
            if due {
                // A file write: not on this request's thread, nor under the lock.
                let st = st.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = st.devices.save() {
                        tracing::warn!(error = %e, "couldn't save when owner devices were last seen");
                    }
                });
            }
        }
    }
    next.run(req).await
}

/// What a [`LinkPlace`] means, for people.
pub fn place_says(place: PlaceKind) -> &'static str {
    match place {
        PlaceKind::Network => "on another device on this network",
        PlaceKind::ThisMachine => "on this machine",
        PlaceKind::DockerHost => {
            "on the Docker host (from another machine, with the host's address in place of localhost; \
             and the port you published, if it isn't this one)"
        }
    }
}

/// What this machine's browsers are to flux (`--localhost-role`), as far as
/// owner links go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Local {
    /// The owner or an admin: they set flux up themselves.
    SetsUp,
    /// Anonymous (`none`), so a link works here too.
    Anonymous,
    /// A lesser role, which a link can't change.
    Lesser,
}

impl Local {
    pub fn of(role: Option<Role>) -> Local {
        match role {
            Some(Role::Owner | Role::Admin) => Local::SetsUp,
            None => Local::Anonymous,
            Some(_) => Local::Lesser,
        }
    }
}

/// Where an owner link (`path`, `/own#…`) works: where a browser flux can't
/// place by itself reaches it. `lan` is this machine's addresses on the
/// network (`api::lan_urls`); in Docker those are the container's own,
/// which nothing outside it can use, so the link goes through the host's
/// `localhost` instead. `local` is the local listener's address.
pub fn link_places(local: &str, lan: &[String], docker: bool, who: Local, path: &str) -> Vec<LinkPlace> {
    let place = |url: String, place| LinkPlace { url, place };
    if docker {
        return vec![place(format!("{local}{path}"), PlaceKind::DockerHost)];
    }
    let here = (who == Local::Anonymous).then(|| place(format!("{local}{path}"), PlaceKind::ThisMachine));
    lan.iter()
        .map(|u| place(format!("{u}{path}"), PlaceKind::Network))
        .chain(here)
        .collect()
}

/// The log's line at startup while flux has no libraries: where to set it
/// up, with an owner link (`mint` makes it) where some browser needs one
/// ([`link_places`]), and only where this machine's callers are flux's
/// owner: whoever can read the log could open the link. Elsewhere it says to
/// make one with `flux-server owner-link`. `local` is the local listener's
/// address.
pub fn setup_line(
    local: &str,
    lan: &[String],
    docker: bool,
    who: Local,
    mint: impl FnOnce() -> String,
) -> String {
    let exec = if docker { "docker exec <container> " } else { "" };
    if who != Local::SetsUp {
        return format!(
            "flux has no libraries yet. This machine's browsers aren't its owner (--localhost-role), so the \
             link that sets it up isn't in this log: make one with `{exec}flux-server owner-link`, as the \
             account flux runs as"
        );
    }
    let places = link_places(local, lan, docker, who, "");
    if places.is_empty() {
        return format!("flux has no libraries yet: set it up at {local}/setup");
    }
    let path = format!("/own#{}", mint());
    let links = link_places(local, lan, docker, who, &path)
        .into_iter()
        .map(|p| format!("{} {}", p.url, place_says(p.place)))
        .collect::<Vec<_>>()
        .join(", or ");
    let start = if docker {
        format!("flux has no libraries yet. To set it up, open {links}")
    } else {
        format!("flux has no libraries yet: set it up at {local}/setup, or open {links}")
    };
    format!(
        "{start}: that makes the browser an owner (the link works once, within a day; `{exec}flux-server \
         owner-link` makes another)"
    )
}

/// `flux-server owner-link`: asks the running flux for a new owner link,
/// with the data dir's owner key (or, when it can't read that, as one of
/// this machine's callers), and prints where to open it.
pub fn cli(cfg: &crate::config::Config) -> std::process::ExitCode {
    use std::process::ExitCode;
    let key_path = cfg.data_dir.join(KEY_FILE);
    let key = std::fs::read_to_string(&key_path).ok();
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("flux-server: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async {
        // Straight to this machine: never through a proxy from the environment.
        let client = match reqwest::Client::builder().no_proxy().build() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("flux-server: {e}");
                return ExitCode::FAILURE;
            }
        };
        let from_file = std::fs::read(cfg.data_dir.join(crate::server::SERVER_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v["url"].as_str().map(str::to_string));
        let urls = from_file
            .into_iter()
            .chain([format!("http://127.0.0.1:{}", cfg.local_addr.port())]);
        for url in urls {
            let mut ask = client
                .post(format!("{url}/api/v1/admin/owner-links"))
                .header(flux_core::api::REQUEST_HEADER, "1")
                .timeout(Duration::from_secs(5));
            if let Some(key) = &key {
                ask = ask.header(KEY_HEADER, key.trim());
            }
            let Ok(res) = ask.send().await else { continue };
            let status = res.status();
            if status == axum::http::StatusCode::FORBIDDEN && key.is_none() {
                eprintln!(
                    "The flux at {url} doesn't take this machine's callers for its owner (--localhost-role), and \
                     {} can't be read: run this as the account flux runs as (or with sudo), with the server's \
                     --config or --data-dir.",
                    key_path.display()
                );
                return ExitCode::FAILURE;
            }
            if status == axum::http::StatusCode::NOT_FOUND {
                eprintln!(
                    "The flux at {url} is older than this command (no owner links): restart it as this version."
                );
                return ExitCode::FAILURE;
            }
            if !status.is_success() {
                let text = res.text().await.unwrap_or_default();
                eprintln!("The flux at {url} refused ({status}): {}", text.trim());
                return ExitCode::FAILURE;
            }
            let Ok(link) = res.json::<flux_core::api::OwnerLink>().await else {
                continue;
            };
            let width = link.places.iter().map(|p| p.url.len()).max().unwrap_or(0);
            println!("Open this in the browser that should be an owner. It works once, within a day.\n");
            for p in &link.places {
                println!("  {:width$}  {}", p.url, place_says(p.place));
            }
            if link.places.is_empty() {
                println!("  {}  after the address that browser reaches flux at\n", link.path);
                println!(
                    "flux listens only on this machine, whose browsers are its owner already: other devices \
                     reach it with --lan or remote access."
                );
            } else {
                println!(
                    "\nThrough another address (a reverse proxy's, say), put {} after it.",
                    link.path
                );
            }
            return ExitCode::SUCCESS;
        }
        eprintln!("flux isn't running here (start it, then run this again).");
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places(v: &[LinkPlace]) -> Vec<(PlaceKind, &str)> {
        v.iter().map(|p| (p.place, p.url.as_str())).collect()
    }

    #[test]
    fn a_link_makes_one_owner_device_once() {
        let d = tempfile::tempdir().unwrap();
        let devices = Devices::load(d.path());
        let (token, expires) = devices.new_link(Role::Owner, "this computer");
        assert!(expires > now_ms());
        assert_eq!(token.len(), 43, "256 random bits");
        let (info, key) = devices.claim(&token, "Chrome on the living-room Mac").unwrap();
        assert_eq!(
            (info.role, info.added_by.as_str()),
            (Role::Owner, "this computer")
        );
        assert!(info.this_device);
        assert_eq!(
            devices.claim(&token, "again").unwrap_err(),
            ClaimError::BadLink,
            "once"
        );
        assert_eq!(devices.claim("guess", "x").unwrap_err(), ClaimError::BadLink);
        let (who, id, _) = devices.identify(&key).expect("the key works");
        assert_eq!(
            (who.role, who.display_name.as_str(), id.as_str()),
            (Role::Owner, "Chrome on the living-room Mac", info.id.as_str())
        );
        assert!(devices.identify("not-it").is_none());
    }

    /// A tailnet admin's link made owners, which outlived the admin.
    #[test]
    fn a_link_makes_devices_of_its_makers_role() {
        let d = tempfile::tempdir().unwrap();
        let devices = Devices::load(d.path());
        let (token, _) = devices.new_link(Role::Admin, "ana@example.com");
        let (info, key) = devices.claim(&token, "Tablet").unwrap();
        assert_eq!(
            (info.role, info.added_by.as_str()),
            (Role::Admin, "ana@example.com")
        );
        assert_eq!(devices.identify(&key).map(|(w, ..)| w.role), Some(Role::Admin));
    }

    #[test]
    fn devices_last_across_restarts_and_only_their_hashes_are_kept() {
        let d = tempfile::tempdir().unwrap();
        let (token, _) = Devices::load(d.path()).new_link(Role::Owner, "x");
        // A link doesn't outlive the process that made it.
        assert_eq!(
            Devices::load(d.path()).claim(&token, "x").unwrap_err(),
            ClaimError::BadLink
        );
        let devices = Devices::load(d.path());
        let (token, _) = devices.new_link(Role::Owner, "x");
        let (_, key) = devices.claim(&token, "Phone").unwrap();
        let file = std::fs::read_to_string(d.path().join(FILE)).unwrap();
        assert!(!file.contains(&key), "the key itself is never stored");
        let again = Devices::load(d.path());
        assert!(again.identify(&key).is_some());
        #[cfg(unix)]
        for f in [FILE, KEY_FILE] {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.path().join(f)).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "{f} readable by its owner only");
        }
    }

    #[test]
    fn a_removed_device_is_anonymous_again() {
        let d = tempfile::tempdir().unwrap();
        let devices = Devices::load(d.path());
        let (token, _) = devices.new_link(Role::Owner, "x");
        let (info, key) = devices.claim(&token, "\u{1b}[31mTab\u{202e}let\n").unwrap();
        assert_eq!(info.name, "[31mTablet", "control and reordering characters go");
        assert_eq!(devices.list(None).len(), 1);
        assert!(devices.remove(&info.id).unwrap());
        assert!(!devices.remove(&info.id).unwrap());
        assert!(devices.identify(&key).is_none());
        assert!(Devices::load(d.path()).list(None).is_empty());
    }

    /// Removing a device reported success when the file couldn't be written,
    /// and the device came back at the next start.
    #[test]
    #[cfg(unix)]
    fn a_change_that_cant_be_saved_says_so() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let devices = Devices::load(d.path());
        let (token, _) = devices.new_link(Role::Owner, "x");
        let (info, _) = devices.claim(&token, "Phone").unwrap();
        let (token, _) = devices.new_link(Role::Owner, "x");
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::write(d.path().join("probe"), b"").is_ok() {
            // Root writes anyway: nothing to see here.
            std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let unsaved = devices.claim(&token, "Laptop");
        let removed = devices.remove(&info.id);
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(unsaved, Err(ClaimError::Unsaved(_))), "{unsaved:?}");
        assert!(removed.is_err(), "the removal wasn't saved");
        // The claim was undone and its link kept for another try.
        assert!(devices.claim(&token, "Laptop").is_ok());
    }

    #[test]
    fn a_damaged_file_is_kept_aside_not_overwritten() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(FILE), b"{ not json").unwrap();
        let devices = Devices::load(d.path());
        assert!(devices.list(None).is_empty());
        assert_eq!(
            std::fs::read(d.path().join("devices.json.bad")).unwrap(),
            b"{ not json"
        );
    }

    #[test]
    fn links_and_devices_are_bounded() {
        let d = tempfile::tempdir().unwrap();
        let devices = Devices::load(d.path());
        let first = devices.new_link(Role::Owner, "x").0;
        for _ in 0..MAX_LINKS {
            devices.new_link(Role::Owner, "x");
        }
        assert_eq!(
            devices.claim(&first, "x").unwrap_err(),
            ClaimError::BadLink,
            "the oldest went"
        );
        for i in 0..MAX_DEVICES {
            let (t, _) = devices.new_link(Role::Owner, "x");
            devices.claim(&t, &format!("Device {i}")).unwrap();
        }
        let (t, _) = devices.new_link(Role::Owner, "x");
        assert_eq!(devices.claim(&t, "one too many").unwrap_err(), ClaimError::Full);
    }

    #[test]
    fn a_device_unseen_for_half_a_year_stops_working() {
        let d = tempfile::tempdir().unwrap();
        let devices = Devices::load(d.path());
        let (t, _) = devices.new_link(Role::Owner, "x");
        let (info, key) = devices.claim(&t, "Old laptop").unwrap();
        devices.inner().devices[0].last_seen_at = now_ms() - UNSEEN_LIMIT_MS - 1;
        assert!(devices.identify(&key).is_none());
        assert!(devices.list(None).iter().all(|x| x.id != info.id));
    }

    #[test]
    fn the_owner_key_is_new_each_start_and_only_its_hash_is_kept() {
        let d = tempfile::tempdir().unwrap();
        let first = Devices::load(d.path());
        let key = std::fs::read_to_string(d.path().join(KEY_FILE)).unwrap();
        assert!(first.is_owner_key(&key));
        assert!(!first.is_owner_key("guess"));
        let second = Devices::load(d.path());
        assert!(!second.is_owner_key(&key), "a new key at each start");
    }

    #[test]
    fn reads_only_short_header_values() {
        let mut h = axum::http::HeaderMap::new();
        h.insert(HEADER, " abc_DEF-123 ".parse().unwrap());
        assert_eq!(header_value(&h, HEADER).as_deref(), Some("abc_DEF-123"));
        h.insert(HEADER, "x".repeat(129).parse().unwrap());
        assert_eq!(header_value(&h, HEADER), None);
        assert_eq!(header_value(&h, KEY_HEADER), None);
    }

    /// The log said "open http://localhost:8096/own#…" to a LAN device that
    /// could only reach flux on the --lan port. Links now carry addresses a
    /// browser can open; none is made where nobody could use one, and none
    /// goes in the log where reading the log doesn't mean owning flux.
    #[test]
    fn the_setup_line_links_only_where_a_browser_needs_it() {
        let local = "http://localhost:8096";
        let lan = vec!["http://10.0.0.164:8097".to_string()];
        let mint = || "TOKEN".to_string();
        let never = || -> String { panic!("no link here") };

        // 127.0.0.1 only, its browsers the owner: nothing else reaches flux.
        assert_eq!(
            setup_line(local, &[], false, Local::SetsUp, never),
            "flux has no libraries yet: set it up at http://localhost:8096/setup"
        );
        // With --lan: this machine sets up; the network gets the LAN port.
        let line = setup_line(local, &lan, false, Local::SetsUp, mint);
        assert!(
            line.contains("set it up at http://localhost:8096/setup"),
            "{line}"
        );
        assert!(
            line.contains("http://10.0.0.164:8097/own#TOKEN on another device on this network"),
            "{line}"
        );
        assert!(!line.contains("localhost:8096/own"), "{line}");
        assert!(line.contains("`flux-server owner-link` makes another"), "{line}");
        // --localhost-role none or viewer: no link in the log at all.
        for who in [Local::Anonymous, Local::Lesser] {
            let line = setup_line(local, &lan, false, who, never);
            assert!(line.contains("make one with `flux-server owner-link`"), "{line}");
        }
        // Docker: the container's own addresses are no use; the host's localhost is.
        let line = setup_line(local, &[], true, Local::SetsUp, mint);
        assert!(
            line.contains("open http://localhost:8096/own#TOKEN on the Docker host"),
            "{line}"
        );
        assert!(
            line.contains("`docker exec <container> flux-server owner-link`"),
            "{line}"
        );
    }

    #[test]
    fn link_places_list_the_network_first() {
        let local = "http://localhost:8096";
        let lan = [
            "http://10.0.0.164:8097".to_string(),
            "http://192.168.1.5:8097".to_string(),
        ];
        assert_eq!(
            places(&link_places(local, &lan, false, Local::Anonymous, "/own#T")),
            [
                (PlaceKind::Network, "http://10.0.0.164:8097/own#T"),
                (PlaceKind::Network, "http://192.168.1.5:8097/own#T"),
                (PlaceKind::ThisMachine, "http://localhost:8096/own#T"),
            ]
        );
        // This machine's browsers are placed already: the network only.
        assert_eq!(link_places(local, &lan, false, Local::SetsUp, "/own#T").len(), 2);
        assert!(link_places(local, &[], false, Local::Lesser, "/own#T").is_empty());
        assert_eq!(
            places(&link_places(local, &lan, true, Local::SetsUp, "/own#T")),
            [(PlaceKind::DockerHost, "http://localhost:8096/own#T")]
        );
    }
}
