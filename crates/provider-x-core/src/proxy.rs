use std::{env, net::IpAddr, sync::Arc};

/// Snapshot of the process proxy environment with provider-x `NO_PROXY` matching semantics.
///
/// Proxy URLs are intentionally not exposed through `Debug` because they may contain credentials.
#[derive(Clone, Default)]
pub struct ProxyEnvironment {
    http: Option<String>,
    https: Option<String>,
    no_proxy: NoProxy,
}

impl ProxyEnvironment {
    #[must_use]
    pub fn read() -> Self {
        let all = first_env(&["ALL_PROXY", "all_proxy"]);
        Self {
            http: first_env(&["HTTP_PROXY", "http_proxy"]).or_else(|| all.clone()),
            https: first_env(&["HTTPS_PROXY", "https_proxy"]).or(all),
            no_proxy: NoProxy::parse(
                first_env(&["NO_PROXY", "no_proxy"])
                    .as_deref()
                    .unwrap_or_default(),
            ),
        }
    }

    #[must_use]
    pub fn proxy_url(&self, scheme: &str) -> Option<&str> {
        match scheme {
            "http" => self.http.as_deref(),
            "https" => self.https.as_deref(),
            _ => None,
        }
    }

    #[must_use]
    pub fn should_proxy(&self, scheme: &str, host: &str, port: Option<u16>) -> bool {
        self.proxy_url(scheme).is_some() && !self.no_proxy.matches(host, port)
    }

    /// Builds a deterministic proxy snapshot. Primarily used by connector tests and explicit
    /// application wiring; values are retained in memory and never included in `Debug` output.
    #[must_use]
    pub fn from_values(http: Option<&str>, https: Option<&str>, no_proxy: &str) -> Self {
        Self {
            http: http.map(str::to_owned),
            https: https.map(str::to_owned),
            no_proxy: NoProxy::parse(no_proxy),
        }
    }
}

fn first_env(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        env::var_os(name)
            .and_then(|value| value.into_string().ok())
            .filter(|value| !value.trim().is_empty())
    })
}

#[derive(Clone, Default)]
struct NoProxy {
    all: bool,
    entries: Arc<[NoProxyEntry]>,
}

impl NoProxy {
    fn parse(value: &str) -> Self {
        let mut all = false;
        let entries = value
            .split(',')
            .filter_map(|raw| {
                let raw = raw.trim();
                if raw == "*" {
                    all = true;
                    return None;
                }
                NoProxyEntry::parse(raw)
            })
            .collect::<Vec<_>>();
        Self {
            all,
            entries: entries.into(),
        }
    }

    fn matches(&self, host: &str, port: Option<u16>) -> bool {
        is_loopback_host(host)
            || self.all
            || self.entries.iter().any(|entry| entry.matches(host, port))
    }
}

#[derive(Clone)]
struct NoProxyEntry {
    host: String,
    port: Option<u16>,
}

impl NoProxyEntry {
    fn parse(raw: &str) -> Option<Self> {
        if raw.is_empty() {
            return None;
        }
        let without_scheme = raw
            .strip_prefix("http://")
            .or_else(|| raw.strip_prefix("https://"))
            .unwrap_or(raw);
        let authority = without_scheme.split('/').next()?;
        let (host, port) = split_host_port(authority);
        let host = host.trim_matches(['[', ']']).trim_start_matches('.');
        if host.is_empty() {
            return None;
        }
        Some(Self {
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    fn matches(&self, host: &str, port: Option<u16>) -> bool {
        if self.port.is_some() && self.port != port {
            return false;
        }
        let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
        host == self.host || host.ends_with(&format!(".{}", self.host))
    }
}

fn split_host_port(authority: &str) -> (&str, Option<u16>) {
    if authority.starts_with('[') {
        if let Some((host, port)) = authority.rsplit_once("]:") {
            return (host, port.parse().ok());
        }
        return (authority, None);
    }
    authority
        .rsplit_once(':')
        .and_then(|(host, port)| port.parse().ok().map(|port| (host, Some(port))))
        .unwrap_or((authority, None))
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::ProxyEnvironment;

    #[test]
    fn loopback_always_bypasses_proxy() {
        let environment = ProxyEnvironment::from_values(
            Some("http://proxy.example:8080"),
            Some("http://proxy.example:8080"),
            "",
        );
        assert!(!environment.should_proxy("http", "localhost", Some(43119)));
        assert!(!environment.should_proxy("http", "127.0.0.1", Some(43119)));
        assert!(!environment.should_proxy("https", "::1", Some(43119)));
        assert!(environment.should_proxy("https", "chatgpt.com", Some(443)));
    }

    #[test]
    fn no_proxy_matches_domains_subdomains_ports_and_wildcard() {
        let environment = ProxyEnvironment::from_values(
            Some("http://proxy.example:8080"),
            Some("http://proxy.example:8080"),
            "example.com,api.internal:8443,.svc.local",
        );
        assert!(!environment.should_proxy("https", "example.com", Some(443)));
        assert!(!environment.should_proxy("https", "www.example.com", Some(443)));
        assert!(!environment.should_proxy("https", "api.internal", Some(8443)));
        assert!(environment.should_proxy("https", "api.internal", Some(443)));
        assert!(!environment.should_proxy("https", "models.svc.local", Some(443)));
        assert!(environment.should_proxy("https", "notexample.com", Some(443)));

        let wildcard = ProxyEnvironment::from_values(
            Some("http://proxy.example:8080"),
            Some("http://proxy.example:8080"),
            "*",
        );
        assert!(!wildcard.should_proxy("https", "chatgpt.com", Some(443)));
    }
}
