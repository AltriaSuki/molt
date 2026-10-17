//! Which URLs may be fetched, and which addresses connected to.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use reqwest::Url;

/// One allowed host: `docs.rs` is that host only, `*.python.org` any host
/// below `python.org` (not `python.org` itself).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pattern {
    Exact(String),
    Below(String),
}

/// The hosts pages may come from. Empty allows nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allow(Vec<Pattern>);

impl Allow {
    /// Parse `entries`, each `host.name` or `*.host.name`.
    pub fn parse<S: AsRef<str>>(entries: &[S]) -> Result<Self, String> {
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.as_ref().trim().trim_end_matches('.').to_ascii_lowercase();
            let (below, host) = match entry.strip_prefix("*.") {
                Some(host) => (true, host.to_owned()),
                None => (false, entry.clone()),
            };
            let label_ok = |l: &str| {
                !l.is_empty() && !l.starts_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            };
            if !host.contains('.') || !host.split('.').all(label_ok) || host.parse::<IpAddr>().is_ok() {
                return Err(format!(
                    "{entry:?} is not a host to allow: give a name such as docs.rs, or *.python.org for the hosts \
                     below it, without a scheme, port or path"
                ));
            }
            out.push(if below { Pattern::Below(host) } else { Pattern::Exact(host) });
        }
        Ok(Self(out))
    }

    fn allows(&self, host: &str) -> bool {
        self.0.iter().any(|p| match p {
            Pattern::Exact(h) => host == h,
            Pattern::Below(h) => {
                host.strip_suffix(h.as_str()).is_some_and(|rest| rest.len() > 1 && rest.ends_with('.'))
            }
        })
    }

    /// Why `url` may not be fetched, if it may not.
    pub fn check(&self, url: &Url) -> Result<(), String> {
        if url.scheme() != "https" {
            return Err(format!("{url} is not an https URL; only https is fetched"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(format!("{url} has credentials in it"));
        }
        if url.port().is_some_and(|p| p != 443) {
            return Err(format!("{url} names a port; only the default https port is fetched"));
        }
        // `domain` is `None` for an IP address.
        let host = match (url.domain(), url.host_str()) {
            (Some(d), _) => d.trim_end_matches('.').to_ascii_lowercase(),
            (None, Some(_)) => return Err(format!("{url} names an address; only allowed host names are fetched")),
            (None, None) => return Err(format!("{url} has no host")),
        };
        if !self.allows(&host) {
            return Err(format!("{host} is not on the allowlist of hosts the user lets Molt read"));
        }
        Ok(())
    }
}

/// Whether `ip` is an address on the public internet: not loopback,
/// private, link-local, shared, reserved, documentation, multicast or
/// unspecified. Names resolving to anything else are never connected to, so
/// an allowed name cannot be pointed at the user's own machine or network.
pub fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => public_v4(v4),
            None => public_v6(v6),
        },
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        // Shared address space (carrier-grade NAT).
        || (a == 100 && (64..128).contains(&b))
        // IETF protocol assignments.
        || (a == 192 && b == 0 && c == 0)
        // Benchmarking.
        || (a == 198 && (18..20).contains(&b))
        // Reserved.
        || a >= 240)
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        // Unique local fc00::/7 and link-local fe80::/10.
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        // Documentation 2001:db8::/32.
        || (first == 0x2001 && ip.segments()[1] == 0x0db8)
        // NAT64 64:ff9b::/96 and IPv4-compatible ::/96 can reach any IPv4 address.
        || (first == 0x0064 && ip.segments()[1] == 0xff9b)
        || ip.segments()[..6].iter().all(|s| *s == 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow() -> Allow {
        Allow::parse(&["docs.rs", "*.python.org", " Developer.Mozilla.org. "]).unwrap()
    }

    fn check(url: &str) -> Result<(), String> {
        allow().check(&Url::parse(url).unwrap())
    }

    #[test]
    fn only_https_urls_on_allowed_hosts_pass() {
        assert_eq!(check("https://docs.rs/serde"), Ok(()));
        assert_eq!(check("https://DOCS.rs./serde?x=1"), Ok(()));
        assert_eq!(check("https://docs.python.org/3/library/"), Ok(()));
        assert_eq!(check("https://developer.mozilla.org/en-US/"), Ok(()));
        assert_eq!(check("https://docs.rs:443/serde"), Ok(()));
        for (url, why) in [
            ("http://docs.rs/serde", "only https"),
            ("https://evil.com/", "not on the allowlist"),
            ("https://docs.rs.evil.com/", "not on the allowlist"),
            ("https://evildocs.rs/", "not on the allowlist"),
            ("https://python.org/", "not on the allowlist"),
            ("https://notpython.org/", "not on the allowlist"),
            ("https://docs.rs:8443/", "port"),
            ("https://user:pw@docs.rs/", "credentials"),
            ("https://127.0.0.1/", "address"),
            ("https://[::1]/", "address"),
            ("https://10.0.0.1/", "address"),
            ("file:///etc/passwd", "only https"),
        ] {
            let err = check(url).unwrap_err();
            assert!(err.contains(why), "{url}: {err}");
        }
    }

    #[test]
    fn entries_must_be_host_names() {
        for bad in ["", "localhost", "https://docs.rs", "docs.rs/serde", "docs.rs:443", "127.0.0.1", "*", "*.", "a..b"]
        {
            assert!(Allow::parse(&[bad]).is_err(), "{bad:?}");
        }
        assert_eq!(Allow::parse::<&str>(&[]).unwrap(), Allow::default());
        assert!(Allow::default().check(&Url::parse("https://docs.rs/").unwrap()).is_err());
    }

    #[test]
    fn only_public_addresses_are_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "198.18.0.1",
            "240.0.0.1",
            "192.0.0.8",
            "192.0.2.1",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "fc00::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "2001:db8::1",
            "64:ff9b::a00:1",
            "::7f00:1",
            "ff02::1",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip} is not public");
        }
        for ip in ["93.184.216.34", "1.1.1.1", "2606:4700:4700::1111", "::ffff:1.1.1.1"] {
            assert!(public(ip.parse().unwrap()), "{ip} is public");
        }
    }
}
