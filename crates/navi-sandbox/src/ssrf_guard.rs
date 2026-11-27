//! SSRF guard for outbound HTTP(S) URLs.
//!
//! [`validate_url`] parses a URL without DNS resolution and rejects schemes
//! other than `http`/`https`, embedded credentials, and literal addresses in
//! loopback, private, link-local, CGNAT, or otherwise reserved ranges.
//! Hostnames that are not address literals are checked against the policy's
//! localhost rules and explicit host exceptions.

use navi_types::{NaviError, NaviResult};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Policy controlling which URLs [`validate_url`] accepts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SsrfPolicy {
    /// Permit loopback addresses and `localhost` names when true.
    pub allow_localhost: bool,
    /// Hosts (or `host:port` authorities) that bypass the address checks.
    pub allowed_hosts: Vec<String>,
}

impl SsrfPolicy {
    /// Creates a default-deny policy with no localhost or host exceptions.
    pub fn new() -> Self {
        Self::default()
    }
}

/// The parsed form of a URL that passed SSRF validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedUrl {
    /// Normalized scheme: `http` or `https`.
    pub scheme: String,
    /// Normalized host: lowercase, brackets removed, trailing dot removed.
    pub host: String,
    /// Explicit port, or the scheme default (80 for `http`, 443 for `https`).
    pub port: u16,
    /// Path, query, and fragment; `/` when the URL has no path.
    pub path_and_query: String,
}

/// Returns true when `ip` belongs to a loopback, private, link-local, CGNAT,
/// unspecified, multicast, or otherwise reserved range.
///
/// IPv4-mapped and IPv4-compatible IPv6 addresses are classified by the IPv4
/// address they embed, so `::ffff:127.0.0.1` is treated as `127.0.0.1`.
pub fn is_private_or_reserved_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_reserved_v4(v4),
        IpAddr::V6(v6) => is_reserved_v6(v6),
    }
}

/// Validates `url` against `policy` and returns its parsed components.
///
/// # Errors
///
/// Returns [`NaviError::InvalidInput`] when the URL is malformed, uses a
/// non-HTTP(S) scheme, carries credentials, or targets a blocked address.
pub fn validate_url(url: &str, policy: &SsrfPolicy) -> NaviResult<ValidatedUrl> {
    if url.is_empty()
        || url
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Err(NaviError::InvalidInput(
            "URL is empty or contains invalid characters".into(),
        ));
    }

    let Some(scheme_end) = url.find(':') else {
        return Err(NaviError::InvalidInput("URL is missing a scheme".into()));
    };
    let raw_scheme = &url[..scheme_end];
    if !raw_scheme.eq_ignore_ascii_case("http") && !raw_scheme.eq_ignore_ascii_case("https") {
        return Err(NaviError::InvalidInput(format!(
            "unsupported URL scheme '{raw_scheme}'"
        )));
    }
    let scheme = raw_scheme.to_ascii_lowercase();

    let Some(rest) = url[scheme_end + 1..].strip_prefix("//") else {
        return Err(NaviError::InvalidInput(
            "URL is missing '//' after the scheme".into(),
        ));
    };

    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let tail = &rest[authority_end..];

    if authority.is_empty() {
        return Err(NaviError::InvalidInput("URL has an empty host".into()));
    }
    if authority.contains('@') {
        return Err(NaviError::InvalidInput(
            "URL must not contain embedded credentials".into(),
        ));
    }

    let (host_token, port) = split_authority(authority)?;
    if host_token.is_empty()
        || host_token.bytes().any(|b| {
            b.is_ascii_whitespace() || matches!(b, b'/' | b'\\' | b'%' | b'[' | b']' | b'@' | b'#')
        })
    {
        return Err(NaviError::InvalidInput("URL host is malformed".into()));
    }

    let host = host_token.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Err(NaviError::InvalidInput("URL has an empty host".into()));
    }

    let authority_lower = authority.to_ascii_lowercase();
    let explicitly_allowed = policy.allowed_hosts.iter().any(|allowed| {
        let allowed = allowed.trim().trim_end_matches('.');
        allowed.eq_ignore_ascii_case(host.as_str())
            || allowed.eq_ignore_ascii_case(authority_lower.as_str())
    });

    if let Some(ip) = parse_host_ip(&host) {
        let localhost_permitted = policy.allow_localhost && is_loopback_target(ip);
        if !explicitly_allowed && !localhost_permitted && is_private_or_reserved_ip(ip) {
            return Err(NaviError::InvalidInput(format!(
                "host '{host}' is a private or reserved address"
            )));
        }
    } else if !explicitly_allowed
        && !policy.allow_localhost
        && (host == "localhost" || host.ends_with(".localhost") || host == "localhost.localdomain")
    {
        return Err(NaviError::InvalidInput(format!(
            "host '{host}' is not permitted"
        )));
    }

    let default_port = if scheme == "https" { 443 } else { 80 };
    let path_and_query = if tail.is_empty() {
        "/".to_owned()
    } else {
        tail.to_owned()
    };

    Ok(ValidatedUrl {
        scheme,
        host,
        port: port.unwrap_or(default_port),
        path_and_query,
    })
}

/// Splits an authority into its host token and optional explicit port.
fn split_authority(authority: &str) -> NaviResult<(&str, Option<u16>)> {
    if let Some(inner) = authority.strip_prefix('[') {
        let Some(end) = inner.find(']') else {
            return Err(NaviError::InvalidInput(
                "URL has an unterminated IPv6 host".into(),
            ));
        };
        let host = &inner[..end];
        let remainder = &inner[end + 1..];
        let port = if remainder.is_empty() {
            None
        } else {
            let raw_port = remainder.strip_prefix(':').ok_or_else(|| {
                NaviError::InvalidInput("URL has a malformed IPv6 authority".into())
            })?;
            Some(parse_port(raw_port)?)
        };
        return Ok((host, port));
    }

    if authority.contains('[') || authority.contains(']') {
        return Err(NaviError::InvalidInput("URL host is malformed".into()));
    }
    if let Some((host, raw_port)) = authority.rsplit_once(':') {
        if host.contains(':') {
            return Err(NaviError::InvalidInput(
                "IPv6 hosts must be enclosed in brackets".into(),
            ));
        }
        return Ok((host, Some(parse_port(raw_port)?)));
    }
    Ok((authority, None))
}

/// Parses a decimal port into the range accepted by TCP.
fn parse_port(raw: &str) -> NaviResult<u16> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(NaviError::InvalidInput(format!("invalid port '{raw}'")));
    }
    raw.parse::<u16>()
        .map_err(|_| NaviError::InvalidInput(format!("port '{raw}' is out of range")))
}

/// Interprets a host token as an IP address, including alternate IPv4 forms.
fn parse_host_ip(host: &str) -> Option<IpAddr> {
    host.parse::<IpAddr>()
        .ok()
        .or_else(|| parse_ipv4_host(host).map(IpAddr::V4))
}

/// Returns true when `ip` is loopback, including IPv6 forms wrapping IPv4.
fn is_loopback_target(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
                || v6.to_ipv4().is_some_and(|v4| v4.is_loopback())
        }
    }
}

fn is_reserved_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 0 // 0.0.0.0/8 (unspecified and "this network")
        || a == 10 // RFC1918
        || a == 127 // loopback
        || (a == 100 && (64..=127).contains(&b)) // CGNAT
        || (a == 169 && b == 254) // link-local / cloud metadata
        || (a == 172 && (16..=31).contains(&b)) // RFC1918
        || (a == 192 && b == 168) // RFC1918
        || a >= 224 // multicast and reserved
}

fn is_reserved_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    let first = ip.segments()[0];
    if (first & 0xff00) == 0xff00 // ff00::/8 multicast
        || (first & 0xffc0) == 0xfe80 // fe80::/10 link-local
        || (first & 0xfe00) == 0xfc00
    // fc00::/7 unique local
    {
        return true;
    }
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_reserved_v4(mapped);
    }
    if let Some(embedded) = ip.to_ipv4() {
        return is_reserved_v4(embedded);
    }
    false
}

/// Parses dotted-decimal, integer, octal, and hexadecimal IPv4 literals.
fn parse_ipv4_host(host: &str) -> Option<Ipv4Addr> {
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Some(ip);
    }
    if host.contains('.') {
        let mut parts = host.split('.');
        let a = parse_ipv4_component(parts.next()?)?;
        let b = parse_ipv4_component(parts.next()?)?;
        let c = parse_ipv4_component(parts.next()?)?;
        let d = parse_ipv4_component(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        return Some(Ipv4Addr::new(a, b, c, d));
    }
    let value = parse_ipv4_number(host)?;
    Some(Ipv4Addr::from(value))
}

fn parse_ipv4_component(part: &str) -> Option<u8> {
    u8::try_from(parse_ipv4_number(part)?).ok()
}

/// Parses one unsigned IPv4 number, honoring `0x` hex and leading-zero octal.
fn parse_ipv4_number(raw: &str) -> Option<u32> {
    let (digits, radix) =
        if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
            (hex, 16)
        } else if raw.len() > 1 && raw.starts_with('0') {
            (&raw[1..], 8)
        } else {
            (raw, 10)
        };
    if digits.is_empty() {
        return None;
    }
    let valid = match radix {
        8 => digits.bytes().all(|b| (b'0'..=b'7').contains(&b)),
        10 => digits.bytes().all(|b| b.is_ascii_digit()),
        _ => digits.bytes().all(|b| b.is_ascii_hexdigit()),
    };
    if !valid {
        return None;
    }
    u32::from_str_radix(digits, radix).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(url: &str, policy: &SsrfPolicy) {
        let result = validate_url(url, policy);
        assert!(result.is_err(), "expected rejection of '{url}'");
    }

    fn accepted(url: &str, policy: &SsrfPolicy) -> ValidatedUrl {
        match validate_url(url, policy) {
            Ok(validated) => validated,
            Err(err) => panic!("expected acceptance of '{url}', got {err}"),
        }
    }

    #[test]
    fn accepts_public_http_and_https_urls() {
        let policy = SsrfPolicy::new();
        let http = accepted("http://example.com:8080/path?q=1#frag", &policy);
        assert_eq!(http.scheme, "http");
        assert_eq!(http.host, "example.com");
        assert_eq!(http.port, 8080);
        assert_eq!(http.path_and_query, "/path?q=1#frag");

        let https = accepted("https://8.8.8.8", &policy);
        assert_eq!(https.scheme, "https");
        assert_eq!(https.host, "8.8.8.8");
        assert_eq!(https.port, 443);
        assert_eq!(https.path_and_query, "/");

        let mixed_case = accepted("HTTP://Example.COM./", &policy);
        assert_eq!(mixed_case.scheme, "http");
        assert_eq!(mixed_case.host, "example.com");
    }

    #[test]
    fn rejects_non_http_schemes() {
        let policy = SsrfPolicy::new();
        for url in [
            "ftp://example.com/file",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "gopher://127.0.0.1",
            "example.com/path",
            "//example.com/path",
            "http:example.com",
        ] {
            rejected(url, &policy);
        }
    }

    #[test]
    fn rejects_embedded_credentials() {
        let policy = SsrfPolicy::new();
        for url in [
            "http://user:pass@example.com/",
            "https://user@example.com/",
            "http://user:pass@127.0.0.1/",
            "http://example.com:80@evil.test/",
        ] {
            rejected(url, &policy);
        }
    }

    #[test]
    fn blocks_ipv4_private_and_reserved_ranges() {
        let policy = SsrfPolicy::new();
        for url in [
            "http://127.0.0.1/",
            "http://127.255.255.254/",
            "http://10.1.2.3/",
            "http://172.16.0.1/",
            "http://172.31.255.255/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
            "http://224.0.0.1/",
        ] {
            rejected(url, &policy);
        }
        // The public neighbors of each reserved range remain usable.
        accepted("http://172.15.0.1/", &policy);
        accepted("http://172.32.0.1/", &policy);
        accepted("http://100.63.255.255/", &policy);
        accepted("http://100.128.0.1/", &policy);
    }

    #[test]
    fn blocks_ipv6_loopback_link_local_ula_and_mapped_ipv4() {
        let policy = SsrfPolicy::new();
        for url in [
            "http://[::1]/",
            "http://[::]/",
            "http://[fe80::1]/",
            "http://[febf::1]/",
            "http://[fc00::1]/",
            "http://[fd12:3456::1]/",
            "http://[ff02::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::ffff:10.0.0.1]/",
            "http://[::ffff:192.168.1.1]/",
            "http://[::ffff:7f00:1]/",
        ] {
            rejected(url, &policy);
        }
        let public = accepted("https://[2001:4860:4860::8888]/", &policy);
        assert_eq!(public.host, "2001:4860:4860::8888");
        assert_eq!(public.port, 443);
    }

    #[test]
    fn allow_localhost_permits_only_loopback() {
        let policy = SsrfPolicy {
            allow_localhost: true,
            ..SsrfPolicy::default()
        };
        accepted("http://localhost:3000/api", &policy);
        accepted("http://app.localhost/", &policy);
        accepted("http://127.0.0.1:8080/", &policy);
        accepted("http://[::1]/", &policy);
        accepted("http://[::ffff:127.0.0.1]/", &policy);
        rejected("http://10.0.0.1/", &policy);
        rejected("http://169.254.169.254/", &policy);
        rejected("http://[fe80::1]/", &policy);

        let strict = SsrfPolicy::new();
        rejected("http://localhost/", &strict);
        rejected("http://app.localhost/", &strict);
    }

    #[test]
    fn allowed_hosts_override_address_checks() {
        let policy = SsrfPolicy {
            allow_localhost: false,
            allowed_hosts: vec!["127.0.0.1".into(), "INTERNAL.Example.COM.".into()],
        };
        accepted("http://127.0.0.1:9000/", &policy);
        accepted("http://internal.example.com/", &policy);
        accepted("http://internal.example.com:9000/", &policy);
        rejected("http://192.168.0.1/", &policy);
        rejected("http://localhost/", &policy);
    }

    #[test]
    fn blocks_alternate_ipv4_notations() {
        let policy = SsrfPolicy::new();
        for url in [
            "http://2130706433/", // 127.0.0.1 as a decimal integer
            "http://0x7f000001/", // hexadecimal integer
            "http://0177.0.0.1/", // octal first component
            "http://0x7f.0.0.1/", // hexadecimal first component
            "http://3232235777/", // 192.168.1.1 as a decimal integer
        ] {
            rejected(url, &policy);
        }
        accepted("http://0x08080808/", &policy); // 8.8.8.8
    }

    #[test]
    fn rejects_malformed_urls_and_ports() {
        let policy = SsrfPolicy::new();
        for url in [
            "",
            "   ",
            "http:///path",
            "http://",
            "http://[::1/",
            "http://[::1]junk/",
            "http://::1/",
            "http://example.com:abc/",
            "http://example.com:99999/",
            "http://example.com:/",
            "http://example.com\\@evil.test/",
            "http://exa mple.com/",
            " http://example.com/",
            "http://example.com/ ",
            "http://example%2Ecom/",
        ] {
            rejected(url, &policy);
        }
    }

    #[test]
    fn classifies_ip_addresses_directly() {
        for raw in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            let ip: IpAddr = raw.parse().expect("valid test IP literal");
            assert!(
                is_private_or_reserved_ip(ip),
                "expected '{raw}' to be classified as reserved"
            );
        }
        for raw in ["8.8.8.8", "1.1.1.1", "172.32.0.1", "2001:4860::8888"] {
            let ip: IpAddr = raw.parse().expect("valid test IP literal");
            assert!(
                !is_private_or_reserved_ip(ip),
                "expected '{raw}' to be classified as public"
            );
        }
    }

    #[test]
    fn checks_reserved_range_boundaries() {
        let reserved = [
            "10.0.0.0",
            "10.255.255.255",
            "100.64.0.0",
            "100.127.255.255",
            "127.0.0.0",
            "127.255.255.255",
            "169.254.0.0",
            "169.254.255.255",
            "172.16.0.0",
            "172.31.255.255",
            "192.168.0.0",
            "192.168.255.255",
            "224.0.0.0",
        ];
        for raw in reserved {
            let ip: IpAddr = raw.parse().expect("valid test IP literal");
            assert!(
                is_private_or_reserved_ip(ip),
                "expected '{raw}' to be classified as reserved"
            );
        }

        let public = [
            "9.255.255.255",
            "11.0.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "126.255.255.255",
            "128.0.0.0",
            "169.253.255.255",
            "169.255.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.167.255.255",
            "192.169.0.0",
            "223.255.255.255",
        ];
        for raw in public {
            let ip: IpAddr = raw.parse().expect("valid test IP literal");
            assert!(
                !is_private_or_reserved_ip(ip),
                "expected '{raw}' to be classified as public"
            );
        }
    }
}
