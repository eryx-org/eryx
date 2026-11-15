//! Size-capped downloads from allowed hosts.

use std::sync::Arc;

use crate::{Error, Result};

/// Fetches URLs. [`HttpFetch`] does it over HTTPS; tests supply their own.
pub trait Fetch: Send + Sync {
    /// GET `url` with `accept` as the `Accept` header, failing if the body is
    /// over `max_bytes`.
    fn get(
        &self,
        url: &str,
        accept: &str,
        max_bytes: u64,
    ) -> impl Future<Output = Result<Vec<u8>>> + Send;
}

/// Fetches over HTTPS from a fixed set of hosts, with the webpki roots.
#[derive(Debug, Clone)]
pub struct HttpFetch {
    client: reqwest::Client,
    allowed_hosts: Arc<Vec<String>>,
}

impl HttpFetch {
    /// A fetcher that only connects to `allowed_hosts` (exact, case-insensitive
    /// host names), over HTTPS, and follows no redirects to other hosts.
    ///
    /// # Errors
    ///
    /// Fails if the HTTP client can't be built.
    pub fn new(allowed_hosts: impl IntoIterator<Item = impl Into<String>>) -> Result<Self> {
        let allowed_hosts: Arc<Vec<String>> = Arc::new(
            allowed_hosts
                .into_iter()
                .map(|h| h.into().to_ascii_lowercase())
                .collect(),
        );
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let redirect_hosts = Arc::clone(&allowed_hosts);
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .https_only(true)
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                let allowed = attempt
                    .url()
                    .host_str()
                    .is_some_and(|h| redirect_hosts.iter().any(|a| a.eq_ignore_ascii_case(h)));
                if attempt.previous().len() >= 5 {
                    attempt.error("too many redirects")
                } else if allowed {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .user_agent(concat!("eryx-packages/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Fetch {
                url: String::new(),
                message: format!("building the HTTP client: {e}"),
            })?;
        Ok(Self {
            client,
            allowed_hosts,
        })
    }

    fn check_host(&self, url: &str) -> Result<reqwest::Url> {
        let refused = |message: &str| Error::Fetch {
            url: url.to_string(),
            message: message.to_string(),
        };
        let parsed = reqwest::Url::parse(url).map_err(|_| refused("not a valid URL"))?;
        if parsed.scheme() != "https" {
            return Err(refused("only https URLs are fetched"));
        }
        let host = parsed.host_str().unwrap_or_default();
        if !self
            .allowed_hosts
            .iter()
            .any(|a| a.eq_ignore_ascii_case(host))
        {
            return Err(refused("host is not an allowed registry host"));
        }
        Ok(parsed)
    }
}

impl Fetch for HttpFetch {
    async fn get(&self, url: &str, accept: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let parsed = self.check_host(url)?;
        let fail = |message: String| Error::Fetch {
            url: url.to_string(),
            message,
        };
        let response = self
            .client
            .get(parsed)
            .header(reqwest::header::ACCEPT, accept)
            .send()
            .await
            .map_err(|e| fail(e.to_string()))?;
        if !response.status().is_success() {
            return Err(fail(format!("HTTP {}", response.status())));
        }
        if response.content_length().is_some_and(|len| len > max_bytes) {
            return Err(Error::Limit(format!("{url} is over {max_bytes} bytes")));
        }
        let mut response = response;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| fail(e.to_string()))? {
            if body.len() as u64 + chunk.len() as u64 > max_bytes {
                return Err(Error::Limit(format!("{url} is over {max_bytes} bytes")));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refuses_other_hosts_and_plain_http_before_connecting() {
        let fetch = HttpFetch::new(["registry.npmjs.org"]).unwrap();
        for url in [
            "http://registry.npmjs.org/zod",
            "https://evil.example/zod",
            "https://registry.npmjs.org.evil.example/zod",
            "not a url",
        ] {
            let err = fetch.get(url, "*/*", 10).await.unwrap_err();
            assert!(matches!(err, Error::Fetch { .. }), "{url}: {err}");
        }
    }
}
