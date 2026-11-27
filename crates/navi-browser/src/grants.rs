//! Capability grant state machine for the browser bridge.
//!
//! A grant records a user decision made elsewhere: this module does not
//! authenticate a browser target, persist grants, or reconnect. Grants are
//! stored by id, their origins are normalized once on insertion, and every
//! capability check is answered against a caller-supplied clock value so the
//! state machine stays pure and browser-neutral.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use navi_types::{NaviError, NaviResult};
use serde::{Deserialize, Serialize};

/// A browser capability a grant may authorize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Observe,
    Focus,
    Scroll,
}

impl Capability {
    /// Returns the protocol spelling of this capability.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Focus => "focus",
            Self::Scroll => "scroll",
        }
    }

    /// Returns true when the grant covers this capability.
    pub fn is_granted_by(&self, grant: &Grant) -> bool {
        grant.capabilities.contains(self)
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<&str> for Capability {
    type Error = NaviError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.trim().to_ascii_lowercase().as_str() {
            "observe" => Ok(Self::Observe),
            "focus" => Ok(Self::Focus),
            "scroll" => Ok(Self::Scroll),
            other => Err(NaviError::InvalidInput(format!(
                "unknown capability: {other}"
            ))),
        }
    }
}

/// One decision to allow a set of capabilities for an origin over a time window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub id: String,
    pub origin: String,
    pub capabilities: BTreeSet<Capability>,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub revoked: bool,
}

impl Grant {
    /// Builds an active grant with the given id, origin, capabilities, and lifetime.
    pub fn new(
        id: impl Into<String>,
        origin: impl Into<String>,
        capabilities: impl IntoIterator<Item = Capability>,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Self {
        Self {
            id: id.into(),
            origin: origin.into(),
            capabilities: capabilities.into_iter().collect(),
            issued_at_ms,
            expires_at_ms,
            revoked: false,
        }
    }

    /// Returns true while the grant is neither revoked nor past its lifetime.
    pub fn is_active(&self, now_ms: u64) -> bool {
        !self.revoked && now_ms < self.expires_at_ms
    }

    /// Returns true when this grant authorizes `capability`.
    pub fn has_capability(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

/// Canonicalizes a web origin: trims whitespace, lowercases scheme and host,
/// drops userinfo, paths, queries and fragments, and removes default ports.
///
/// Malformed origins are rejected instead of being guessed at, so a check can
/// never match a grant by accident because two spellings were silently
/// treated as equivalent.
pub fn normalize_origin(origin: &str) -> NaviResult<String> {
    let trimmed = origin.trim();
    if trimmed.is_empty() {
        return Err(NaviError::InvalidInput(
            "origin must not be empty".to_owned(),
        ));
    }
    if trimmed.eq_ignore_ascii_case("null") {
        return Ok("null".to_owned());
    }

    let Some((scheme, remainder)) = trimmed.split_once("://") else {
        return Err(NaviError::InvalidInput(
            "origin must contain a scheme and authority".to_owned(),
        ));
    };
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    {
        return Err(NaviError::InvalidInput("invalid origin scheme".to_owned()));
    }
    let scheme = scheme.to_ascii_lowercase();

    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    if host_port.is_empty() {
        return Err(NaviError::InvalidInput(
            "origin authority must not be empty".to_owned(),
        ));
    }

    let (host, raw_port) = split_host_port(host_port)?;
    if host.is_empty()
        || !host.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'-' | b'_' | b':' | b'%' | b'[' | b']')
        })
    {
        return Err(NaviError::InvalidInput("invalid origin host".to_owned()));
    }

    let port = match raw_port {
        Some(raw) => {
            if raw.is_empty() {
                return Err(NaviError::InvalidInput("invalid origin port".to_owned()));
            }
            let number: u16 = raw
                .parse()
                .map_err(|_| NaviError::InvalidInput("invalid origin port".to_owned()))?;
            if (scheme == "http" && number == 80) || (scheme == "https" && number == 443) {
                None
            } else {
                Some(number)
            }
        }
        None => None,
    };

    let host = host.to_ascii_lowercase();
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    let mut normalized = format!("{scheme}://{host}");
    if let Some(number) = port {
        normalized.push(':');
        normalized.push_str(&number.to_string());
    }
    Ok(normalized)
}

/// Splits an authority into host and optional port, keeping IPv6 literals intact.
fn split_host_port(authority: &str) -> NaviResult<(&str, Option<&str>)> {
    if let Some(inner) = authority.strip_prefix('[') {
        let Some((host, rest)) = inner.split_once(']') else {
            return Err(NaviError::InvalidInput("unterminated IPv6 host".to_owned()));
        };
        let port = match rest {
            "" => None,
            rest => Some(rest.strip_prefix(':').ok_or_else(|| {
                NaviError::InvalidInput("invalid characters after IPv6 host".to_owned())
            })?),
        };
        return Ok((host, port));
    }

    Ok(match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    })
}

/// In-memory collection of grants. Nothing here survives a worker restart.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GrantStore {
    grants: BTreeMap<String, Grant>,
}

impl GrantStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a grant by id, normalizing its origin first.
    pub fn grant(&mut self, grant: Grant) -> NaviResult<()> {
        let mut grant = grant;
        grant.origin = normalize_origin(&grant.origin)?;
        self.grants.insert(grant.id.clone(), grant);
        Ok(())
    }

    /// Revokes a grant by id. Returns true only when this call changed its state.
    pub fn revoke(&mut self, id: &str) -> bool {
        match self.grants.get_mut(id) {
            Some(grant) if !grant.revoked => {
                grant.revoked = true;
                true
            }
            _ => false,
        }
    }

    /// Revokes every grant for `origin`, returning how many were newly revoked.
    pub fn revoke_origin(&mut self, origin: &str) -> NaviResult<usize> {
        let normalized = normalize_origin(origin)?;
        let mut revoked = 0;
        for grant in self.grants.values_mut() {
            if grant.origin == normalized && !grant.revoked {
                grant.revoked = true;
                revoked += 1;
            }
        }
        Ok(revoked)
    }

    /// Returns true when a live grant for `origin` covers `capability` at `now_ms`.
    pub fn check(&self, origin: &str, capability: Capability, now_ms: u64) -> bool {
        let Ok(normalized) = normalize_origin(origin) else {
            return false;
        };
        self.grants.values().any(|grant| {
            grant.origin == normalized && grant.is_active(now_ms) && capability.is_granted_by(grant)
        })
    }

    /// Removes grants whose lifetime ended at or before `now_ms`, returning the count removed.
    pub fn prune_expired(&mut self, now_ms: u64) -> usize {
        let before = self.grants.len();
        self.grants.retain(|_, grant| grant.expires_at_ms > now_ms);
        before - self.grants.len()
    }

    /// Returns the grants that are active at `now_ms`, ordered by id.
    pub fn list_active(&self, now_ms: u64) -> Vec<&Grant> {
        self.grants
            .values()
            .filter(|grant| grant.is_active(now_ms))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_store() -> GrantStore {
        let mut store = GrantStore::new();
        store
            .grant(Grant::new(
                "g1",
                "https://example.com",
                [Capability::Observe, Capability::Focus],
                100,
                500,
            ))
            .expect("valid grant");
        store
    }

    #[test]
    fn check_accepts_equivalent_origin_spellings() {
        let store = sample_store();
        assert!(store.check("https://example.com", Capability::Observe, 200));
        assert!(store.check("HTTPS://EXAMPLE.com:443/", Capability::Focus, 200));
        assert!(store.check("https://user@example.com?tab=1", Capability::Observe, 200));
        assert!(!store.check("http://example.com", Capability::Observe, 200));
        assert_eq!(store.list_active(200)[0].origin, "https://example.com");
    }

    #[test]
    fn check_denies_capabilities_that_were_not_granted() {
        let store = sample_store();
        assert!(store.check("https://example.com", Capability::Observe, 200));
        assert!(!store.check("https://example.com", Capability::Scroll, 200));
        assert!(!store.check("https://example.com", Capability::Focus, 500));
    }

    #[test]
    fn expired_grants_are_denied_and_pruned() {
        let mut store = sample_store();
        assert!(store.check("https://example.com", Capability::Observe, 499));
        assert!(!store.check("https://example.com", Capability::Observe, 500));
        assert_eq!(store.prune_expired(499), 0);
        assert_eq!(store.prune_expired(500), 1);
        assert!(store.list_active(500).is_empty());
    }

    #[test]
    fn revoke_by_id_is_effective_and_idempotent() {
        let mut store = sample_store();
        assert!(store.revoke("g1"));
        assert!(!store.revoke("g1"));
        assert!(!store.revoke("missing"));
        assert!(!store.check("https://example.com", Capability::Observe, 200));
        assert!(store.list_active(200).is_empty());
    }

    #[test]
    fn revoke_origin_only_affects_the_matching_origin() {
        let mut store = sample_store();
        store
            .grant(Grant::new(
                "g2",
                "https://EXAMPLE.com:443/path",
                [Capability::Observe],
                0,
                1_000,
            ))
            .expect("valid grant");
        store
            .grant(Grant::new(
                "g3",
                "http://example.com:80",
                [Capability::Observe],
                0,
                1_000,
            ))
            .expect("valid grant");

        assert_eq!(
            store
                .revoke_origin("HTTPS://Example.COM/")
                .expect("valid origin"),
            2
        );
        assert!(!store.check("https://example.com", Capability::Observe, 200));
        assert!(store.check("http://example.com", Capability::Observe, 200));
        assert_eq!(
            store
                .revoke_origin("https://example.com")
                .expect("valid origin"),
            0
        );
    }

    #[test]
    fn granting_the_same_id_replaces_the_previous_grant() {
        let mut store = sample_store();
        store
            .grant(Grant::new(
                "g1",
                "https://other.example",
                [Capability::Scroll],
                0,
                1_000,
            ))
            .expect("valid grant");

        assert!(!store.check("https://example.com", Capability::Observe, 200));
        assert!(store.check("https://other.example", Capability::Scroll, 200));
        assert_eq!(store.list_active(200).len(), 1);
    }

    #[test]
    fn malformed_origins_are_rejected() {
        for origin in [
            "",
            "   ",
            "example.com",
            "https://",
            "https:///path",
            "https://exa mple.com",
            "https://example.com:abc",
            "https://example.com:70000",
            "https://[::1",
            "https://example.com\npath",
        ] {
            assert!(normalize_origin(origin).is_err(), "accepted {origin:?}");
        }

        let mut store = GrantStore::new();
        let bad = Grant::new(
            "g1",
            "https://example.com:abc",
            [Capability::Observe],
            0,
            1_000,
        );
        assert!(store.grant(bad).is_err());
        assert!(store.list_active(0).is_empty());
        assert!(!store.check("not-an-origin", Capability::Observe, 200));
    }

    #[test]
    fn list_active_filters_expired_and_revoked_in_id_order() {
        let mut store = sample_store();
        store
            .grant(Grant::new(
                "a",
                "https://a.example",
                [Capability::Observe],
                0,
                1_000,
            ))
            .expect("valid grant");
        store
            .grant(Grant::new(
                "z",
                "https://z.example",
                [Capability::Observe],
                0,
                50,
            ))
            .expect("valid grant");
        store
            .grant(Grant::new(
                "y",
                "https://y.example",
                [Capability::Observe],
                0,
                1_000,
            ))
            .expect("valid grant");
        store.revoke("y");

        let ids: Vec<&str> = store
            .list_active(100)
            .iter()
            .map(|grant| grant.id.as_str())
            .collect();
        assert_eq!(ids, ["a", "g1"]);
        assert_eq!(store.prune_expired(100), 1);
    }

    #[test]
    fn capability_parsing_and_display_match_protocol_names() {
        assert_eq!(
            Capability::try_from(" OBSERVE ").expect("valid"),
            Capability::Observe
        );
        assert_eq!(
            Capability::try_from("focus").expect("valid"),
            Capability::Focus
        );
        assert_eq!(
            Capability::try_from("SCROLL").expect("valid"),
            Capability::Scroll
        );
        assert!(Capability::try_from("click").is_err());
        assert_eq!(Capability::Scroll.as_str(), "scroll");
        assert_eq!(Capability::Focus.to_string(), "focus");
    }

    #[test]
    fn grant_survives_serialization_roundtrip() {
        let grant = Grant::new(
            "g1",
            "https://example.com",
            [Capability::Observe, Capability::Scroll],
            100,
            500,
        );
        let json = serde_json::to_string(&grant).expect("serialize");
        let parsed: Grant = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, grant);
    }

    #[test]
    fn null_and_ipv6_origins_normalize_cleanly() {
        assert_eq!(normalize_origin(" NULL ").expect("valid origin"), "null");
        assert_eq!(
            normalize_origin("https://[::1]:443/").expect("valid origin"),
            "https://[::1]"
        );
        assert_eq!(
            normalize_origin("http://[::FFFF:127.0.0.1]:80").expect("valid origin"),
            "http://[::ffff:127.0.0.1]"
        );
        assert_eq!(
            normalize_origin("https://[::1]:8443").expect("valid origin"),
            "https://[::1]:8443"
        );
        assert!(normalize_origin("https://[::1]x").is_err());
    }

    #[test]
    fn empty_store_denies_every_check() {
        let mut store = GrantStore::new();
        assert!(store.list_active(0).is_empty());
        assert!(!store.check("https://example.com", Capability::Observe, 0));
        assert_eq!(store.prune_expired(0), 0);
        assert!(!store.revoke("missing"));
        assert_eq!(
            store
                .revoke_origin("https://example.com")
                .expect("valid origin"),
            0
        );
    }
}
