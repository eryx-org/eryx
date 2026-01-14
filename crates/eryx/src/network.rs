//! Built-in network callback with security controls.
//!
//! This module provides a `fetch` callback that Python code can use to make
//! HTTP requests, with configurable security controls including:
//!
//! - **Host allowlist**: Restrict which hosts can be accessed using wildcard patterns
//! - **SSRF protection**: Block requests to private IP ranges by default
//! - **Timeout**: Limit request duration
//! - **Response size limit**: Prevent memory exhaustion from large responses
//! - **Method restrictions**: Control which HTTP methods are allowed
//!
//! # Example
//!
//! ```rust,ignore
//! use eryx::{Sandbox, NetworkConfig, HttpMethod};
//! use std::time::Duration;
//!
//! let config = NetworkConfig::builder()
//!     .allowed_hosts(vec!["api.example.com", "*.trusted.org"])
//!     .timeout(Duration::from_secs(10))
//!     .max_response_bytes(1024 * 1024) // 1MB
//!     .allowed_methods(vec![HttpMethod::Get, HttpMethod::Post])
//!     .build();
//!
//! let sandbox = Sandbox::embedded()
//!     .with_network(config)
//!     .build()?;
//!
//! // Python can now use `await fetch(...)`:
//! // response = await fetch("https://api.example.com/data", method="GET")
//! // print(response["status"])   # 200
//! // print(response["body"])     # Response body as string
//! // print(response["headers"])  # Response headers dict
//! ```
//!
//! # Security
//!
//! By default, private IP ranges (10.x.x.x, 172.16-31.x.x, 192.168.x.x, 127.x.x.x, etc.)
//! are blocked to prevent SSRF attacks. Use `allow_private_ips(true)` to override.

use std::{
    collections::HashMap,
    future::Future,
    net::IpAddr,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::callback::{Callback, CallbackError};
use crate::schema::Schema;

/// HTTP methods supported by the fetch callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// HTTP GET request
    Get,
    /// HTTP POST request
    Post,
    /// HTTP PUT request
    Put,
    /// HTTP DELETE request
    Delete,
    /// HTTP PATCH request
    Patch,
    /// HTTP HEAD request
    Head,
    /// HTTP OPTIONS request
    Options,
}

impl HttpMethod {
    /// Convert to reqwest method.
    fn to_reqwest(self) -> reqwest::Method {
        match self {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Put => reqwest::Method::PUT,
            HttpMethod::Delete => reqwest::Method::DELETE,
            HttpMethod::Patch => reqwest::Method::PATCH,
            HttpMethod::Head => reqwest::Method::HEAD,
            HttpMethod::Options => reqwest::Method::OPTIONS,
        }
    }

    /// Parse from string (case-insensitive).
    fn from_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "GET" => Some(HttpMethod::Get),
            "POST" => Some(HttpMethod::Post),
            "PUT" => Some(HttpMethod::Put),
            "DELETE" => Some(HttpMethod::Delete),
            "PATCH" => Some(HttpMethod::Patch),
            "HEAD" => Some(HttpMethod::Head),
            "OPTIONS" => Some(HttpMethod::Options),
            _ => None,
        }
    }
}

impl std::fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpMethod::Get => write!(f, "GET"),
            HttpMethod::Post => write!(f, "POST"),
            HttpMethod::Put => write!(f, "PUT"),
            HttpMethod::Delete => write!(f, "DELETE"),
            HttpMethod::Patch => write!(f, "PATCH"),
            HttpMethod::Head => write!(f, "HEAD"),
            HttpMethod::Options => write!(f, "OPTIONS"),
        }
    }
}

/// Configuration for the network callback.
///
/// Controls security settings for HTTP requests made from Python code.
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    /// Allowed host patterns (supports wildcards like "*.example.com").
    /// An empty list blocks all hosts. Use `["*"]` to allow all hosts.
    pub allowed_hosts: Vec<String>,
    /// Request timeout.
    pub timeout: Duration,
    /// Maximum response size in bytes.
    pub max_response_bytes: usize,
    /// Allowed HTTP methods.
    pub allowed_methods: Vec<HttpMethod>,
    /// Whether to allow requests to private IP ranges (SSRF protection).
    /// Default is `false` to prevent SSRF attacks.
    pub allow_private_ips: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            allowed_hosts: vec!["*".to_string()], // Allow all hosts by default
            timeout: Duration::from_secs(30),
            max_response_bytes: 10 * 1024 * 1024, // 10MB
            allowed_methods: vec![HttpMethod::Get, HttpMethod::Post],
            allow_private_ips: false, // Block private IPs by default (SSRF protection)
        }
    }
}

impl NetworkConfig {
    /// Create a new builder for NetworkConfig.
    #[must_use]
    pub fn builder() -> NetworkConfigBuilder {
        NetworkConfigBuilder::default()
    }
}

/// Builder for [`NetworkConfig`].
#[derive(Debug, Clone, Default)]
pub struct NetworkConfigBuilder {
    allowed_hosts: Option<Vec<String>>,
    timeout: Option<Duration>,
    max_response_bytes: Option<usize>,
    allowed_methods: Option<Vec<HttpMethod>>,
    allow_private_ips: Option<bool>,
}

impl NetworkConfigBuilder {
    /// Set allowed host patterns.
    ///
    /// Supports wildcards: `"*.example.com"` matches `api.example.com`.
    /// Use `["*"]` to allow all hosts, or an empty list to block all.
    #[must_use]
    pub fn allowed_hosts(mut self, hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.allowed_hosts = Some(hosts.into_iter().map(Into::into).collect());
        self
    }

    /// Set the request timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Set the maximum response size in bytes.
    #[must_use]
    pub fn max_response_bytes(mut self, max_bytes: usize) -> Self {
        self.max_response_bytes = Some(max_bytes);
        self
    }

    /// Set allowed HTTP methods.
    #[must_use]
    pub fn allowed_methods(mut self, methods: impl IntoIterator<Item = HttpMethod>) -> Self {
        self.allowed_methods = Some(methods.into_iter().collect());
        self
    }

    /// Allow or block requests to private IP ranges.
    ///
    /// Default is `false` to prevent SSRF attacks.
    /// Set to `true` only if you need to access internal services.
    #[must_use]
    pub fn allow_private_ips(mut self, allow: bool) -> Self {
        self.allow_private_ips = Some(allow);
        self
    }

    /// Build the NetworkConfig.
    #[must_use]
    pub fn build(self) -> NetworkConfig {
        let default = NetworkConfig::default();
        NetworkConfig {
            allowed_hosts: self.allowed_hosts.unwrap_or(default.allowed_hosts),
            timeout: self.timeout.unwrap_or(default.timeout),
            max_response_bytes: self.max_response_bytes.unwrap_or(default.max_response_bytes),
            allowed_methods: self.allowed_methods.unwrap_or(default.allowed_methods),
            allow_private_ips: self.allow_private_ips.unwrap_or(default.allow_private_ips),
        }
    }
}

/// The fetch callback implementation.
///
/// This is created internally by `SandboxBuilder::with_network()`.
#[derive(Debug)]
pub struct FetchCallback {
    config: NetworkConfig,
    client: reqwest::Client,
}

impl FetchCallback {
    /// Create a new FetchCallback with the given configuration.
    pub fn new(config: NetworkConfig) -> Result<Self, CallbackError> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| CallbackError::ExecutionFailed(format!("failed to create HTTP client: {e}")))?;

        Ok(Self { config, client })
    }

    /// Check if a host matches any of the allowed patterns.
    fn is_host_allowed(&self, host: &str) -> bool {
        if self.config.allowed_hosts.is_empty() {
            return false;
        }

        for pattern in &self.config.allowed_hosts {
            if pattern == "*" {
                return true;
            }
            if wildmatch::WildMatch::new(pattern).matches(host) {
                return true;
            }
        }
        false
    }

    /// Check if an IP address is private/internal.
    fn is_private_ip(ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(ipv4) => {
                // Loopback: 127.0.0.0/8
                if ipv4.is_loopback() {
                    return true;
                }
                // Private ranges: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
                if ipv4.is_private() {
                    return true;
                }
                // Link-local: 169.254.0.0/16
                if ipv4.is_link_local() {
                    return true;
                }
                // Broadcast: 255.255.255.255
                if ipv4.is_broadcast() {
                    return true;
                }
                // Unspecified: 0.0.0.0
                if ipv4.is_unspecified() {
                    return true;
                }
                // Documentation: 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24
                let octets = ipv4.octets();
                if (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
                    || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
                    || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
                {
                    return true;
                }
                false
            }
            IpAddr::V6(ipv6) => {
                // Loopback: ::1
                if ipv6.is_loopback() {
                    return true;
                }
                // Unspecified: ::
                if ipv6.is_unspecified() {
                    return true;
                }
                // Unique local addresses (ULA): fc00::/7
                let segments = ipv6.segments();
                if (segments[0] & 0xfe00) == 0xfc00 {
                    return true;
                }
                // Link-local: fe80::/10
                if (segments[0] & 0xffc0) == 0xfe80 {
                    return true;
                }
                false
            }
        }
    }

    /// Validate a URL and check security constraints.
    async fn validate_url(&self, url_str: &str) -> Result<reqwest::Url, CallbackError> {
        let url = reqwest::Url::parse(url_str)
            .map_err(|e| CallbackError::InvalidArguments(format!("invalid URL: {e}")))?;

        // Only allow http and https
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(CallbackError::InvalidArguments(format!(
                "unsupported URL scheme: {}. Only http and https are allowed",
                url.scheme()
            )));
        }

        // Get the host
        let host = url
            .host_str()
            .ok_or_else(|| CallbackError::InvalidArguments("URL has no host".to_string()))?;

        // Check against allowlist
        if !self.is_host_allowed(host) {
            return Err(CallbackError::InvalidArguments(format!(
                "host '{}' is not in the allowed list",
                host
            )));
        }

        // Check for private IPs (SSRF protection)
        if !self.config.allow_private_ips {
            // Try to parse the host as an IP address directly
            if let Ok(ip) = host.parse::<IpAddr>()
                && Self::is_private_ip(ip)
            {
                return Err(CallbackError::InvalidArguments(format!(
                    "requests to private IP addresses are blocked: {}",
                    ip
                )));
            }

            // DNS resolution to check for private IPs
            // We do a DNS lookup to ensure the resolved IPs are not private
            let socket_addr = format!("{}:{}", host, url.port_or_known_default().unwrap_or(80));
            if let Ok(addrs) = tokio::net::lookup_host(&socket_addr).await {
                for addr in addrs {
                    if Self::is_private_ip(addr.ip()) {
                        return Err(CallbackError::InvalidArguments(format!(
                            "host '{}' resolves to private IP address: {}",
                            host,
                            addr.ip()
                        )));
                    }
                }
            }
        }

        Ok(url)
    }
}

/// Arguments for the fetch callback.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchArgs {
    /// The URL to fetch.
    pub url: String,
    /// HTTP method (default: GET).
    #[serde(default = "default_method")]
    pub method: String,
    /// Request headers as a dictionary.
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    /// Request body (for POST, PUT, PATCH).
    #[serde(default)]
    pub body: Option<String>,
}

fn default_method() -> String {
    "GET".to_string()
}

impl Callback for FetchCallback {
    fn name(&self) -> &str {
        "fetch"
    }

    fn description(&self) -> &str {
        "Make an HTTP request. Returns a dict with 'status', 'headers', and 'body' keys."
    }

    fn parameters_schema(&self) -> Schema {
        Schema::for_type::<FetchArgs>()
    }

    fn invoke(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            // Parse arguments
            let args: FetchArgs = serde_json::from_value(args)
                .map_err(|e| CallbackError::InvalidArguments(e.to_string()))?;

            // Parse and validate the method
            let method = HttpMethod::from_str(&args.method).ok_or_else(|| {
                CallbackError::InvalidArguments(format!("unsupported HTTP method: {}", args.method))
            })?;

            // Check if method is allowed
            if !self.config.allowed_methods.contains(&method) {
                return Err(CallbackError::InvalidArguments(format!(
                    "HTTP method {} is not allowed. Allowed methods: {:?}",
                    method, self.config.allowed_methods
                )));
            }

            // Validate URL (host allowlist, SSRF protection)
            let url = self.validate_url(&args.url).await?;

            // Log the request
            tracing::info!(
                url = %url,
                method = %method,
                "network callback: making HTTP request"
            );

            // Build the request
            let mut request = self.client.request(method.to_reqwest(), url);

            // Add headers
            if let Some(headers) = args.headers {
                for (key, value) in headers {
                    request = request.header(&key, &value);
                }
            }

            // Add body for appropriate methods
            if let Some(body) = args.body {
                match method {
                    HttpMethod::Post | HttpMethod::Put | HttpMethod::Patch => {
                        request = request.body(body);
                    }
                    _ => {
                        tracing::warn!(
                            method = %method,
                            "body provided for HTTP method that typically doesn't have a body"
                        );
                    }
                }
            }

            // Execute the request
            let response = request.send().await.map_err(|e| {
                if e.is_timeout() {
                    CallbackError::Timeout
                } else {
                    CallbackError::ExecutionFailed(format!("HTTP request failed: {e}"))
                }
            })?;

            // Get status and headers before consuming the response
            let status = response.status().as_u16();
            let headers: HashMap<String, String> = response
                .headers()
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str()
                        .ok()
                        .map(|val| (k.as_str().to_string(), val.to_string()))
                })
                .collect();

            // Check content-length before reading body
            if let Some(content_length) = response.content_length()
                && content_length > self.config.max_response_bytes as u64
            {
                return Err(CallbackError::ExecutionFailed(format!(
                    "response too large: {} bytes (max: {} bytes)",
                    content_length, self.config.max_response_bytes
                )));
            }

            // Read the body with size limit
            let body_bytes = response.bytes().await.map_err(|e| {
                CallbackError::ExecutionFailed(format!("failed to read response body: {e}"))
            })?;

            if body_bytes.len() > self.config.max_response_bytes {
                return Err(CallbackError::ExecutionFailed(format!(
                    "response too large: {} bytes (max: {} bytes)",
                    body_bytes.len(),
                    self.config.max_response_bytes
                )));
            }

            // Convert body to string (best effort)
            let body = String::from_utf8_lossy(&body_bytes).into_owned();

            tracing::info!(
                status = status,
                body_size = body_bytes.len(),
                "network callback: HTTP request completed"
            );

            Ok(json!({
                "status": status,
                "headers": headers,
                "body": body,
            }))
        })
    }
}

/// Create a FetchCallback wrapped in an Arc for use with SandboxBuilder.
pub fn create_fetch_callback(config: NetworkConfig) -> Result<Arc<dyn Callback>, CallbackError> {
    Ok(Arc::new(FetchCallback::new(config)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==========================================================================
    // HttpMethod tests
    // ==========================================================================

    #[test]
    fn http_method_from_str_case_insensitive() {
        assert_eq!(HttpMethod::from_str("get"), Some(HttpMethod::Get));
        assert_eq!(HttpMethod::from_str("GET"), Some(HttpMethod::Get));
        assert_eq!(HttpMethod::from_str("Get"), Some(HttpMethod::Get));
        assert_eq!(HttpMethod::from_str("POST"), Some(HttpMethod::Post));
        assert_eq!(HttpMethod::from_str("put"), Some(HttpMethod::Put));
        assert_eq!(HttpMethod::from_str("DELETE"), Some(HttpMethod::Delete));
        assert_eq!(HttpMethod::from_str("patch"), Some(HttpMethod::Patch));
        assert_eq!(HttpMethod::from_str("HEAD"), Some(HttpMethod::Head));
        assert_eq!(HttpMethod::from_str("options"), Some(HttpMethod::Options));
    }

    #[test]
    fn http_method_from_str_invalid() {
        assert_eq!(HttpMethod::from_str("INVALID"), None);
        assert_eq!(HttpMethod::from_str(""), None);
        assert_eq!(HttpMethod::from_str("CONNECT"), None);
    }

    #[test]
    fn http_method_display() {
        assert_eq!(HttpMethod::Get.to_string(), "GET");
        assert_eq!(HttpMethod::Post.to_string(), "POST");
        assert_eq!(HttpMethod::Put.to_string(), "PUT");
        assert_eq!(HttpMethod::Delete.to_string(), "DELETE");
        assert_eq!(HttpMethod::Patch.to_string(), "PATCH");
        assert_eq!(HttpMethod::Head.to_string(), "HEAD");
        assert_eq!(HttpMethod::Options.to_string(), "OPTIONS");
    }

    // ==========================================================================
    // NetworkConfig tests
    // ==========================================================================

    #[test]
    fn network_config_default() {
        let config = NetworkConfig::default();
        assert_eq!(config.allowed_hosts, vec!["*"]);
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert_eq!(config.max_response_bytes, 10 * 1024 * 1024);
        assert_eq!(config.allowed_methods, vec![HttpMethod::Get, HttpMethod::Post]);
        assert!(!config.allow_private_ips);
    }

    #[test]
    fn network_config_builder() {
        let config = NetworkConfig::builder()
            .allowed_hosts(vec!["api.example.com", "*.trusted.org"])
            .timeout(Duration::from_secs(10))
            .max_response_bytes(1024)
            .allowed_methods(vec![HttpMethod::Get])
            .allow_private_ips(true)
            .build();

        assert_eq!(config.allowed_hosts, vec!["api.example.com", "*.trusted.org"]);
        assert_eq!(config.timeout, Duration::from_secs(10));
        assert_eq!(config.max_response_bytes, 1024);
        assert_eq!(config.allowed_methods, vec![HttpMethod::Get]);
        assert!(config.allow_private_ips);
    }

    #[test]
    fn network_config_builder_partial() {
        let config = NetworkConfig::builder()
            .timeout(Duration::from_secs(5))
            .build();

        // Only timeout changed, rest is default
        assert_eq!(config.timeout, Duration::from_secs(5));
        assert_eq!(config.allowed_hosts, vec!["*"]);
        assert_eq!(config.max_response_bytes, 10 * 1024 * 1024);
    }

    // ==========================================================================
    // Host allowlist tests
    // ==========================================================================

    #[test]
    fn host_allowlist_wildcard_all() {
        let callback = FetchCallback::new(NetworkConfig::default()).unwrap();
        assert!(callback.is_host_allowed("example.com"));
        assert!(callback.is_host_allowed("api.example.com"));
        assert!(callback.is_host_allowed("any.host.here"));
    }

    #[test]
    fn host_allowlist_empty_blocks_all() {
        let config = NetworkConfig::builder()
            .allowed_hosts(Vec::<String>::new())
            .build();
        let callback = FetchCallback::new(config).unwrap();
        assert!(!callback.is_host_allowed("example.com"));
        assert!(!callback.is_host_allowed("any.host"));
    }

    #[test]
    fn host_allowlist_exact_match() {
        let config = NetworkConfig::builder()
            .allowed_hosts(vec!["api.example.com"])
            .build();
        let callback = FetchCallback::new(config).unwrap();
        assert!(callback.is_host_allowed("api.example.com"));
        assert!(!callback.is_host_allowed("example.com"));
        assert!(!callback.is_host_allowed("other.example.com"));
    }

    #[test]
    fn host_allowlist_wildcard_subdomain() {
        let config = NetworkConfig::builder()
            .allowed_hosts(vec!["*.example.com"])
            .build();
        let callback = FetchCallback::new(config).unwrap();
        assert!(callback.is_host_allowed("api.example.com"));
        assert!(callback.is_host_allowed("sub.example.com"));
        // Note: wildmatch treats *.example.com as matching at least one character
        assert!(!callback.is_host_allowed("example.com"));
    }

    #[test]
    fn host_allowlist_multiple_patterns() {
        let config = NetworkConfig::builder()
            .allowed_hosts(vec!["api.example.com", "*.trusted.org", "specific.host"])
            .build();
        let callback = FetchCallback::new(config).unwrap();
        assert!(callback.is_host_allowed("api.example.com"));
        assert!(callback.is_host_allowed("any.trusted.org"));
        assert!(callback.is_host_allowed("specific.host"));
        assert!(!callback.is_host_allowed("untrusted.com"));
    }

    // ==========================================================================
    // Private IP detection tests
    // ==========================================================================

    #[test]
    fn is_private_ip_loopback() {
        assert!(FetchCallback::is_private_ip("127.0.0.1".parse().unwrap()));
        assert!(FetchCallback::is_private_ip("127.255.255.255".parse().unwrap()));
        assert!(FetchCallback::is_private_ip("::1".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_private_ranges() {
        // 10.0.0.0/8
        assert!(FetchCallback::is_private_ip("10.0.0.1".parse().unwrap()));
        assert!(FetchCallback::is_private_ip("10.255.255.255".parse().unwrap()));
        // 172.16.0.0/12
        assert!(FetchCallback::is_private_ip("172.16.0.1".parse().unwrap()));
        assert!(FetchCallback::is_private_ip("172.31.255.255".parse().unwrap()));
        // 192.168.0.0/16
        assert!(FetchCallback::is_private_ip("192.168.0.1".parse().unwrap()));
        assert!(FetchCallback::is_private_ip("192.168.255.255".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_link_local() {
        assert!(FetchCallback::is_private_ip("169.254.0.1".parse().unwrap()));
        assert!(FetchCallback::is_private_ip("169.254.255.255".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_public() {
        assert!(!FetchCallback::is_private_ip("8.8.8.8".parse().unwrap()));
        assert!(!FetchCallback::is_private_ip("1.1.1.1".parse().unwrap()));
        assert!(!FetchCallback::is_private_ip("93.184.216.34".parse().unwrap()));
        assert!(!FetchCallback::is_private_ip("2607:f8b0:4004:800::200e".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_ipv6_private() {
        // Unique local addresses (fc00::/7)
        assert!(FetchCallback::is_private_ip("fc00::1".parse().unwrap()));
        assert!(FetchCallback::is_private_ip("fd00::1".parse().unwrap()));
        // Link-local (fe80::/10)
        assert!(FetchCallback::is_private_ip("fe80::1".parse().unwrap()));
    }

    // ==========================================================================
    // FetchCallback trait implementation tests
    // ==========================================================================

    #[test]
    fn fetch_callback_name() {
        let callback = FetchCallback::new(NetworkConfig::default()).unwrap();
        assert_eq!(callback.name(), "fetch");
    }

    #[test]
    fn fetch_callback_description() {
        let callback = FetchCallback::new(NetworkConfig::default()).unwrap();
        assert!(callback.description().contains("HTTP"));
    }

    #[test]
    fn fetch_callback_schema_has_url() {
        let callback = FetchCallback::new(NetworkConfig::default()).unwrap();
        let schema = callback.parameters_schema();
        let value = schema.to_value();
        let properties = value.get("properties").unwrap();
        assert!(properties.get("url").is_some());
    }

    // ==========================================================================
    // Validation tests (async)
    // ==========================================================================

    #[tokio::test]
    async fn validate_url_rejects_invalid_scheme() {
        let callback = FetchCallback::new(NetworkConfig::default()).unwrap();
        let result = callback.validate_url("ftp://example.com/file").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unsupported URL scheme"));
    }

    #[tokio::test]
    async fn validate_url_rejects_blocked_host() {
        let config = NetworkConfig::builder()
            .allowed_hosts(vec!["api.example.com"])
            .build();
        let callback = FetchCallback::new(config).unwrap();
        let result = callback.validate_url("https://blocked.com/path").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not in the allowed list"));
    }

    #[tokio::test]
    async fn validate_url_accepts_allowed_host() {
        let config = NetworkConfig::builder()
            .allowed_hosts(vec!["api.example.com"])
            .allow_private_ips(true) // Skip SSRF check for this test
            .build();
        let callback = FetchCallback::new(config).unwrap();
        let result = callback.validate_url("https://api.example.com/path").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn validate_url_rejects_private_ip_direct() {
        let config = NetworkConfig::builder()
            .allowed_hosts(vec!["*"])
            .allow_private_ips(false)
            .build();
        let callback = FetchCallback::new(config).unwrap();
        let result = callback.validate_url("http://127.0.0.1/path").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("private IP"));
    }

    #[tokio::test]
    async fn validate_url_allows_private_ip_when_enabled() {
        let config = NetworkConfig::builder()
            .allowed_hosts(vec!["*"])
            .allow_private_ips(true)
            .build();
        let callback = FetchCallback::new(config).unwrap();
        let result = callback.validate_url("http://127.0.0.1/path").await;
        assert!(result.is_ok());
    }

    // ==========================================================================
    // Method validation tests
    // ==========================================================================

    #[tokio::test]
    async fn invoke_rejects_disallowed_method() {
        let config = NetworkConfig::builder()
            .allowed_methods(vec![HttpMethod::Get])
            .build();
        let callback = FetchCallback::new(config).unwrap();

        let result = callback
            .invoke(json!({
                "url": "https://example.com",
                "method": "POST"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not allowed"));
    }
}
