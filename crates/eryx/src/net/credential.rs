//! Credentials the host adds to sandboxed code's requests.
//!
//! A [`Credential`] is one or more headers the host gives some requests to
//! some hosts, built from secrets the guest never sees: not as values, and
//! not as placeholders. The child proxy adds it to each matching request of
//! the commands it's for, and with [`Credential::for_script`] `eryx:net`
//! adds it to the script's own (Python's and JavaScript's), replacing any
//! header of those names the request had, and masks it out of the
//! responses.

use base64::Engine as _;
use base64::prelude::BASE64_STANDARD;

use super::{
    HttpRule, host_matches_pattern, is_normalized_path, is_valid_host_pattern,
    request_matches_rules, split_pattern_port,
};

/// What a masked credential reads as in a response.
const MASKED: &[u8] = b"[REDACTED]";

/// The most bytes a credential's headers (names and values) may take.
const MAX_HEADERS_LEN: usize = 16 * 1024;

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

/// Headers the host adds to some requests to some hosts, built from secrets
/// the sandbox never sees.
///
/// ```rust,ignore
/// let gh = Credential::bearer(token).for_hosts(["api.github.com"]).for_commands(["gh"]);
/// let git = Credential::basic("x-access-token", token)
///     .for_hosts(["github.com"])
///     .for_paths(["/my-org/"])
///     .for_commands(["git"]);
/// // Two headers that always travel together, for the script and every child.
/// let api = Credential::headers([("Authorization", access), ("X-Id-Token", id)])
///     .for_hosts(["api.example.com:443"])
///     .for_script();
/// ```
///
/// Give it to a sandbox with
/// [`SandboxBuilder::with_credential`](crate::SandboxBuilder::with_credential),
/// or to one execution with
/// [`ExecuteOptions::with_credential`](crate::ExecuteOptions::with_credential).
/// It goes to child processes' requests (through the child proxy) and, with
/// [`for_script`](Self::for_script), to the script's own.
///
/// It's added only to requests the host inspects: over TLS the host does
/// (`eryx:net`'s, or the child proxy's terminated TLS; not blind tunnels,
/// which can't reach its hosts at all) or, with
/// [`allow_plaintext`](Self::allow_plaintext), plain HTTP. A request it
/// applies to over plain HTTP without that is refused rather than sent
/// without it, and so is anything but an HTTP/1.1 request on a connection to
/// one of its hosts. A connection to a host one applies to refuses `Upgrade`
/// and `CONNECT` requests outright, so its hosts can't be used for
/// WebSockets or other upgrades, and requests naming another host than the
/// connection's (in `Host` or an absolute-form target). Every header it sets
/// is added together: any header of those names the request had is
/// dropped, so code that needs a token to send requests at all (like `gh`)
/// can be given a dummy one, and can't send one without the others.
/// Responses on such a connection are asked for and must come uncompressed,
/// and have the credential masked out; every request on such a connection
/// loses its `Range` and `If-Range` headers.
///
/// Where an execution's credential and the sandbox's both apply to a request
/// and set a header in common, the execution's wins and the sandbox's isn't
/// added at all; between one level's own, the first given wins the same way.
///
/// Masking only hides whole, verbatim echoes of the values (and of a `Basic`
/// payload, and the secret): not pieces of them, nor forms a host re-escapes
/// or re-encodes them into (JSON's `\/` for `/`, say). So don't give
/// credentials for hosts that echo requests. The values and the proxy's
/// password are also scrubbed from the output and traces, but that's defence
/// in depth, not a boundary: a script that gets hold of either can transform
/// it first.
///
/// Code given a credential can do anything with it that its hosts and
/// requests allow, on the guest's behalf: scope it as narrowly as you can.
#[derive(Clone)]
pub struct Credential {
    /// The headers it sets, name and value, with the secrets in them.
    headers: Vec<(String, String)>,
    /// The secrets the values were built from, masked like them.
    secrets: Vec<String>,
    hosts: Vec<String>,
    paths: Vec<String>,
    rules: Vec<HttpRule>,
    commands: Option<Vec<String>>,
    script: bool,
    plaintext: bool,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("headers", &self.header_names().collect::<Vec<_>>())
            .field("hosts", &self.hosts)
            .field("paths", &self.paths)
            .field("rules", &self.rules)
            .field("commands", &self.commands)
            .field("script", &self.script)
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

    /// Several headers, each value a secret, added together or not at all
    /// (an access token and an ID token, say). Names must differ, ignoring
    /// case, and the names and values together take at most 16 KiB. Every
    /// value is masked out of responses wherever it appears, so a short,
    /// non-secret one (an org ID, say) masks every copy of itself too.
    #[must_use]
    pub fn headers(
        headers: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        Self {
            headers: headers
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
            secrets: Vec::new(),
            ..Self::new(String::new(), String::new(), String::new())
        }
    }

    fn new(header: impl Into<String>, value: String, secret: String) -> Self {
        Self {
            headers: vec![(header.into(), value)],
            secrets: vec![secret],
            hosts: Vec::new(),
            paths: Vec::new(),
            rules: Vec::new(),
            commands: None,
            script: false,
            plaintext: false,
        }
    }

    /// The hosts it's sent to: each an exact name (`api.github.com`), or
    /// `*.` and a name for its subdomains (`*.github.com`, which doesn't
    /// match `github.com` itself), optionally with a port
    /// (`api.github.com:443`, `[::1]:8443`) to match only connections to
    /// that port; without one, any port the network allows. Required, and
    /// narrower than [`NetConfig::allow_host`](crate::NetConfig::allow_host)'s
    /// patterns: a `*` anywhere else (`*`, `*.*`, `github.com*`,
    /// `*github.com`) is refused, since it would match hosts the guest can
    /// register, and so is `*.` and a single label (`*.com`). Names are
    /// ASCII letters, digits, hyphens and dots (write internationalized names
    /// in punycode, `xn--...`), or an IP address; a path, space or port
    /// outside 1-65535 is refused.
    ///
    /// A wildcard under a public suffix (`*.co.uk`, `*.github.io`,
    /// `*.herokuapp.com`) isn't refused, since eryx doesn't carry the public
    /// suffix list, but it matches names anyone can register: don't use one.
    #[must_use]
    pub fn for_hosts(mut self, hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.hosts = hosts.into_iter().map(Into::into).collect();
        self
    }

    /// Only requests for paths under these (decoded, as for
    /// [`HttpRule::with_paths`]); by default, every path.
    #[must_use]
    pub fn for_paths(mut self, paths: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.paths = paths.into_iter().map(Into::into).collect();
        self
    }

    /// Only requests one of `rules` matches, by host, method, path and
    /// query, as the network's
    /// [`NetConfig::allow_http`](crate::NetConfig::allow_http) rules match.
    /// They apply on top of [`for_hosts`](Self::for_hosts) (and
    /// [`for_paths`](Self::for_paths)): a request has to be to one of its
    /// hosts *and* match a rule. By default, every request to its hosts.
    #[must_use]
    pub fn for_requests(mut self, rules: impl IntoIterator<Item = HttpRule>) -> Self {
        self.rules = rules.into_iter().collect();
        self
    }

    /// Only these registered commands' requests (their own, not their
    /// children's, which have their own commands); by default every child's.
    /// With [`for_script`](Self::for_script), an empty list gives it to the
    /// script alone.
    #[must_use]
    pub fn for_commands(mut self, commands: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.commands = Some(commands.into_iter().map(Into::into).collect());
        self
    }

    /// Also add it to the script's own requests (`eryx:net`: Python's
    /// sockets and HTTP clients, JavaScript's `fetch`), not just its
    /// children's. Off by default.
    #[must_use]
    pub fn for_script(mut self) -> Self {
        self.script = true;
        self
    }

    /// Also add it to plain-HTTP requests, sending the secrets unencrypted.
    /// Off by default.
    #[must_use]
    pub fn allow_plaintext(mut self) -> Self {
        self.plaintext = true;
        self
    }

    /// The names of the headers it sets.
    pub fn header_names(&self) -> impl Iterator<Item = &str> {
        self.headers.iter().map(|(name, _)| name.as_str())
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

    /// The HTTP rules it's limited to; empty for every request.
    #[must_use]
    pub fn rules(&self) -> &[HttpRule] {
        &self.rules
    }

    /// The commands it's limited to; `None` for every child.
    #[must_use]
    pub fn commands(&self) -> Option<&[String]> {
        self.commands.as_deref()
    }

    /// Whether the script's own requests get it.
    #[must_use]
    pub fn is_for_script(&self) -> bool {
        self.script
    }

    /// Fail if it can't be sent as configured.
    ///
    /// # Errors
    ///
    /// Describes the first problem: no headers or hosts, a header name
    /// that isn't one or is reserved (`Host`, `Content-Length`,
    /// `Connection`, `Proxy-*`, ...), one set twice, an empty value or one
    /// with a control character (CR, LF, NUL), headers over 16 KiB, a host
    /// pattern, path or rule that can't match, or no commands without the
    /// script.
    pub fn validate(&self) -> Result<(), String> {
        if self.headers.is_empty() {
            return Err("credential has no headers".into());
        }
        let mut len = 0;
        for (i, (header, value)) in self.headers.iter().enumerate() {
            if header.is_empty() || !header.bytes().all(super::is_tchar) {
                return Err(format!("credential header {header:?} isn't a header name"));
            }
            let lower = header.to_ascii_lowercase();
            if RESERVED_HEADERS.contains(&lower.as_str()) || lower.starts_with("proxy-") {
                return Err(format!("a credential can't set the {header} header"));
            }
            if self.headers[..i]
                .iter()
                .any(|(other, _)| other.eq_ignore_ascii_case(header))
            {
                return Err(format!("credential sets the {header} header twice"));
            }
            if value.is_empty() {
                return Err(format!(
                    "credential for {header} has an empty value (a header template needs \
                     {{secret}})"
                ));
            }
            if value.bytes().any(|b| b.is_ascii_control() && b != b'\t') {
                return Err(format!(
                    "credential for {header} contains a control character (CR, LF, NUL, ...)"
                ));
            }
            len += header.len() + value.len();
        }
        if len > MAX_HEADERS_LEN {
            return Err(format!(
                "credential headers take {len} bytes, over the {MAX_HEADERS_LEN} allowed"
            ));
        }
        let names = self.header_names().collect::<Vec<_>>().join(", ");
        if self.secrets.iter().any(String::is_empty) {
            return Err(format!("credential for {names} has an empty secret"));
        }
        if self.hosts.is_empty() {
            return Err(format!(
                "credential for {names} has no hosts: use for_hosts"
            ));
        }
        for host in &self.hosts {
            if !is_credential_host_pattern(host) {
                return Err(format!(
                    "credential host pattern {host:?} must be an IP address, an ASCII host \
                     name (letters, digits, hyphens and dots; punycode for others), or `*.` and \
                     a name of two labels or more (no other `*`, path or spaces), optionally \
                     with a port from 1 to 65535"
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
        for rule in &self.rules {
            if !is_valid_host_pattern(rule.host()) {
                return Err(format!(
                    "credential HTTP rule host {:?} isn't a host pattern",
                    rule.host()
                ));
            }
            rule.validate()?;
        }
        if !self.script && self.commands.as_ref().is_some_and(Vec::is_empty) {
            return Err(format!(
                "credential for {names} is for no commands: leave out for_commands for every child"
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

    /// Whether it's for connections to `host` on `port`, whatever their
    /// transport.
    pub(super) fn for_host(&self, host: &str, port: u16) -> bool {
        self.hosts
            .iter()
            .any(|p| host_matches_pattern(host, port, p))
    }

    /// Whether it may be sent `over_tls` or not.
    pub(super) fn sends_over(&self, over_tls: bool) -> bool {
        over_tls || self.plaintext
    }

    /// Whether the request `method target` (on a connection to `host` on
    /// `port`, one of its hosts) gets it: its decoded path is under one of
    /// its paths, and a rule matches it.
    pub(super) fn for_request(&self, host: &str, port: u16, method: &str, target: &str) -> bool {
        let path = super::request_path(target);
        let path_ok = self.paths.is_empty()
            || path
                .as_deref()
                .is_some_and(|path| self.paths.iter().any(|p| super::path_is_under(path, p)));
        path_ok && request_matches_rules(&self.rules, host, port, method, target)
    }

    /// Whether it sets a header `other` sets too.
    pub(super) fn overlaps(&self, other: &Credential) -> bool {
        self.header_names()
            .any(|a| other.header_names().any(|b| a.eq_ignore_ascii_case(b)))
    }

    /// Whether it sets the header `name`.
    pub(super) fn sets(&self, name: &str) -> bool {
        self.header_names().any(|h| h.eq_ignore_ascii_case(name))
    }

    /// What output and traces are scrubbed of, should it get there some way
    /// masking misses: the header values, `Basic` payloads, and the secrets.
    pub fn sensitive_values(&self) -> impl Iterator<Item = String> + '_ {
        self.masks()
            .into_iter()
            .map(|(value, _)| String::from_utf8_lossy(&value).into_owned())
    }

    /// The header lines it adds.
    pub(super) fn header_lines(&self) -> String {
        self.headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect()
    }

    /// `(value, mask)` pairs for responses: each header value, what follows
    /// a leading scheme in one (`Bearer <token>`'s token, `Basic`'s base64
    /// payload), and the secrets, each as `[REDACTED]`. Longest first, since
    /// they're applied in turn and one may contain another.
    pub(super) fn masks(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut values: Vec<&str> = Vec::new();
        for (_, value) in &self.headers {
            values.push(value);
            if let Some((scheme, rest)) = value.split_once(' ')
                && !scheme.is_empty()
                && scheme.bytes().all(super::is_tchar)
            {
                values.push(rest.trim());
            }
        }
        values.extend(self.secrets.iter().map(String::as_str));
        values.retain(|v| !v.is_empty());
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        values.dedup();
        values
            .into_iter()
            .map(|v| (v.as_bytes().to_vec(), MASKED.to_vec()))
            .collect()
    }
}

/// Whether `pattern` is an IP literal (bracketed or not, without a zone), an
/// exact ASCII host name (letters, digits, hyphens and dots, no empty
/// labels; a trailing dot and case are ignored), or `*.` and such a name of
/// at least two labels (so not `*.com`), optionally with a `:port` (1 to
/// 65535; an IPv6 address takes one only in brackets). No path, space or
/// non-ASCII character: those would never match, so they're refused as
/// typos.
fn is_credential_host_pattern(pattern: &str) -> bool {
    let Some((pattern, _)) = split_pattern_port(pattern) else {
        return false;
    };
    let literal = pattern.strip_circumfix('[', ']').unwrap_or(pattern);
    if literal.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let host = pattern.strip_suffix('.').unwrap_or(pattern);
    let (wildcard, name) = match host.strip_prefix("*.") {
        Some(name) => (true, name),
        None => (false, host),
    };
    let labels: Vec<&str> = name.split('.').collect();
    labels.iter().all(|label| {
        !label.is_empty()
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    }) && (!wildcard || labels.len() >= 2)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn values_are_built_from_the_secret() {
        let basic = Credential::basic("x-access-token", "s3cr3t");
        assert_eq!(
            basic.header_lines(),
            format!(
                "Authorization: Basic {}\r\n",
                BASE64_STANDARD.encode("x-access-token:s3cr3t")
            )
        );
        assert_eq!(
            Credential::bearer("s").header_lines(),
            "Authorization: Bearer s\r\n"
        );
        assert_eq!(
            Credential::token("s").header_lines(),
            "Authorization: token s\r\n"
        );
        assert_eq!(
            Credential::header("X-Api-Key", "k={secret}", "s").header_lines(),
            "X-Api-Key: k=s\r\n"
        );
        // The masks cover the secret, the value and a Basic payload.
        let masks: Vec<_> = basic.masks().into_iter().map(|(v, _)| v).collect();
        assert!(masks.contains(&b"s3cr3t".to_vec()));
        assert!(masks.contains(&BASE64_STANDARD.encode("x-access-token:s3cr3t").into_bytes()));
        // Debug never shows the secret.
        assert!(!format!("{basic:?}").contains("s3cr3t"));

        // Several headers go together, each value masked.
        let both = Credential::headers([("Authorization", "Bearer a1"), ("X-Id-Token", "i2")]);
        assert_eq!(
            both.header_lines(),
            "Authorization: Bearer a1\r\nX-Id-Token: i2\r\n"
        );
        let masks: Vec<_> = both.masks().into_iter().map(|(v, _)| v).collect();
        assert_eq!(
            masks,
            [b"Bearer a1".to_vec(), b"a1".to_vec(), b"i2".to_vec()]
        );
        assert!(!format!("{both:?}").contains("a1"));
        assert!(both.sets("x-id-token") && !both.sets("cookie"));
        assert!(both.overlaps(&Credential::bearer("other")));
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
                "needs {secret}",
            ),
            (
                Credential::headers(Vec::<(String, String)>::new()),
                "no headers",
            ),
            (Credential::headers([("X-A", "1"), ("x-a", "2")]), "twice"),
            (Credential::headers([("X-A", "")]), "empty value"),
            (
                Credential::headers([("X-A", "1"), ("X-B", "2\n")]),
                "control character",
            ),
            (Credential::headers([("Connection", "close")]), "can't set"),
            (
                Credential::headers([("Transfer-Encoding", "x")]),
                "can't set",
            ),
            (
                Credential::headers([("X-A", "a".repeat(16 * 1024))]),
                "over the",
            ),
            (
                ok().for_requests([HttpRule::new("api.example.com").with_paths(["/a/../b"])]),
                "normalized",
            ),
            (
                ok().for_requests([HttpRule::new("api.example.com").with_queries(["("])]),
                "HTTP rule",
            ),
            (
                ok().for_requests([HttpRule::new("")]),
                "isn't a host pattern",
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
        // The script alone: no commands.
        assert!(
            ok().for_script()
                .for_commands(Vec::<String>::new())
                .validate()
                .is_ok()
        );
        assert!(
            Credential::headers([("X-A", "1"), ("X-B", "2")])
                .for_hosts(["api.example.com:8443"])
                .validate()
                .is_ok()
        );
    }

    /// Only an IP literal, an exact ASCII name, or `*.` and a name of two
    /// labels or more: no other glob, which could match a host the guest
    /// controls, and no port, path, space or non-ASCII character, which
    /// would never match.
    #[test]
    fn host_patterns_are_names_or_one_leading_wildcard_label() {
        for ok in [
            "api.github.com",
            "GitHub.com.",
            "*.github.com",
            "localhost",
            "127.0.0.1",
            "[::1]",
            "::1",
            "*.github.co.uk",
            "my-host.example-1.com",
            "xn--bcher-kva.example",
            "*.xn--bcher-kva.example",
            "api.github.com:443",
            "*.github.com:8443",
            "[::1]:443",
            "127.0.0.1:80",
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
            "*.com",
            "*.localhost",
            "*.com.",
            "api.github.com:0",
            "api.github.com:x",
            "api.github.com:",
            "api.github.com:65536",
            "[::1]:",
            "*:443",
            "fe80::1%lo",
            "github.com/org",
            "api.github.com/",
            "api github.com",
            " api.github.com",
            "bücher.example",
            "*.bücher.example",
            "api_x.github.com",
            "https://api.github.com",
            "user@github.com",
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
    fn scope_is_hosts_ports_requests_transport_and_commands() {
        let c = Credential::bearer("s")
            .for_hosts(["github.com", "api.example.com:8443"])
            .for_paths(["/org/"])
            .for_commands(["git"]);
        assert!(c.for_host("GitHub.com.", 443) && c.for_host("github.com", 80));
        assert!(c.for_host("api.example.com", 8443) && !c.for_host("api.example.com", 443));
        assert!(!c.for_host("evil.com", 443));
        assert!(c.sends_over(true) && !c.sends_over(false));
        assert!(c.clone().allow_plaintext().sends_over(false));
        let get = |target| c.for_request("github.com", 443, "GET", target);
        assert!(get("/org/repo.git/info/refs"));
        assert!(!get("/other/repo.git"));
        assert!(!get("/org/../other"));
        assert!(c.for_command("git") && !c.for_command("curl"));
        assert!(Credential::bearer("s").for_command("anything"));
        assert!(!c.is_for_script() && c.for_script().is_for_script());

        // Rules on top of the hosts and paths: method, path and query.
        let c = Credential::bearer("s")
            .for_hosts(["github.com"])
            .for_requests([HttpRule::new("github.com")
                .with_methods(["GET"])
                .with_paths(["/org/repo.git/info/refs"])
                .with_queries(["service=git-upload-pack"])]);
        let ok = "/org/repo.git/info/refs?service=git-upload-pack";
        assert!(c.for_request("github.com", 443, "GET", ok));
        assert!(!c.for_request("github.com", 443, "POST", ok));
        assert!(!c.for_request(
            "github.com",
            443,
            "GET",
            "/org/repo.git/info/refs?service=git-receive-pack"
        ));
    }
}
