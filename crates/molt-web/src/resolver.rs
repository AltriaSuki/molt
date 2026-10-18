//! Public-address DNS, with an explicit HTTPS resolver independent of host DNS.
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;
use serde_json::Value;

use crate::{public, Allow};

const DNS_TIMEOUT: Duration = Duration::from_secs(5);
const DNS_BYTES: usize = 64 * 1024;
const PREFLIGHT_HOSTS: usize = 16;

#[derive(Clone)]
pub(crate) enum Resolver {
    System,
    Doh(Doh),
}

#[derive(Clone)]
pub(crate) struct Doh {
    endpoint: Url,
    client: reqwest::Client,
}

fn endpoint(spec: &str) -> Result<Option<Url>, String> {
    if spec == "system" {
        return Ok(None);
    }
    let url = spec.strip_prefix("doh:").ok_or("web resolver must be system or doh:https://PUBLIC_IP/dns-query")?;
    let url = Url::parse(url).map_err(|_| "invalid DoH resolver URL")?;
    let ip = url.host_str().and_then(|host| host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().ok());
    if url.scheme() != "https"
        || !ip.is_some_and(public)
        || url.port().is_some_and(|port| port != 443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("DoH resolver must use HTTPS on port 443 at a public IP, without credentials, query or fragment; a literal IP avoids fake-IP bootstrap DNS".into());
    }
    Ok(Some(url))
}

pub fn validate_resolver(spec: &str) -> Result<(), String> {
    endpoint(spec).map(|_| ())
}

impl Resolver {
    pub(crate) fn new(spec: &str) -> anyhow::Result<Self> {
        let Some(endpoint) = endpoint(spec).map_err(anyhow::Error::msg)? else { return Ok(Self::System) };
        let client = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(DNS_TIMEOUT)
            .connect_timeout(Duration::from_secs(3))
            .user_agent(concat!("molt-web-dns/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self::Doh(Doh { endpoint, client }))
    }

    pub(crate) async fn lookup(&self, host: &str) -> Result<Vec<SocketAddr>, String> {
        if host.len() > 253 || host.split('.').any(|label| label.is_empty() || label.len() > 63) {
            return Err("invalid DNS host name".into());
        }
        match self {
            Self::System => {
                let addresses = tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host, 443)))
                    .await
                    .map_err(|_| format!("DNS lookup of {host} timed out"))?
                    .map_err(|error| format!("DNS lookup of {host}: {error}"))?;
                public_addresses(host, addresses.map(|address| address.ip()).collect())
            }
            Self::Doh(doh) => lookup_doh(host, doh).await,
        }
    }
}

pub(crate) fn public_addresses(host: &str, found: Vec<IpAddr>) -> Result<Vec<SocketAddr>, String> {
    let mut addresses: Vec<_> =
        found.iter().copied().filter(|&ip| public(ip)).map(|ip| SocketAddr::new(ip, 443)).collect();
    addresses.sort_unstable();
    addresses.dedup();
    if !addresses.is_empty() {
        return Ok(addresses);
    }
    if found.is_empty() {
        return Err(format!("{host} has no address"));
    }
    let fake = found.iter().any(|ip| match ip {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            a == 198 && (18..20).contains(&b)
        }
        _ => false,
    });
    let hint = if fake {
        "fake-IP DNS (198.18.0.0/15), often from a Clash/mihomo TUN, was detected"
    } else {
        "host DNS returned only private or reserved addresses"
    };
    Err(format!("{}{host} resolves only to addresses that are not public; {hint}. This is a host network configuration problem: changing the URL will not fix it. Do not retry web URLs; ask the user to change DNS or explicitly use --web-resolver doh:https://1.1.1.1/dns-query. Private addresses remain blocked", molt_api::web::ENVIRONMENT_UNAVAILABLE))
}

#[async_trait]
trait DnsQuery: Send + Sync {
    async fn query(&self, host: &str, kind: u16) -> Result<Vec<IpAddr>, String>;
}

async fn lookup_doh(host: &str, query: &dyn DnsQuery) -> Result<Vec<SocketAddr>, String> {
    let (a, aaaa) = tokio::join!(query.query(host, 1), query.query(host, 28));
    if let (Err(a), Err(aaaa)) = (&a, &aaaa) {
        return Err(format!("DoH lookup of {host} failed: {a}; {aaaa}"));
    }
    let found = a.into_iter().chain(aaaa).flatten().collect();
    public_addresses(host, found)
}

impl Doh {
    fn request(&self, host: &str, kind: u16) -> Result<reqwest::Request, String> {
        let mut url = self.endpoint.clone();
        url.query_pairs_mut().append_pair("name", host).append_pair("type", if kind == 1 { "A" } else { "AAAA" });
        self.client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/dns-json")
            .build()
            .map_err(|error| error.to_string())
    }
}

#[async_trait]
impl DnsQuery for Doh {
    async fn query(&self, host: &str, kind: u16) -> Result<Vec<IpAddr>, String> {
        let mut response = self.client.execute(self.request(host, kind)?).await.map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("DoH HTTP status {}", response.status()));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
            if bytes.len() + chunk.len() > DNS_BYTES {
                return Err("DoH response exceeds 64 KiB".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        answers(&bytes, kind)
    }
}

fn answers(bytes: &[u8], kind: u16) -> Result<Vec<IpAddr>, String> {
    if bytes.len() > DNS_BYTES {
        return Err("DoH response exceeds 64 KiB".into());
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| "invalid DoH JSON response")?;
    if value.get("Status").and_then(Value::as_u64) != Some(0) || value.get("TC").and_then(Value::as_bool) == Some(true)
    {
        return Err("DoH returned a DNS error or truncated answer".into());
    }
    let Some(records) = value.get("Answer") else { return Ok(Vec::new()) };
    let records = records.as_array().ok_or("invalid DoH answer list")?;
    if records.len() > 256 {
        return Err("DoH returned more than 256 records".into());
    }
    let mut addresses = Vec::new();
    for record in records {
        if record.get("type").and_then(Value::as_u64) != Some(u64::from(kind)) {
            continue;
        }
        let ip: IpAddr = record
            .get("data")
            .and_then(Value::as_str)
            .ok_or("missing DoH address")?
            .parse()
            .map_err(|_| "invalid DoH address")?;
        if (kind == 1 && ip.is_ipv4()) || (kind == 28 && ip.is_ipv6()) {
            addresses.push(ip);
        }
    }
    Ok(addresses)
}

#[async_trait]
trait HostLookup: Send + Sync {
    async fn lookup(&self, host: &str) -> Result<Vec<SocketAddr>, String>;
}
#[async_trait]
impl HostLookup for Resolver {
    async fn lookup(&self, host: &str) -> Result<Vec<SocketAddr>, String> {
        Resolver::lookup(self, host).await
    }
}

/// Probe a bounded set of exact hosts (wildcards use their apex), before model work.
pub async fn preflight(allow: &Allow, spec: &str) -> anyhow::Result<Vec<String>> {
    preflight_with(allow, Arc::new(Resolver::new(spec)?)).await
}

async fn preflight_with(allow: &Allow, resolver: Arc<dyn HostLookup>) -> anyhow::Result<Vec<String>> {
    let hosts = allow.probe_hosts();
    let host_count = hosts.len();
    let mut work = tokio::task::JoinSet::new();
    for host in hosts.into_iter().take(PREFLIGHT_HOSTS) {
        let resolver = resolver.clone();
        work.spawn(async move { resolver.lookup(&host).await.err() });
    }
    let mut warnings = Vec::new();
    while let Some(result) = work.join_next().await {
        match result {
            Ok(Some(error)) => warnings.push(error),
            Ok(None) => {}
            Err(error) => warnings.push(format!("DNS preflight could not finish: {error}")),
        }
    }
    if host_count > PREFLIGHT_HOSTS {
        warnings.push(format!("DNS preflight checked the first {PREFLIGHT_HOSTS} hosts; every fetch still checks its actual public addresses"));
    }
    warnings.sort();
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolver_configuration_cannot_use_private_addresses_credentials_or_redirect_bootstrap() {
        assert!(validate_resolver("system").is_ok());
        assert!(validate_resolver("doh:https://1.1.1.1/dns-query").is_ok());
        for spec in [
            "doh:http://1.1.1.1/dns-query",
            "doh:https://127.0.0.1/dns-query",
            "doh:https://198.18.0.1/dns-query",
            "doh:https://dns.example.org/dns-query",
            "doh:https://user:secret@1.1.1.1/dns-query",
            "doh:https://1.1.1.1:8443/dns-query",
            "doh:https://1.1.1.1/dns-query?key=secret",
        ] {
            assert!(validate_resolver(spec).is_err(), "{spec}");
        }
    }

    #[test]
    fn fake_ip_errors_explain_the_host_problem_and_public_addresses_are_still_required() {
        let error = public_addresses("docs.rs", vec!["198.18.0.127".parse().unwrap()]).unwrap_err();
        assert!(
            error.starts_with(molt_api::web::ENVIRONMENT_UNAVAILABLE)
                && error.contains("fake-IP DNS")
                && error.contains("Do not retry")
        );
        for ip in ["127.0.0.1", "10.0.0.1", "::1", "::ffff:127.0.0.1"] {
            assert!(public_addresses("docs.rs", vec![ip.parse().unwrap()]).is_err());
        }
        assert_eq!(
            public_addresses("docs.rs", vec!["10.0.0.1".parse().unwrap(), "93.184.216.34".parse().unwrap()]).unwrap(),
            ["93.184.216.34:443".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn doh_requests_use_the_pinned_https_endpoint_and_bound_and_validate_answers() {
        let Resolver::Doh(doh) = Resolver::new("doh:https://1.1.1.1/dns-query").unwrap() else { panic!() };
        let request = doh.request("docs.rs", 1).unwrap();
        assert_eq!(request.url().host_str(), Some("1.1.1.1"));
        assert_eq!(request.url().query(), Some("name=docs.rs&type=A"));
        assert_eq!(request.headers()[reqwest::header::ACCEPT], "application/dns-json");
        let response =
            json!({"Status":0,"Answer":[{"type":5,"data":"cdn.example.org"},{"type":1,"data":"93.184.216.34"}]});
        assert_eq!(
            answers(&serde_json::to_vec(&response).unwrap(), 1).unwrap(),
            ["93.184.216.34".parse::<IpAddr>().unwrap()]
        );
        for response in [
            json!({"Status":2}),
            json!({"Status":0,"TC":true}),
            json!({"Status":0,"Answer":[{"type":1,"data":"bad address"}]}),
            json!({"Status":0,"Answer":{}}),
        ] {
            assert!(answers(&serde_json::to_vec(&response).unwrap(), 1).is_err());
        }
        assert!(answers(&vec![b' '; DNS_BYTES + 1], 1).is_err());
    }

    struct FakeHostDns;
    #[async_trait]
    impl HostLookup for FakeHostDns {
        async fn lookup(&self, host: &str) -> Result<Vec<SocketAddr>, String> {
            public_addresses(host, vec!["198.18.0.127".parse().unwrap()])
        }
    }
    #[tokio::test]
    async fn preflight_reports_fake_ip_before_work_and_bounds_host_probes() {
        let allow = Allow::parse(&["docs.rs", "*.python.org"]).unwrap();
        let warnings = preflight_with(&allow, Arc::new(FakeHostDns)).await.unwrap();
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().all(|warning| warning.contains("fake-IP DNS") && warning.contains("--web-resolver")));
        let hosts: Vec<_> = (0..20).map(|i| format!("h{i}.example.org")).collect();
        let warnings = preflight_with(&Allow::parse(&hosts).unwrap(), Arc::new(FakeHostDns)).await.unwrap();
        assert_eq!(warnings.len(), PREFLIGHT_HOSTS + 1);
    }

    struct FakeDoh {
        private: bool,
    }
    #[async_trait]
    impl DnsQuery for FakeDoh {
        async fn query(&self, host: &str, kind: u16) -> Result<Vec<IpAddr>, String> {
            assert_eq!(host, "docs.rs");
            let data = if self.private { "198.18.0.127" } else { "93.184.216.34" };
            answers(&serde_json::to_vec(&json!({"Status":0,"Answer":[{"type":1,"data":data}]})).unwrap(), kind)
        }
    }
    #[tokio::test]
    async fn an_explicit_doh_answer_replaces_fake_ip_dns_without_allowing_private_answers() {
        assert!(public_addresses("docs.rs", vec!["198.18.0.127".parse().unwrap()]).is_err());
        assert_eq!(
            lookup_doh("docs.rs", &FakeDoh { private: false }).await.unwrap(),
            ["93.184.216.34:443".parse::<SocketAddr>().unwrap()]
        );
        assert!(lookup_doh("docs.rs", &FakeDoh { private: true }).await.unwrap_err().contains("fake-IP"));
    }
}
