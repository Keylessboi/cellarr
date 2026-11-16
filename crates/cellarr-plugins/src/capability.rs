//! The one capability a plugin can be granted: HTTP, scoped by an allow-list.
//!
//! A guest never opens a socket. It hands the host a request; the host checks
//! the URL against the plugin's allow-list and performs the request itself (or
//! refuses). The allow-list is built from the hosts the plugin's own settings
//! name (an indexer's base URL, a download client's address) plus any its
//! manifest declares, so a plugin can reach exactly the services the user
//! pointed it at.

use crate::error::{PluginError, Result};

/// A request a guest asks the host to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// `GET`, `POST`, ….
    pub method: String,
    /// Absolute URL.
    pub url: String,
    /// Request headers.
    pub headers: Vec<(String, String)>,
    /// Request body (empty for none).
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// A bare GET.
    #[must_use]
    pub fn get(url: &str) -> Self {
        Self {
            method: "GET".into(),
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
}

/// What came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status.
    pub status: u16,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// Response body.
    pub body: Vec<u8>,
}

/// Something that can perform (or refuse) a guest's HTTP request.
pub trait HttpCapability: Send + Sync {
    /// Perform `req`, or refuse it.
    ///
    /// # Errors
    /// [`PluginError::CapabilityDenied`] when the request is not allowed, or a
    /// transport error.
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse>;

    /// Convenience: a bare GET.
    ///
    /// # Errors
    /// As [`HttpCapability::send`].
    fn fetch(&self, url: &str) -> Result<HttpResponse> {
        self.send(&HttpRequest::get(url))
    }
}

/// Refuses everything.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAllHttp;

impl HttpCapability for DenyAllHttp {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse> {
        Err(PluginError::CapabilityDenied(format!(
            "http is not granted to this plugin (attempted: {})",
            req.url
        )))
    }
}

/// Allows requests whose URL starts with one of the allowed prefixes, and hands
/// them to `transport`.
pub struct AllowListHttp<F> {
    allowed_prefixes: Vec<String>,
    transport: F,
}

impl<F> AllowListHttp<F>
where
    F: Fn(&HttpRequest) -> Result<HttpResponse> + Send + Sync,
{
    /// An allow-list over `transport`.
    pub fn new(allowed_prefixes: Vec<String>, transport: F) -> Self {
        Self {
            allowed_prefixes,
            transport,
        }
    }

    fn is_allowed(&self, url: &str) -> bool {
        // Compare on a prefix that ends at the authority, so `http://a.test`
        // does not admit `http://a.test.evil`.
        self.allowed_prefixes.iter().any(|p| {
            url.strip_prefix(p.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?', '#']))
        })
    }
}

impl<F> HttpCapability for AllowListHttp<F>
where
    F: Fn(&HttpRequest) -> Result<HttpResponse> + Send + Sync,
{
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse> {
        if !self.is_allowed(&req.url) {
            return Err(PluginError::CapabilityDenied(format!(
                "url not on this plugin's allow-list: {}",
                req.url
            )));
        }
        (self.transport)(req)
    }
}

/// The `scheme://host[:port]` origin of a URL, when it is an http(s) URL.
#[must_use]
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return None;
    }
    Some(format!("{}://{}", scheme.to_ascii_lowercase(), authority))
}

/// The origins a plugin's settings point at: every string value that is an
/// http(s) URL, plus `host`/`port`/`useSsl` style triples.
#[must_use]
pub fn origins_in_settings(settings: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(o) = settings.as_object() {
        for v in o.values() {
            if let Some(origin) = v.as_str().and_then(origin_of) {
                out.push(origin);
            }
        }
        if let Some(host) = o
            .get("host")
            .and_then(|v| v.as_str())
            .filter(|h| !h.is_empty())
        {
            let ssl = o
                .get("useSsl")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let scheme = if ssl { "https" } else { "http" };
            match o.get("port").and_then(serde_json::Value::as_u64) {
                Some(port) => out.push(format!("{scheme}://{host}:{port}")),
                None => out.push(format!("{scheme}://{host}")),
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(req: &HttpRequest) -> Result<HttpResponse> {
        Ok(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: req.url.as_bytes().to_vec(),
        })
    }

    #[test]
    fn deny_all_refuses_everything() {
        let err = DenyAllHttp
            .fetch("https://example.invalid/caps")
            .unwrap_err();
        assert!(matches!(err, PluginError::CapabilityDenied(_)));
    }

    #[test]
    fn allow_list_permits_only_its_origins() {
        let cap = AllowListHttp::new(vec!["https://api.allowed.test".to_string()], ok);
        assert_eq!(
            cap.fetch("https://api.allowed.test/search").unwrap().status,
            200
        );
        assert_eq!(cap.fetch("https://api.allowed.test").unwrap().status, 200);
        for bad in [
            "https://evil.test/exfiltrate",
            "https://api.allowed.test.evil/x",
            "https://api.allowed.testx/",
        ] {
            assert!(
                matches!(cap.fetch(bad), Err(PluginError::CapabilityDenied(_))),
                "{bad} must be denied"
            );
        }
    }

    #[test]
    fn settings_name_the_origins_a_plugin_may_reach() {
        let s = serde_json::json!({
            "baseUrl": "http://prowlarr.local:9696/1/",
            "apiKey": "k",
            "host": "qbit.lan", "port": 8080, "useSsl": false,
        });
        assert_eq!(
            origins_in_settings(&s),
            ["http://prowlarr.local:9696", "http://qbit.lan:8080"]
        );
        assert_eq!(origin_of("ftp://x/y"), None);
    }
}
