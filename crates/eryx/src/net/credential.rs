//! Credentials the child proxy adds to child processes' requests.
//!
//! A [`Credential`] is a header the host gives some commands' requests to
//! some hosts, built from a secret the guest never sees: not as a value, and
//! not as a placeholder. The child proxy adds it to each matching request it
//! inspects (replacing any header of that name the child sent), and masks it
//! out of the responses. The guest's own requests (`eryx:net`) never get it.

use base64::Engine as _;
use base64::prelude::BASE64_STANDARD;

use super::{host_matches_pattern, is_normalized_path, normalize_host};

/// What a masked credential reads as in a response.
const MASKED: &[u8] = b"[REDACTED]";

/// Headers a credential can't set: the ones that frame the request or route
/// it, or that the proxy itself handles.
const RESERVED_HEADERS: [&str; 9] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "upgrade",
    "te",
    "trailer",
    "keep-alive",
    "accept-encoding",
];

/// A header the child proxy adds to child processes' requests to some
/// hosts, built from a secret the sandbox never sees.
///
/// ```rust,ignore
/// let gh = Credential::bearer(token).for_hosts(["api.github.com"]).for_commands(["gh"]);
/// let git = Credential::basic("x-access-token", token)
///     .for_hosts(["github.com"])
///     .for_paths(["/my-org/"])
///     .for_commands(["git"]);
/// ```
///
/// Give it to a sandbox with
/// [`SandboxBuilder::with_credential`](crate::SandboxBuilder::with_credential),
/// or to one execution with
/// [`ExecuteOptions::with_credential`](crate::ExecuteOptions::with_credential).
///
/// The proxy adds it only to requests it inspects: over TLS it terminated
/// (not blind tunnels, which can't reach its hosts at all) or, with
/// [`allow_plaintext`](Self::allow_plaintext), plain HTTP; never to an
/// `Upgrade` request. Any header of the same name the child sent is
/// replaced, so a client that needs a token to send requests at all (like
/// `gh`) can be given a dummy one. Responses on such a connection are asked
/// for and must come uncompressed, and have the credential masked out; a
/// request that gets it loses its `Range` and `If-Range` headers.
///
/// Where an execution's credential and the sandbox's both apply to a request
/// and set the same header, the execution's wins.
///
/// Masking only hides whole, verbatim echoes of the value (and of a `Basic`
/// payload, and the secret): not pieces of it, nor forms a host re-escapes
/// or re-encodes it into (JSON's `\/` for `/`, say). So don't give
/// credentials for hosts that echo requests. The value and the proxy's
/// password are also scrubbed from the output and traces, but that's defence
/// in depth, not a boundary: a script that gets hold of either can transform
/// it first. [`for_hosts`](Self::for_hosts) doesn't scope by port.
///
/// A command given a credential can do anything with it that its hosts and
/// paths allow, on the guest's behalf: scope it as narrowly as you can.
#[derive(Clone)]
pub struct Credential {
    header: String,
    /// The header's value, with the secret in it.
    value: String,
    secret: String,
    hosts: Vec<String>,
    paths: Vec<String>,
    commands: Option<Vec<String>>,
    plaintext: bool,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("header", &self.header)
            .field("hosts", &self.hosts)
            .field("paths", &self.paths)
            .field("commands", &self.commands)
            .field("plaintext", &self.plaintext)
            .finish_non_exhaustive()
    }
}

impl Credential {
    /// `Authorization: Bearer <secret>`.
    #[must_use]
    pub fn bearer(secret: impl Into<String>) -> Self {
        let secret = secret.into();
        Self::new("Authorization", format!("Bearer {secret}"), secret)
    }

    /// `Authorization: token <secret>`, as GitHub also accepts.
    #[must_use]
    pub fn token(secret: impl Into<String>) -> Self {
        let secret = secret.into();
        Self::new("Authorization", format!("token {secret}"), secret)
    }

    /// `Authorization: Basic base64(<user>:<secret>)`, as git sends (e.g.
    /// user `x-access-token` with a GitHub token).
    #[must_use]
    pub fn basic(user: impl AsRef<str>, secret: impl Into<String>) -> Self {
        let secret = secret.into();
        let encoded = BASE64_STANDARD.encode(format!("{}:{secret}", user.as_ref()));
        Self::new("Authorization", format!("Basic {encoded}"), secret)
    }

    /// The header `name`, with `template`'s `{secret}` replaced by `secret`
    /// (e.g. `("X-Api-Key", "{secret}")`). A template without `{secret}`
    /// fails validation.
    #[must_use]
    pub fn header(name: impl Into<String>, template: &str, secret: impl Into<String>) -> Self {
        let secret = secret.into();
        let value = if template.contains("{secret}") {
            template.replace("{secret}", &secret)
        } else {
            String::new()
        };
        Self::new(name, value, secret)
    }

    fn new(header: impl Into<String>, value: String, secret: String) -> Self {
        Self {
            header: header.into(),
            value,
            secret,
            hosts: Vec::new(),
            paths: Vec::new(),
            commands: None,
            plaintext: false,
        }
    }

    /// The hosts it's sent to: each an exact name (`api.github.com`), or
    /// `*.` and a name for its subdomains (`*.github.com`, which doesn't
    /// match `github.com` itself). Required, and narrower than
    /// [`NetConfig::allow_host`](crate::NetConfig::allow_host)'s patterns: a
    /// `*` anywhere else (`*`, `*.*`, `github.com*`, `*github.com`) is
    /// refused, since it would match hosts the guest can register. Any port
    /// the network allows on a host gets it: hosts aren't scoped by port.
    #[must_use]
    pub fn for_hosts(mut self, hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.hosts = hosts.into_iter().map(Into::into).collect();
        self
    }

    /// Only requests for paths under these (decoded, as for
    /// [`HttpRule::with_paths`](crate::HttpRule::with_paths)); by default,
    /// every path.
    #[must_use]
    pub fn for_paths(mut self, paths: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.paths = paths.into_iter().map(Into::into).collect();
        self
    }

    /// Only these registered commands' requests (their own, not their
    /// children's, which have their own commands); by default every child's.
    #[must_use]
    pub fn for_commands(mut self, commands: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.commands = Some(commands.into_iter().map(Into::into).collect());
        self
    }

    /// Also add it to plain-HTTP requests (`HTTP_PROXY`), sending the secret
    /// unencrypted. Off by default.
    #[must_use]
    pub fn allow_plaintext(mut self) -> Self {
        self.plaintext = true;
        self
    }

    /// The header it sets.
    #[must_use]
    pub fn header_name(&self) -> &str {
        &self.header
    }

    /// The host patterns it's sent to.
    #[must_use]
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }

    /// The path prefixes it's limited to; empty for every path.
    #[must_use]
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    /// The commands it's limited to; `None` for every child.
    #[must_use]
    pub fn commands(&self) -> Option<&[String]> {
        self.commands.as_deref()
    }

    /// Fail if it can't be sent as configured.
    pub(crate) fn validate(&self) -> Result<(), String> {
        let header = &self.header;
        if header.is_empty() || !header.bytes().all(super::is_tchar) {
            return Err(format!("credential header {header:?} isn't a header name"));
        }
        let lower = header.to_ascii_lowercase();
        if RESERVED_HEADERS.contains(&lower.as_str()) || lower.starts_with("proxy-") {
            return Err(format!("a credential can't set the {header} header"));
        }
        if self.secret.is_empty() {
            return Err(format!("credential for {header} has an empty secret"));
        }
        if self.value.is_empty() {
            return Err(format!(
                "credential for {header}: its template has no {{secret}}"
            ));
        }
        if self
            .value
            .bytes()
            .any(|b| b.is_ascii_control() && b != b'\t')
        {
            return Err(format!(
                "credential for {header} contains a control character (CR, LF, NUL, ...)"
            ));
        }
        if self.hosts.is_empty() {
            return Err(format!(
                "credential for {header} has no hosts: use for_hosts"
            ));
        }
        for host in &self.hosts {
            if !is_credential_host_pattern(host) {
                return Err(format!(
                    "credential host pattern {host:?} must be a host name, or `*.` and one \
                     (no other `*`, no empty labels)"
                ));
            }
        }
        for path in &self.paths {
            if !is_normalized_path(path) {
                return Err(format!(
                    "credential path {path:?} must be a normalized absolute path"
                ));
            }
        }
        if self.commands.as_ref().is_some_and(Vec::is_empty) {
            return Err(format!(
                "credential for {header} is for no commands: leave out for_commands for every child"
            ));
        }
        Ok(())
    }

    /// Whether it's for the command `name`.
    pub(crate) fn for_command(&self, name: &str) -> bool {
        self.commands
            .as_ref()
            .is_none_or(|commands| commands.iter().any(|c| c == name))
    }

    /// Whether a connection to `host`, `over_tls` or not, may get it.
    pub(super) fn for_connection(&self, host: &str, over_tls: bool) -> bool {
        (over_tls || self.plaintext) && self.hosts.iter().any(|p| host_matches_pattern(host, p))
    }

    /// Whether a request for the decoded `path` (`None` if it has none or
    /// it's ambiguous) gets it.
    pub(super) fn for_path(&self, path: Option<&str>) -> bool {
        self.paths.is_empty()
            || path.is_some_and(|path| self.paths.iter().any(|p| super::path_is_under(path, p)))
    }

    /// What output and traces are scrubbed of, should it get there some way
    /// masking misses: the header value, a `Basic` payload, and the secret.
    pub(crate) fn sensitive_values(&self) -> impl Iterator<Item = String> + '_ {
        self.masks()
            .into_iter()
            .map(|(value, _)| String::from_utf8_lossy(&value).into_owned())
    }

    /// The header line it adds.
    pub(super) fn header_line(&self) -> String {
        format!("{}: {}\r\n", self.header, self.value)
    }

    /// `(value, mask)` pairs for responses: the header value, for `Basic`
    /// its base64 payload, and the secret, each as `[REDACTED]`. Longest
    /// first, since they're applied in turn and each contains the next.
    pub(super) fn masks(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut values = vec![self.value.clone()];
        if let Some(payload) = self.value.strip_prefix("Basic ") {
            values.push(payload.to_owned());
        }
        values.push(self.secret.clone());
        values
            .into_iter()
            .map(|v| (v.into_bytes(), MASKED.to_vec()))
            .collect()
    }
}

/// Whether `pattern` is an exact host name, or a single leading `*.` label
/// and a name with no `*` (and no empty labels) in it.
fn is_credential_host_pattern(pattern: &str) -> bool {
    let host = normalize_host(pattern);
    let name = host.strip_prefix("*.").unwrap_or(&host);
    !name.contains('*') && name.split('.').all(|label| !label.is_empty())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn values_are_built_from_the_secret() {
        let basic = Credential::basic("x-access-token", "s3cr3t");
        assert_eq!(
            basic.header_line(),
            format!(
                "Authorization: Basic {}\r\n",
                BASE64_STANDARD.encode("x-access-token:s3cr3t")
            )
        );
        assert_eq!(
            Credential::bearer("s").header_line(),
            "Authorization: Bearer s\r\n"
        );
        assert_eq!(
            Credential::token("s").header_line(),
            "Authorization: token s\r\n"
        );
        assert_eq!(
            Credential::header("X-Api-Key", "k={secret}", "s").header_line(),
            "X-Api-Key: k=s\r\n"
        );
        // The masks cover the secret, the value and a Basic payload.
        let masks: Vec<_> = basic.masks().into_iter().map(|(v, _)| v).collect();
        assert!(masks.contains(&b"s3cr3t".to_vec()));
        assert!(masks.contains(&BASE64_STANDARD.encode("x-access-token:s3cr3t").into_bytes()));
        // Debug never shows the secret.
        assert!(!format!("{basic:?}").contains("s3cr3t"));
    }

    #[test]
    fn validation_refuses_what_cant_be_sent_safely() {
        let ok = || Credential::bearer("s").for_hosts(["api.example.com"]);
        assert!(ok().validate().is_ok());
        for (bad, why) in [
            (Credential::bearer("s"), "no hosts"),
            (Credential::bearer("s").for_hosts(["*"]), "host pattern"),
            (Credential::bearer("s").for_hosts(["."]), "host pattern"),
            (Credential::bearer(""), "empty secret"),
            (Credential::bearer("a\r\nX-Evil: 1"), "control character"),
            (Credential::header("Host", "{secret}", "s"), "can't set"),
            (
                Credential::header("Proxy-Authorization", "{secret}", "s"),
                "can't set",
            ),
            (
                Credential::header("Accept-Encoding", "{secret}", "s"),
                "can't set",
            ),
            (
                Credential::header("Bad Name", "{secret}", "s"),
                "isn't a header name",
            ),
            (
                Credential::header("X-Key", "no placeholder", "s"),
                "no {secret}",
            ),
            (ok().for_paths(["/a/../b"]), "normalized"),
            (ok().for_commands(Vec::<String>::new()), "no commands"),
        ] {
            let bad = if bad.hosts.is_empty() && why != "no hosts" {
                bad.for_hosts(["api.example.com"])
            } else {
                bad
            };
            let err = bad.validate().unwrap_err();
            assert!(err.contains(why), "{why}: {err}");
        }
    }

    /// Only an exact name or `*.` and a name: no other glob, which could
    /// match a host the guest controls.
    #[test]
    fn host_patterns_are_names_or_one_leading_wildcard_label() {
        for ok in [
            "api.github.com",
            "GitHub.com.",
            "*.github.com",
            "localhost",
            "127.0.0.1",
            "[::1]",
        ] {
            assert!(is_credential_host_pattern(ok), "{ok}");
            assert!(Credential::bearer("s").for_hosts([ok]).validate().is_ok());
        }
        for bad in [
            "",
            ".",
            "*",
            "*.",
            "**",
            "*.*",
            "*m",
            "github.com*",
            "*github.com",
            "gi*hub.com",
            "*.*.github.com",
            "api.*.com",
            ".github.com",
            "github..com",
            "*..github.com",
        ] {
            assert!(!is_credential_host_pattern(bad), "{bad}");
            let err = Credential::bearer("s")
                .for_hosts(["api.github.com", bad])
                .validate()
                .unwrap_err();
            assert!(err.contains("host pattern"), "{bad}: {err}");
        }
    }

    #[test]
    fn scope_is_hosts_paths_transport_and_commands() {
        let c = Credential::bearer("s")
            .for_hosts(["github.com"])
            .for_paths(["/org/"])
            .for_commands(["git"]);
        assert!(c.for_connection("GitHub.com.", true));
        assert!(!c.for_connection("github.com", false));
        assert!(!c.for_connection("evil.com", true));
        assert!(c.allow_plaintext().for_connection("github.com", false));
        let c = Credential::bearer("s")
            .for_hosts(["github.com"])
            .for_paths(["/org/"])
            .for_commands(["git"]);
        assert!(c.for_path(Some("/org/repo.git/info/refs")));
        assert!(!c.for_path(Some("/other/repo.git")));
        assert!(!c.for_path(None));
        assert!(c.for_command("git") && !c.for_command("curl"));
        assert!(Credential::bearer("s").for_command("anything"));
    }
}
