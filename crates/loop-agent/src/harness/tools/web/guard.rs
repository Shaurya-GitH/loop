//! SSRF guard for outbound web requests.
//!
//! Every URL is validated *before* a request is made (scheme, credentials,
//! literal-IP hosts) and hostname resolution is intercepted so a connection is
//! only ever established to a public address. This closes both the plain
//! "fetch localhost" case and DNS-rebinding variants: redirects are re-validated
//! by the caller (see [`super`]) and resolution is checked at connect time.
//!
//! The guard fails closed: a hostname that resolves to *any* non-public address
//! is rejected outright. Set [`FetchPolicy::allow_private_addresses`] to reach
//! intranet mirrors and self-hosted services; that policy also permits
//! localhost, loopback and link-local literals.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve};
use reqwest::Url;

/// Whether private addresses may be fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FetchPolicy {
    /// Allow loopback, private, link-local and reserved addresses. Intended for
    /// intranet mirrors; leaves the default fail-closed posture.
    pub allow_private_addresses: bool,
}

impl FetchPolicy {
    /// Whether `ip` may be connected to under this policy.
    pub fn permits_addr(self, ip: IpAddr) -> bool {
        !is_private_address(ip) || self.allow_private_addresses
    }

    /// Whether a `localhost` / `*.localhost` hostname may be fetched.
    fn permits_localhost(self) -> bool {
        self.allow_private_addresses
    }
}

/// Whether `ip` is anything other than a global unicast address: loopback,
/// private, link-local, CGNAT, shared, documentation, benchmark, reserved,
/// multicast or unspecified.
pub fn is_private_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_ipv4(v4),
        IpAddr::V6(v6) => is_private_ipv6(v6),
    }
}

fn is_private_ipv4(v: Ipv4Addr) -> bool {
    let o = v.octets();
    v.is_loopback()
        || v.is_broadcast()
        || v.is_multicast()
        || o[0] == 0 // "this network"
        || o[0] == 10 // private
        || (o[0] == 100 && o[1] & 0xc0 == 64) // CGNAT 100.64/10
        || (o[0] == 169 && o[1] == 254) // link-local (cloud metadata)
        || (o[0] == 172 && o[1] & 0xf0 == 16) // private
        || (o[0] == 192 && o[1] == 168) // private
        || (o[0] == 192 && o[1] == 0 && matches!(o[2], 0 | 2)) // shared / TEST-NET-1
        || (o[0] == 198 && matches!(o[1], 18 | 19)) // benchmark
        || (o[0] == 198 && o[1] == 51 && o[2] == 100) // TEST-NET-2
        || (o[0] == 203 && o[1] == 0 && o[2] == 113) // TEST-NET-3
        || o[0] >= 240 // reserved
}

fn is_private_ipv6(v: Ipv6Addr) -> bool {
    let s = v.segments();
    v.is_loopback()
        || v.is_unspecified()
        || v.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
        || (s[0] & 0xffc0) == 0xfe80 // link-local
        || (s[0] == 0x0064
            && s[1] == 0xff9b
            && s[2..6].iter().all(|segment| *segment == 0)) // NAT64 well-known prefix
        || s[0] == 0x2002 // 6to4
        || s[..6].iter().all(|segment| *segment == 0) // IPv4-compatible
        || (s[0] & 0xffc0) == 0xfec0 // site-local
        || (s[0] == 0x2001 && s[1] == 0) // Teredo
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || v.to_ipv4_mapped().is_some_and(is_private_ipv4)
}

/// Validate a request target before connecting.
///
/// Checks the scheme (`http`/`https` only), rejects embedded credentials, and
/// for literal-IP hosts applies [`FetchPolicy::permits_addr`]. Hostnames are
/// checked at connect time by [`GuardResolver`] (which also covers redirects).
pub fn validate_request_url(url: &Url, policy: FetchPolicy) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "unsupported URL scheme '{}': only http and https are allowed",
            url.scheme()
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs with embedded credentials are not allowed".into());
    }
    // `host_str` serializes IPv6 literals with brackets; strip one pair so the
    // `IpAddr` parse recognizes both families.
    let Some(host) = url.host_str() else {
        return Err("URL has no host".into());
    };
    let literal = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = literal.parse::<IpAddr>() {
        if !policy.permits_addr(ip) {
            return Err(format!("refusing to fetch private address {ip}"));
        }
        return Ok(());
    }
    let host_lower = host.to_ascii_lowercase();
    if (host_lower == "localhost" || host_lower.ends_with(".localhost"))
        && !policy.permits_localhost()
    {
        return Err("refusing to fetch localhost".into());
    }
    Ok(())
}

/// Resolve a hostname locally before forwarding the URL to Soket.
pub(super) async fn validate_resolved_host(url: &Url, policy: FetchPolicy) -> Result<(), String> {
    validate_request_url(url, policy)?;
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| format!("could not resolve '{host}' before Soket fetch: {error}"))?
        .map(|addr| addr.ip())
        .collect::<Vec<_>>();
    validate_resolved_addrs(host, &addrs, policy)
}

fn validate_resolved_addrs(
    host: &str,
    addrs: &[IpAddr],
    policy: FetchPolicy,
) -> Result<(), String> {
    if addrs.is_empty() {
        return Err(format!("host '{host}' did not resolve to any address"));
    }
    if let Some(addr) = addrs.iter().find(|addr| !policy.permits_addr(**addr)) {
        return Err(format!(
            "refusing to send '{host}' to Soket: resolves to forbidden {addr}"
        ));
    }
    Ok(())
}

/// DNS resolver that enforces [`FetchPolicy`] at connect time.
///
/// Resolution is rejected when any resolved address is non-public, so the
/// connection can never land on a forbidden address (DNS rebinding included).
pub struct GuardResolver {
    policy: FetchPolicy,
}

impl GuardResolver {
    /// Resolver enforcing `policy`.
    pub fn new(policy: FetchPolicy) -> Self {
        Self { policy }
    }
}

impl Resolve for GuardResolver {
    fn resolve(&self, name: Name) -> reqwest::dns::Resolving {
        let policy = self.policy;
        Box::pin(async move {
            // The port is irrelevant for validation; hyper overrides it.
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            let host = name.as_str().to_string();
            match addrs.iter().find(|addr| !policy.permits_addr(addr.ip())) {
                Some(addr) => Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing to connect to '{host}': resolves to forbidden {}",
                        addr.ip()
                    ),
                )
                .into()),
                None if addrs.is_empty() => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("host '{host}' did not resolve to any address"),
                )
                .into()),
                None => Ok(Box::new(addrs.into_iter()) as Addrs),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_private(ip: &str) -> bool {
        is_private_address(ip.parse().unwrap())
    }

    #[test]
    fn ipv4_classification() {
        for blocked in [
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "10.255.1.1",
            "100.64.0.1",
            "100.127.255.254",
            "127.0.0.1",
            "127.255.255.254",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.1",
            "192.0.0.1",
            "192.0.2.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.19.255.1",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "239.1.1.1",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            assert!(is_private(blocked), "{blocked} should be private");
        }
        for public in [
            "1.1.1.1",
            "8.8.8.8",
            "100.63.255.1",
            "100.128.0.1",
            "172.15.255.1",
            "172.32.0.1",
            "172.217.4.46",
            "192.0.1.1",
            "192.169.0.1",
            "198.17.0.1",
            "198.20.0.1",
            "198.52.100.1",
            "203.0.114.1",
            "223.255.255.1",
        ] {
            assert!(!is_private(public), "{public} should be public");
        }
    }

    #[test]
    fn ipv6_classification() {
        for blocked in [
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:192.168.0.1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "64:ff9b::808:808",
            "2002:808:808::",
            "::808:808",
            "fec0::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
        ] {
            assert!(is_private(blocked), "{blocked} should be private");
        }
        for public in [
            "2606:4700:4700::1111",
            "2a00:1450:4001::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_private(public), "{public} should be public");
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn url_validation() {
        let strict = FetchPolicy::default();
        let lenient = FetchPolicy {
            allow_private_addresses: true,
        };

        assert!(validate_request_url(&url("https://example.com"), strict).is_ok());
        assert!(validate_request_url(&url("http://example.com:8080/x"), strict).is_ok());
        for blocked in [
            "ftp://example.com",
            "file:///etc/passwd",
            "https://user:pass@example.com",
            "http://localhost/",
            "http://api.localhost/",
            "http://127.0.0.1/",
            "http://192.168.1.10/",
            "http://[::1]/",
            "http://[::ffff:10.0.0.1]/",
            "http://169.254.169.254/latest/meta-data",
        ] {
            assert!(
                validate_request_url(&url(blocked), strict).is_err(),
                "{blocked} should be rejected"
            );
        }
        // Denial reasons are distinguishable for the two guard paths.
        let err = validate_request_url(&url("http://192.168.1.10/"), strict).unwrap_err();
        assert!(err.contains("private address"), "got: {err}");
        let err = validate_request_url(&url("http://localhost/"), strict).unwrap_err();
        assert!(err.contains("localhost"), "got: {err}");

        // Lenient policy allows intranet targets …
        for allowed in [
            "http://localhost/",
            "http://127.0.0.1/",
            "http://192.168.1.10/",
            "http://[::1]/",
        ] {
            assert!(validate_request_url(&url(allowed), lenient).is_ok());
        }
        // … including via `permits_addr` directly.
        assert!(lenient.permits_addr("10.0.0.1".parse().unwrap()));
        // … but never weakens scheme/credential rules.
        assert!(validate_request_url(&url("ftp://example.com"), lenient).is_err());
        assert!(validate_request_url(&url("https://u:p@example.com"), lenient).is_err());
    }

    #[test]
    fn resolved_host_validation_rejects_any_private_answer() {
        let result = validate_resolved_addrs(
            "internal.example",
            &["8.8.8.8".parse().unwrap(), "192.168.1.20".parse().unwrap()],
            FetchPolicy::default(),
        );
        assert!(result.unwrap_err().contains("192.168.1.20"));
        assert!(validate_resolved_addrs(
            "public.example",
            &["8.8.8.8".parse().unwrap()],
            FetchPolicy::default(),
        )
        .is_ok());
    }
}
