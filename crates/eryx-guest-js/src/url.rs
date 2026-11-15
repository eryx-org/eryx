//! The `http:`/`https:` URLs `fetch` takes.
//!
//! The `url` crate would bring IDNA's Unicode tables (ICU4X), over a
//! megabyte of guest, for host names the host's network policy has to match
//! as ASCII anyway. So this parses what an HTTP request needs, following the
//! WHATWG URL rules for it: ASCII host names (non-ASCII ones in their
//! punycode `xn--` form), IPv4 and bracketed IPv6 literals, a port, and the
//! path and query, percent-encoded and with dot segments resolved. The
//! fragment is dropped. URLs with credentials are refused, as `fetch` does.

use std::fmt;
use std::net::Ipv6Addr;

use crate::fetch::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Url {
    pub(crate) https: bool,
    /// Lowercase; an IPv6 literal without its brackets.
    pub(crate) host: String,
    /// Only when it isn't the scheme's default.
    pub(crate) explicit_port: Option<u16>,
    /// The request target: the path (starting with `/`) and query.
    pub(crate) target: String,
}

impl Url {
    pub(crate) fn parse(input: &str) -> Result<Self, Error> {
        let input = clean(input);
        let invalid = |why: &str| Error::new("invalid-url", format!("{input:?} {why}"));
        let Some(colon) = input.find(':') else {
            return Err(invalid("is not an absolute URL"));
        };
        let scheme = &input[..colon];
        let valid_scheme = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
        if !valid_scheme {
            return Err(invalid("is not an absolute URL"));
        }
        let https = match scheme.to_ascii_lowercase().as_str() {
            "http" => false,
            "https" => true,
            other => {
                return Err(Error::new(
                    "unsupported-scheme",
                    format!("fetch only supports http and https, not {other}:"),
                ));
            }
        };
        let rest = input[colon + 1..].trim_start_matches(['/', '\\']);
        let end = rest.find(['/', '\\', '?', '#']).unwrap_or(rest.len());
        let (authority, rest) = rest.split_at(end);
        if authority.contains('@') {
            return Err(Error::new(
                "invalid-url",
                "URLs with credentials aren't allowed",
            ));
        }

        let ipv6 = authority.starts_with('[');
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (addr, after) = v6
                .split_once(']')
                .ok_or_else(|| invalid("has a bad IPv6 host"))?;
            let addr: Ipv6Addr = addr.parse().map_err(|_| invalid("has a bad IPv6 host"))?;
            let port = match after {
                "" => None,
                p => Some(
                    p.strip_prefix(':')
                        .ok_or_else(|| invalid("has a bad port"))?,
                ),
            };
            (addr.to_string(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host.to_ascii_lowercase(), Some(port)),
                None => (authority.to_ascii_lowercase(), None),
            }
        };
        if host.is_empty() {
            return Err(invalid("has no host"));
        }
        if !host.is_ascii() {
            return Err(invalid(
                "has a non-ASCII host name: use its punycode (xn--) form",
            ));
        }
        if !ipv6
            && !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
        {
            return Err(invalid("has a bad host name"));
        }
        let default_port = if https { 443 } else { 80 };
        let explicit_port = match port.filter(|p| !p.is_empty()) {
            None => None,
            Some(p) => {
                let p: u16 = p.parse().map_err(|_| invalid("has a bad port"))?;
                (p != default_port).then_some(p)
            }
        };

        Ok(Self {
            https,
            host,
            explicit_port,
            target: target(rest),
        })
    }

    /// Resolve a `Location` header against this URL.
    pub(crate) fn join(&self, location: &str) -> Result<Self, Error> {
        let location = clean(location);
        let has_scheme = location.find(':').is_some_and(|i| {
            let scheme = &location[..i];
            scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        });
        if has_scheme {
            return Self::parse(&location);
        }
        if location.starts_with("//") || location.starts_with("\\\\") {
            let scheme = if self.https { "https:" } else { "http:" };
            return Self::parse(&format!("{scheme}{location}"));
        }
        let target = if location.starts_with(['/', '\\']) {
            target(&location)
        } else if location.starts_with('?') {
            let path = self.target.split('?').next().unwrap_or("/");
            target(&format!("{path}{location}"))
        } else if location.is_empty() || location.starts_with('#') {
            self.target.clone()
        } else {
            let path = self.target.split('?').next().unwrap_or("/");
            let dir = &path[..=path.rfind('/').unwrap_or(0)];
            target(&format!("{dir}{location}"))
        };
        Ok(Self {
            target,
            ..self.clone()
        })
    }

    pub(crate) fn port(&self) -> u16 {
        self.explicit_port
            .unwrap_or(if self.https { 443 } else { 80 })
    }

    /// The `Host` header: the host (bracketed if IPv6), and the port if
    /// it isn't the default.
    pub(crate) fn authority(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match self.explicit_port {
            Some(port) => format!("{host}:{port}"),
            None => host,
        }
    }

    pub(crate) fn same_origin(&self, other: &Self) -> bool {
        self.https == other.https && self.host == other.host && self.port() == other.port()
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = if self.https { "https" } else { "http" };
        write!(f, "{scheme}://{}{}", self.authority(), self.target)
    }
}

/// Strip what the WHATWG parser ignores: leading and trailing C0 controls and
/// spaces, and tabs and newlines anywhere.
fn clean(input: &str) -> String {
    input
        .trim_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect()
}

/// The request target for what follows the authority: the path with dot
/// segments resolved and the query, both percent-encoded, without the
/// fragment.
fn target(rest: &str) -> String {
    let rest = rest.split('#').next().unwrap_or_default();
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (rest, None),
    };
    let path = path.replace('\\', "/");
    let mut segments: Vec<&str> = Vec::new();
    let parts: Vec<&str> = path.split('/').skip(1).collect();
    for (i, segment) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        match segment.to_ascii_lowercase().as_str() {
            "." | "%2e" => {
                if last {
                    segments.push("");
                }
            }
            ".." | ".%2e" | "%2e." | "%2e%2e" => {
                segments.pop();
                if last {
                    segments.push("");
                }
            }
            _ => segments.push(segment),
        }
    }
    let mut target = format!("/{}", segments.join("/"));
    if let Some(query) = query {
        target.push('?');
        target.push_str(query);
    }
    percent_encode(&target)
}

/// Percent-encode what can't go in a request line: controls, space, `"`,
/// `<`, `>`, `` ` ``, and non-ASCII. Existing escapes are kept.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b <= b' ' || b >= 0x7f || b"\"<>`".contains(&b) {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push(char::from(b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap_or_else(|e| panic!("{s}: {e:?}"))
    }

    #[test]
    fn parses_hosts_ports_and_targets() {
        let u = url("HTTPS://API.Example.com/a/b?q=1#frag");
        assert_eq!(
            (u.https, u.host.as_str(), u.port()),
            (true, "api.example.com", 443)
        );
        assert_eq!(u.target, "/a/b?q=1");
        assert_eq!(u.to_string(), "https://api.example.com/a/b?q=1");

        assert_eq!(url("http://a.test").target, "/");
        assert_eq!(url("http://a.test?x").target, "/?x");
        assert_eq!(url("http://a.test:80/").explicit_port, None);
        assert_eq!(url("http://a.test:8080/").authority(), "a.test:8080");
        assert_eq!(url("http://a.test:/").port(), 80);
        let v6 = url("http://[::1]:9/x");
        assert_eq!(
            (v6.host.as_str(), v6.authority().as_str()),
            ("::1", "[::1]:9")
        );
        assert_eq!(url("http://127.0.0.1/").host, "127.0.0.1");
        assert_eq!(url(" http://a.test/\tx\n ").target, "/x");
        assert_eq!(url("http:\\\\a.test\\b").target, "/b");
    }

    #[test]
    fn resolves_dot_segments_and_encodes() {
        assert_eq!(url("http://a.test/a/./b/../c").target, "/a/c");
        assert_eq!(url("http://a.test/a/b/..").target, "/a/");
        assert_eq!(url("http://a.test/../../x").target, "/x");
        assert_eq!(url("http://a.test/%2E%2e/x").target, "/x");
        assert_eq!(url("http://a.test/a b?c d").target, "/a%20b?c%20d");
        assert_eq!(url("http://a.test/é").target, "/%C3%A9");
        assert_eq!(url("http://a.test/%41").target, "/%41");
    }

    #[test]
    fn refuses_what_fetch_refuses() {
        for (input, code) in [
            ("ftp://a.test/", "unsupported-scheme"),
            ("data:text/plain,hi", "unsupported-scheme"),
            ("/relative", "invalid-url"),
            ("not a url", "invalid-url"),
            ("https://user:pw@a.test/", "invalid-url"),
            ("https://user@a.test/", "invalid-url"),
            ("https://", "invalid-url"),
            ("http://::1/", "invalid-url"),
            ("https://bü.test/", "invalid-url"),
            ("https://a b.test/", "invalid-url"),
            ("https://a.test:99999/", "invalid-url"),
            ("https://a.test:x/", "invalid-url"),
            ("https://[::1/", "invalid-url"),
            ("https://[nope]/", "invalid-url"),
        ] {
            assert_eq!(Url::parse(input).unwrap_err().code, code, "{input}");
        }
    }

    #[test]
    fn joins_locations() {
        let base = url("https://a.test:8443/dir/page?q=1");
        let join = |l: &str| base.join(l).unwrap().to_string();
        assert_eq!(join("http://b.test/x"), "http://b.test/x");
        assert_eq!(join("//b.test/x"), "https://b.test/x");
        assert_eq!(join("/x?y"), "https://a.test:8443/x?y");
        assert_eq!(join("other"), "https://a.test:8443/dir/other");
        assert_eq!(join("../up"), "https://a.test:8443/up");
        assert_eq!(join("?z=2"), "https://a.test:8443/dir/page?z=2");
        assert_eq!(join("#f"), "https://a.test:8443/dir/page?q=1");
        assert_eq!(
            base.join("ftp://x/").unwrap_err().code,
            "unsupported-scheme"
        );
    }

    #[test]
    fn origins() {
        let a = url("https://a.test/x");
        assert!(a.same_origin(&url("https://A.test:443/y")));
        assert!(!a.same_origin(&url("http://a.test/x")));
        assert!(!a.same_origin(&url("https://a.test:8443/x")));
        assert!(!a.same_origin(&url("https://b.test/x")));
    }
}
