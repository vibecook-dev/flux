//! Who is calling, and what flux role they get.
//!
//! Identity comes from Tailscale WhoIs (embedded Truffle node or the system
//! tailscaled) and is normalised into [`PeerFacts`]. [`RolePolicy`] turns the
//! facts into a `flux_core::api::Role`:
//!
//! 1. **Tailnet policy is authoritative.** If the tailnet's grants give the
//!    caller the flux app capability (default name `vibecook.dev/cap/flux`,
//!    see research/04 §5.3 and Tailscale KB 1537), its `"role"` values decide:
//!    `{"role":"admin"}` → Admin. Several grants combine to the most
//!    privileged role. An unknown role string fails closed to Guest.
//! 2. Otherwise flux-local overrides by login (`overrides`).
//! 3. Otherwise the login that owns the server node is the Owner.
//! 4. Otherwise tagged devices get `tagged_role`, everyone else
//!    `default_role` (Viewer unless configured).
//!
//! A capability map the sidecar had to drop (too large for its header) makes
//! the caller's grants unknown: everyone but the owner is then a Guest.

use std::collections::HashMap;

use flux_core::api::{Identity, Role};

/// The default application-capability name flux reads from tailnet grants.
pub const DEFAULT_CAPABILITY: &str = "vibecook.dev/cap/flux";

/// What WhoIs says about a tailnet caller, independent of which Tailscale
/// (embedded or system) answered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PeerFacts {
    /// Owner login, e.g. `alice@example.com`; `tagged-devices` for tagged nodes.
    pub login_name: Option<String>,
    pub display_name: Option<String>,
    pub profile_pic_url: Option<String>,
    /// The caller's MagicDNS name, no trailing dot.
    pub node_name: Option<String>,
    /// Stable node id.
    pub node_id: Option<String>,
    pub user_id: Option<i64>,
    /// ACL tags; non-empty means the device has no human owner.
    pub tags: Vec<String>,
    /// Application capabilities granted to the caller on this node.
    pub cap_map: HashMap<String, Vec<serde_json::Value>>,
    /// The capability map was dropped upstream; grants are unknown.
    pub cap_map_dropped: bool,
}

impl PeerFacts {
    /// True for tag-owned devices (shared TVs, servers).
    pub fn is_tagged(&self) -> bool {
        !self.tags.is_empty()
    }

    /// The raw grant values of capability `name`.
    pub fn grants(&self, name: &str) -> &[serde_json::Value] {
        self.cap_map.get(name).map(Vec::as_slice).unwrap_or_default()
    }
}

#[cfg(feature = "embedded")]
impl From<truffle_core::network::TailscalePeerIdentity> for PeerFacts {
    fn from(id: truffle_core::network::TailscalePeerIdentity) -> Self {
        PeerFacts {
            login_name: id.login_name,
            display_name: id.display_name,
            profile_pic_url: id.profile_pic_url,
            node_name: id.dns_name.map(|n| n.trim_end_matches('.').to_string()),
            node_id: id.node_id,
            user_id: id.user_id,
            tags: id.tags,
            cap_map: id.cap_map,
            cap_map_dropped: id.cap_map_dropped,
        }
    }
}

/// How callers map to flux roles. See the module docs for the order.
#[derive(Debug, Clone, PartialEq)]
pub struct RolePolicy {
    /// Application-capability name read from tailnet grants.
    pub capability: String,
    /// The login that owns the server's tailnet node. Filled in from the
    /// node itself (embedded: the login that authorised it; system: the
    /// machine's Tailscale user) unless set explicitly.
    pub owner_login: Option<String>,
    /// Flux-local roles by login (case-insensitive), for households that
    /// don't edit tailnet policy.
    pub overrides: HashMap<String, Role>,
    /// Role for other tailnet users without a grant.
    pub default_role: Role,
    /// Role for tagged devices without a grant.
    pub tagged_role: Role,
    /// Role for requests from this machine's loopback interface. `None`
    /// treats localhost like the LAN (no identity). On a multi-user host any
    /// local process can reach 127.0.0.1, so set `None` there.
    pub localhost_role: Option<Role>,
}

impl Default for RolePolicy {
    fn default() -> Self {
        RolePolicy {
            capability: DEFAULT_CAPABILITY.to_string(),
            owner_login: None,
            overrides: HashMap::new(),
            default_role: Role::Viewer,
            tagged_role: Role::Viewer,
            localhost_role: Some(Role::Owner),
        }
    }
}

/// Privilege order used to combine grants: Owner > Admin > Viewer > Kid > Guest.
fn rank(role: Role) -> u8 {
    match role {
        Role::Owner => 4,
        Role::Admin => 3,
        Role::Viewer => 2,
        Role::Kid => 1,
        Role::Guest => 0,
    }
}

/// The outcome of reading the flux capability's `"role"` fields.
#[derive(Debug, PartialEq, Eq)]
enum GrantedRole {
    /// No grant names a role (absent capability, or grants carrying only
    /// other permissions such as libraries).
    None,
    /// The most privileged role among the grants.
    Role(Role),
    /// A grant names a role flux does not know: fail closed.
    Invalid,
}

fn granted_role(values: &[serde_json::Value]) -> GrantedRole {
    let mut best: Option<Role> = None;
    for value in values {
        let Some(role) = value.get("role") else { continue };
        let Ok(role) = serde_json::from_value::<Role>(role.clone()) else {
            return GrantedRole::Invalid;
        };
        if best.is_none_or(|b| rank(role) > rank(b)) {
            best = Some(role);
        }
    }
    best.map_or(GrantedRole::None, GrantedRole::Role)
}

impl RolePolicy {
    fn is_owner(&self, peer: &PeerFacts) -> bool {
        match (&self.owner_login, &peer.login_name) {
            (Some(owner), Some(login)) => !peer.is_tagged() && owner.eq_ignore_ascii_case(login),
            _ => false,
        }
    }

    /// The flux role of a tailnet caller.
    pub fn role_for(&self, peer: &PeerFacts) -> Role {
        if peer.cap_map_dropped {
            return if self.is_owner(peer) {
                Role::Owner
            } else {
                Role::Guest
            };
        }
        match granted_role(peer.grants(&self.capability)) {
            GrantedRole::Role(role) => return role,
            GrantedRole::Invalid => return Role::Guest,
            GrantedRole::None => {}
        }
        if let Some(login) = &peer.login_name {
            let found = self.overrides.iter().find(|(k, _)| k.eq_ignore_ascii_case(login));
            if let Some((_, role)) = found {
                return *role;
            }
        }
        if self.is_owner(peer) {
            Role::Owner
        } else if peer.is_tagged() {
            self.tagged_role
        } else {
            self.default_role
        }
    }

    /// The `Identity` a tailnet caller gets, role included.
    pub fn identity_for(&self, peer: &PeerFacts) -> Identity {
        let login = peer.login_name.clone().unwrap_or_default();
        let display = peer
            .display_name
            .clone()
            .filter(|d| !d.is_empty())
            .or_else(|| peer.node_name.clone().filter(|_| peer.is_tagged()))
            .unwrap_or_else(|| login.clone());
        Identity {
            login_name: login,
            display_name: display,
            profile_pic_url: peer.profile_pic_url.clone(),
            node_name: peer.node_name.clone(),
            role: self.role_for(peer),
            tags: peer.tags.clone(),
        }
    }

    /// The `Identity` of a loopback caller, or `None` when localhost is not
    /// trusted ([`localhost_role`](Self::localhost_role) is `None`).
    pub fn localhost_identity(&self) -> Option<Identity> {
        let role = self.localhost_role?;
        let login = self.owner_login.clone().unwrap_or_else(|| "local".to_string());
        Some(Identity {
            display_name: login.clone(),
            login_name: login,
            profile_pic_url: None,
            node_name: None,
            role,
            tags: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn peer(login: &str) -> PeerFacts {
        PeerFacts {
            login_name: Some(login.into()),
            display_name: Some("Alice".into()),
            node_name: Some("alice-phone.tail1234.ts.net".into()),
            ..Default::default()
        }
    }

    fn with_caps(mut p: PeerFacts, values: Vec<serde_json::Value>) -> PeerFacts {
        p.cap_map.insert(DEFAULT_CAPABILITY.into(), values);
        p
    }

    fn policy() -> RolePolicy {
        RolePolicy {
            owner_login: Some("owner@example.com".into()),
            ..Default::default()
        }
    }

    #[test]
    fn capability_roles_map_to_flux_roles() {
        let p = policy();
        assert_eq!(
            p.role_for(&with_caps(peer("a@x"), vec![json!({"role":"admin"})])),
            Role::Admin
        );
        assert_eq!(
            p.role_for(&with_caps(peer("a@x"), vec![json!({"role":"viewer"})])),
            Role::Viewer
        );
        assert_eq!(
            p.role_for(&with_caps(peer("a@x"), vec![json!({"role":"kid"})])),
            Role::Kid
        );
    }

    #[test]
    fn several_grants_combine_to_the_most_privileged() {
        let caps = vec![
            json!({"role":"kid"}),
            json!({"libraries":["Cartoons"]}),
            json!({"role":"admin"}),
        ];
        assert_eq!(policy().role_for(&with_caps(peer("a@x"), caps)), Role::Admin);
    }

    #[test]
    fn the_capability_is_authoritative_even_for_the_owner() {
        let owner = with_caps(peer("owner@example.com"), vec![json!({"role":"viewer"})]);
        assert_eq!(policy().role_for(&owner), Role::Viewer);
    }

    #[test]
    fn unknown_role_fails_closed() {
        let caps = vec![json!({"role":"admin"}), json!({"role":"superuser"})];
        assert_eq!(policy().role_for(&with_caps(peer("a@x"), caps)), Role::Guest);
    }

    #[test]
    fn grants_without_a_role_fall_through() {
        let caps = vec![json!({"libraries":["Movies"]})];
        assert_eq!(policy().role_for(&with_caps(peer("a@x"), caps)), Role::Viewer);
    }

    #[test]
    fn owner_overrides_and_defaults() {
        let mut p = policy();
        assert_eq!(
            p.role_for(&peer("OWNER@example.com")),
            Role::Owner,
            "login match is case-insensitive"
        );
        assert_eq!(p.role_for(&peer("friend@example.com")), Role::Viewer);
        p.overrides.insert("friend@example.com".into(), Role::Kid);
        assert_eq!(p.role_for(&peer("Friend@Example.com")), Role::Kid);
        p.default_role = Role::Guest;
        assert_eq!(p.role_for(&peer("stranger@example.com")), Role::Guest);
    }

    #[test]
    fn tagged_devices_are_never_the_owner() {
        let mut tv = peer("owner@example.com");
        tv.tags = vec!["tag:tv".into()];
        let mut p = policy();
        p.tagged_role = Role::Kid;
        assert_eq!(p.role_for(&tv), Role::Kid);
    }

    #[test]
    fn dropped_cap_map_fails_closed_except_for_the_owner() {
        let mut friend = peer("friend@example.com");
        friend.cap_map_dropped = true;
        assert_eq!(policy().role_for(&friend), Role::Guest);
        let mut owner = peer("owner@example.com");
        owner.cap_map_dropped = true;
        assert_eq!(policy().role_for(&owner), Role::Owner);
    }

    #[test]
    fn identity_carries_profile_and_role() {
        let id = policy().identity_for(&with_caps(peer("a@x"), vec![json!({"role":"admin"})]));
        assert_eq!(id.login_name, "a@x");
        assert_eq!(id.display_name, "Alice");
        assert_eq!(id.node_name.as_deref(), Some("alice-phone.tail1234.ts.net"));
        assert_eq!(id.role, Role::Admin);

        let mut tv = PeerFacts {
            login_name: Some("tagged-devices".into()),
            node_name: Some("living-room.tail1234.ts.net".into()),
            ..Default::default()
        };
        tv.tags = vec!["tag:tv".into()];
        assert_eq!(
            policy().identity_for(&tv).display_name,
            "living-room.tail1234.ts.net"
        );
    }

    #[test]
    fn localhost_identity_follows_policy() {
        let id = policy().localhost_identity().unwrap();
        assert_eq!(
            (id.login_name.as_str(), id.role),
            ("owner@example.com", Role::Owner)
        );
        let p = RolePolicy {
            localhost_role: None,
            ..policy()
        };
        assert!(p.localhost_identity().is_none());
    }
}
