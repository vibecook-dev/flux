//! Owner links and owner devices through the router: a LAN caller (no
//! identity of its own) is anonymous until it uses a link; then the key it
//! sends (`X-Flux-Device`) makes it an Owner, until the device is removed. A
//! tailnet identity wins over a key.

mod common;

use std::time::Instant;

use axum::http::{Method, StatusCode};
use common::*;
use flux_core::api::{ClaimedOwner, DeviceInfo, Identity, OwnerLink, PlaceKind, Role, ServerInfo};
use flux_net::NetPath;
use flux_server::{api::router, build_state, Config};
use serde_json::json;

const DEVICE: &str = "x-flux-device";

/// The key a claim answered with. No cookie: it would go to every server on
/// the same host; the page keeps the key in its own storage instead.
fn key_of(r: &Resp) -> String {
    assert!(r.header("set-cookie").is_none(), "no cookie");
    r.json::<ClaimedOwner>().key
}

fn viewer() -> Identity {
    Identity {
        role: Role::Viewer,
        login_name: "kid@example.com".into(),
        display_name: "Kid".into(),
        ..owner()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_owner_link_makes_a_lan_browser_an_owner_until_removed() {
    let dir = scratch("owner-link");
    let (st, _) = build_state(Config::for_tests(dir.path()), Instant::now(), None)
        .await
        .unwrap();
    let app = router(st.clone());

    // Anonymous on the LAN: no owner's screens, and no links of its own.
    let me: ServerInfo = call(&app, Method::GET, "/api/v1/server", None, &[], None)
        .await
        .json();
    assert!(me.you.is_none());
    let mine = call(&app, Method::POST, "/api/v1/admin/owner-links", None, &[], None).await;
    assert_eq!(mine.status, StatusCode::FORBIDDEN);
    let list = call(&app, Method::GET, "/api/v1/admin/devices", None, &[], None).await;
    assert_eq!(list.status, StatusCode::FORBIDDEN);

    // The owner (this machine) makes a link.
    let link: OwnerLink = call(
        &app,
        Method::POST,
        "/api/v1/admin/owner-links",
        Some(owner()),
        &[],
        None,
    )
    .await
    .json();
    let token = link.path.strip_prefix("/own#").expect("a fragment").to_string();

    // A bad token does nothing; the right one makes the browser an owner device.
    let bad = call(
        &app,
        Method::POST,
        "/api/v1/owner/claim",
        None,
        &[],
        Some(json!({"token": "nope", "name": "x"})),
    )
    .await;
    assert_eq!(bad.status, StatusCode::GONE);
    let claimed = call(
        &app,
        Method::POST,
        "/api/v1/owner/claim",
        None,
        &[],
        Some(json!({"token": token, "name": "Safari on iPad"})),
    )
    .await;
    assert_eq!(claimed.status, StatusCode::OK, "{}", claimed.text());
    let key = key_of(&claimed);
    let device = claimed.json::<ClaimedOwner>().device;
    assert_eq!(
        (device.role, device.added_by.as_str()),
        (Role::Owner, "this computer")
    );

    // Its key: an Owner, by the name it chose, everywhere.
    let me: ServerInfo = call(&app, Method::GET, "/api/v1/server", None, &[(DEVICE, &key)], None)
        .await
        .json();
    let you = me.you.expect("an identity");
    assert_eq!(
        (you.role, you.display_name.as_str()),
        (Role::Owner, "Safari on iPad")
    );
    let list: Vec<DeviceInfo> = call(
        &app,
        Method::GET,
        "/api/v1/admin/devices",
        None,
        &[(DEVICE, &key)],
        None,
    )
    .await
    .json();
    assert_eq!(list.len(), 1);
    assert!(list[0].this_device);
    // A request the cross-site guard turns away never reaches the key.
    let rebound = call_raw(
        &app,
        Method::GET,
        "/api/v1/server",
        None,
        &[("host", "evil.example"), (DEVICE, &key)],
        None,
    )
    .await;
    assert_eq!(rebound.status, StatusCode::MISDIRECTED_REQUEST);
    // The key in a cookie is nothing: only the header counts.
    let cookie = format!("flux_device={key}");
    let me: ServerInfo = call(
        &app,
        Method::GET,
        "/api/v1/server",
        None,
        &[("cookie", &cookie)],
        None,
    )
    .await
    .json();
    assert!(me.you.is_none());

    // Once only.
    let again = call(
        &app,
        Method::POST,
        "/api/v1/owner/claim",
        None,
        &[],
        Some(json!({"token": token, "name": "y"})),
    )
    .await;
    assert_eq!(again.status, StatusCode::GONE);

    // A tailnet identity wins over a key.
    let me: ServerInfo = call(
        &app,
        Method::GET,
        "/api/v1/server",
        Some(viewer()),
        &[(DEVICE, &key)],
        None,
    )
    .await
    .json();
    assert_eq!(me.you.map(|y| y.role), Some(Role::Viewer));

    // Removed: anonymous again at once.
    let uri = format!("/api/v1/admin/devices/{}", device.id);
    let gone = call(&app, Method::DELETE, &uri, None, &[(DEVICE, &key)], None).await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT);
    let me: ServerInfo = call(&app, Method::GET, "/api/v1/server", None, &[(DEVICE, &key)], None)
        .await
        .json();
    assert!(me.you.is_none());
    let list = call(
        &app,
        Method::GET,
        "/api/v1/admin/devices",
        None,
        &[(DEVICE, &key)],
        None,
    )
    .await;
    assert_eq!(list.status, StatusCode::FORBIDDEN);
}

/// A link changes nothing for a browser flux already places, so it is left
/// for the device it was made for: an owner (this machine, an owner device)
/// is told it already can, and a tailnet login keeps the role it has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_is_left_unused_by_a_browser_it_cant_change() {
    let dir = scratch("owner-link-placed");
    let (st, _) = build_state(Config::for_tests(dir.path()), Instant::now(), None)
        .await
        .unwrap();
    let app = router(st.clone());
    let (token, _) = st.devices.new_link(Role::Owner, "x");
    let claim = json!({"token": token, "name": "x"});

    let owner_tries = call(
        &app,
        Method::POST,
        "/api/v1/owner/claim",
        Some(owner()),
        &[],
        Some(claim.clone()),
    )
    .await;
    assert_eq!(owner_tries.status, StatusCode::CONFLICT, "{}", owner_tries.text());

    let headers = [("host", "127.0.0.1:8096"), ("x-flux-request", "1")];
    let viewer_tries = call_via(
        &app,
        Method::POST,
        "/api/v1/owner/claim",
        Some(viewer()),
        NetPath::Tailnet,
        &headers,
        Some(claim.clone()),
    )
    .await;
    assert_eq!(viewer_tries.status, StatusCode::FORBIDDEN);
    let said = viewer_tries.text();
    assert!(said.contains("kid@example.com through Tailscale"), "{said}");
    // This machine's browsers, given a lesser role (--localhost-role viewer).
    let local = call(
        &app,
        Method::POST,
        "/api/v1/owner/claim",
        Some(viewer()),
        &[],
        Some(claim.clone()),
    )
    .await;
    assert_eq!(local.status, StatusCode::FORBIDDEN);
    assert!(local.text().contains("--localhost-role"), "{}", local.text());
    assert!(st.devices.list(None).is_empty());

    // Still there for the browser it was made for.
    let lan = call(&app, Method::POST, "/api/v1/owner/claim", None, &[], Some(claim)).await;
    assert_eq!(lan.status, StatusCode::OK, "{}", lan.text());
    // Which, now an owner, is told so if it opens a link again.
    let key = key_of(&lan);
    let (another, _) = st.devices.new_link(Role::Owner, "x");
    let twice = call(
        &app,
        Method::POST,
        "/api/v1/owner/claim",
        None,
        &[(DEVICE, &key)],
        Some(json!({"token": another, "name": "x"})),
    )
    .await;
    assert_eq!(twice.status, StatusCode::CONFLICT);
    assert_eq!(st.devices.list(None).len(), 1);
}

/// A tailnet admin's link made owners, which outlived the admin's role: an
/// admin's link makes admins now, and the list says who added them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_admins_link_makes_admins() {
    let dir = scratch("owner-link-admin");
    let (st, _) = build_state(Config::for_tests(dir.path()), Instant::now(), None)
        .await
        .unwrap();
    let app = router(st.clone());
    let admin = Identity {
        role: Role::Admin,
        login_name: "ana@example.com".into(),
        display_name: "Ana".into(),
        ..owner()
    };
    let headers = [("host", "127.0.0.1:8096"), ("x-flux-request", "1")];
    let link: OwnerLink = call_via(
        &app,
        Method::POST,
        "/api/v1/admin/owner-links",
        Some(admin),
        NetPath::Tailnet,
        &headers,
        None,
    )
    .await
    .json();
    let token = link.path.strip_prefix("/own#").unwrap();
    let claimed = call(
        &app,
        Method::POST,
        "/api/v1/owner/claim",
        None,
        &[],
        Some(json!({"token": token, "name": "Tablet"})),
    )
    .await;
    let device = claimed.json::<ClaimedOwner>().device;
    assert_eq!((device.role, device.added_by.as_str()), (Role::Admin, "Ana"));
}

/// `flux-server owner-link` proves itself with the data dir's owner key, so
/// it works where this machine's callers aren't the owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_data_dirs_owner_key_makes_links() {
    let dir = scratch("owner-key");
    let (st, _) = build_state(Config::for_tests(dir.path()), Instant::now(), None)
        .await
        .unwrap();
    let app = router(st.clone());
    let key = std::fs::read_to_string(dir.path().join(flux_server::devices::KEY_FILE)).unwrap();
    // Anonymous (a LAN caller here) without it, or with a wrong one; the Owner with it.
    let without = call(&app, Method::POST, "/api/v1/admin/owner-links", None, &[], None).await;
    assert_eq!(without.status, StatusCode::FORBIDDEN);
    let wrong = call(
        &app,
        Method::POST,
        "/api/v1/admin/owner-links",
        None,
        &[("x-flux-owner-key", "guess")],
        None,
    )
    .await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    let link: OwnerLink = call(
        &app,
        Method::POST,
        "/api/v1/admin/owner-links",
        None,
        &[("x-flux-owner-key", key.trim())],
        None,
    )
    .await
    .json();
    let token = link.path.strip_prefix("/own#").unwrap();
    let claimed = call(
        &app,
        Method::POST,
        "/api/v1/owner/claim",
        None,
        &[],
        Some(json!({"token": token, "name": "x"})),
    )
    .await;
    assert_eq!(
        claimed.json::<ClaimedOwner>().device.added_by,
        "flux-server owner-link"
    );
    // A test server is this machine's only, and its callers are the owner:
    // no "this machine" place.
    assert!(
        link.places.iter().all(|p| p.place != PlaceKind::ThisMachine),
        "{:?}",
        link.places
    );
}

/// The folder browser reads whatever folder it's given, so a page on another
/// site mustn't get the Owner's browser to call it: admin reads need the
/// request header too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_reads_need_the_request_header() {
    let dir = scratch("admin-csrf");
    let (st, _) = build_state(Config::for_tests(dir.path()), Instant::now(), None)
        .await
        .unwrap();
    let app = router(st.clone());
    for uri in [
        "/api/v1/admin/browse",
        "/api/v1/admin/devices",
        "/api/v1/admin/update",
        "/api/v1/admin/addresses",
    ] {
        let bare = call_raw(
            &app,
            Method::GET,
            uri,
            Some(owner()),
            &[("host", "127.0.0.1:8096")],
            None,
        )
        .await;
        assert_eq!(bare.status, StatusCode::FORBIDDEN, "{uri}");
        let marked = call(&app, Method::GET, uri, Some(owner()), &[], None).await;
        assert_eq!(marked.status, StatusCode::OK, "{uri}: {}", marked.text());
    }
    // Not a folder of files: a Windows device path.
    let device_path = call(
        &app,
        Method::GET,
        "/api/v1/admin/browse?path=%5C%5C.%5Cpipe%5Cx",
        Some(owner()),
        &[],
        None,
    )
    .await;
    assert_eq!(device_path.status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claiming_needs_the_request_header_like_every_change() {
    let dir = scratch("owner-link-csrf");
    let (st, _) = build_state(Config::for_tests(dir.path()), Instant::now(), None)
        .await
        .unwrap();
    let app = router(st.clone());
    let (token, _) = st.devices.new_link(Role::Owner, "x");
    // A form on another site can't add X-Flux-Request: the guard refuses it.
    let r = call_raw(
        &app,
        Method::POST,
        "/api/v1/owner/claim",
        None,
        &[("host", "127.0.0.1:8096")],
        Some(json!({"token": token, "name": "x"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // The link still works for the real thing.
    let ok = call(
        &app,
        Method::POST,
        "/api/v1/owner/claim",
        None,
        &[],
        Some(json!({"token": token, "name": "x"})),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_folder_browser_is_the_owners() {
    let dir = scratch("browse");
    let media = dir.path().join("media");
    std::fs::create_dir_all(media.join("Films/Heat (1995)")).unwrap();
    std::fs::write(media.join("Films/Heat (1995)/Heat (1995).mkv"), b"").unwrap();
    let (st, _) = build_state(Config::for_tests(dir.path().join("data")), Instant::now(), None)
        .await
        .unwrap();
    let app = router(st.clone());
    let uri = format!("/api/v1/admin/browse?path={}", urlencode(media.to_str().unwrap()));
    assert_eq!(
        call(&app, Method::GET, &uri, None, &[], None).await.status,
        StatusCode::FORBIDDEN
    );
    let r: flux_core::api::BrowseResult = call(&app, Method::GET, &uri, Some(owner()), &[], None)
        .await
        .json();
    assert_eq!(
        r.folders
            .iter()
            .map(|f| (f.name.as_str(), f.videos))
            .collect::<Vec<_>>(),
        [("Films", Some(1))]
    );
    let here = r.here.expect("what the folder holds");
    assert_eq!((here.videos, here.films, here.taken_by), (1, 1, None));
    // Starting places, and a relative path refused.
    let start: flux_core::api::BrowseResult = call(
        &app,
        Method::GET,
        "/api/v1/admin/browse",
        Some(owner()),
        &[],
        None,
    )
    .await
    .json();
    assert!(start.path.is_none() && !start.folders.is_empty());
    let relative = call(
        &app,
        Method::GET,
        "/api/v1/admin/browse?path=media",
        Some(owner()),
        &[],
        None,
    )
    .await;
    assert_eq!(relative.status, StatusCode::BAD_REQUEST);
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
