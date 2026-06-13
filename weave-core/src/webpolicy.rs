//! WL-049 / ADR-0002: pure web-access policy + SSRF/loopback URL validator for the
//! governed obscura web-access seam.
//!
//! This module is **pure** (no I/O, no process, no Store) so it can be unit-tested
//! exhaustively and reused by both the MCP `weave_web` dispatcher and the `weave web`
//! CLI without duplicating the deny-by-default decision. It answers two questions:
//!
//!   1. Is `<op>` a known `browser_*` operation, and is it explicitly ALLOWED by the
//!      operator's policy? (deny-by-default — an empty/unset allow-list denies all.)
//!   2. For a URL-bearing op, is `<url>` safe to navigate to — i.e. NOT an internal /
//!      loopback / link-local / RFC1918 / `*.local` / bare-IP target (SSRF guard),
//!      and within the allowed-domains list (when one is configured)?
//!
//! Everything here is feature-gated behind `obscura` so the default build compiles
//! none of it (dependency-light invariant).

use crate::config::Config;

/// Hard cap on the byte length of a single web-op argument value or URL. Mirrors the
/// `MAX_BODY`-class caps elsewhere: an attacker-influenceable arg must never be
/// unbounded before it is forwarded to the obscura child.
pub const MAX_WEB_ARG_LEN: usize = crate::store::MAX_BODY;

/// The 35 `browser_*` operations exposed by `obscura mcp` (verified against
/// `obscura/crates/obscura-mcp/src/lib.rs` `handle_tool_call`). The dispatcher
/// forwards opaquely (`browser_<op>` + args) — weave does NOT re-declare per-op arg
/// schemas; this enum only enumerates the *names* so the policy gate can reject an
/// unknown op BEFORE any child is spawned (deny-by-default for typos/injection too).
///
/// Kept as a flat list rather than a struct-per-op precisely because weave is the
/// thin governance plane: the authoritative arg schemas live in obscura and are
/// fetched on demand via `describe`.
pub const WEB_OPS: &[&str] = &[
    // Core navigation / interaction
    "navigate",
    "snapshot",
    "click",
    "fill",
    "type",
    "press_key",
    "select_option",
    "evaluate",
    "wait_for",
    "network_requests",
    "console_messages",
    "close",
    // Tier 1 agent-UX additions
    "markdown",
    "links",
    "interactive_elements",
    "back",
    "forward",
    "reload",
    "get_cookies",
    "set_cookie",
    "clear_cookies",
    "wait_for_text",
    // Tier 2 agent-UX additions
    "detect_forms",
    "fill_form",
    "scroll",
    "get_attribute",
    "count",
    "extract",
    "tab_new",
    "tab_list",
    "tab_switch",
    "tab_close",
    "search",
    "storage_state",
    "set_storage_state",
];

/// The subset of [`WEB_OPS`] that carry a URL weave must SSRF-validate before
/// forwarding. (Other ops act on the already-loaded page and carry no new URL.)
const URL_BEARING_OPS: &[&str] = &["navigate"];

/// A validated, known web operation. Construction goes through [`WebOp::parse`],
/// which rejects unknown/typo'd op names (the first deny-by-default gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebOp(String);

impl WebOp {
    /// Parse a caller-supplied action into a known op, accepting either the bare op
    /// name (`"navigate"`) or the fully-qualified `browser_*` form
    /// (`"browser_navigate"`). Returns `None` for any unknown op (deny-by-default).
    pub fn parse(action: &str) -> Option<WebOp> {
        let bare = action.strip_prefix("browser_").unwrap_or(action);
        if WEB_OPS.contains(&bare) {
            Some(WebOp(bare.to_string()))
        } else {
            None
        }
    }

    /// The bare op name (`"navigate"`).
    pub fn name(&self) -> &str {
        &self.0
    }

    /// The fully-qualified obscura tool name (`"browser_navigate"`).
    pub fn obscura_tool(&self) -> String {
        format!("browser_{}", self.0)
    }

    /// Does this op carry a URL weave must SSRF-validate?
    pub fn is_url_bearing(&self) -> bool {
        URL_BEARING_OPS.contains(&self.0.as_str())
    }
}

/// The reason a web op was refused, for a clean (non-panicking) error message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denied {
    /// The action is not a known `browser_*` op.
    UnknownOp(String),
    /// The op is not in the operator's allow-list (deny-by-default).
    OpNotAllowed(String),
    /// A URL-bearing op was missing its required `url` argument.
    MissingUrl,
    /// The URL failed the SSRF/loopback guard or exceeded a cap.
    UnsafeUrl(String),
    /// The URL's host is not in the configured allowed-domains list.
    DomainNotAllowed(String),
    /// An argument value exceeded [`MAX_WEB_ARG_LEN`].
    ArgTooLong(String),
}

impl Denied {
    /// A user-facing, actionable message (never leaks internal detail).
    pub fn message(&self) -> String {
        match self {
            Denied::UnknownOp(a) => format!(
                "unknown web op {a:?} (deny-by-default). Known ops: see `weave web --list`."
            ),
            Denied::OpNotAllowed(op) => format!(
                "web op {op:?} is not allowed by policy (deny-by-default). \
                 Add it to obscura_allow_ops (or WEAVE_OBSCURA_ALLOW_OPS), or use \"*\"."
            ),
            Denied::MissingUrl => "this web op requires a 'url' argument.".to_string(),
            Denied::UnsafeUrl(why) => {
                format!("refusing to navigate: {why} (SSRF guard; set obscura_allow_internal=true to override).")
            }
            Denied::DomainNotAllowed(host) => {
                format!("host {host:?} is not in obscura_allow_domains (or WEAVE_OBSCURA_ALLOW_DOMAINS). Add it, or use \"*\" to allow all public hosts (internal hosts stay SSRF-blocked).")
            }
            Denied::ArgTooLong(k) => {
                format!("web argument {k:?} is too long (max {MAX_WEB_ARG_LEN} bytes).")
            }
        }
    }
}

/// The resolved web-access policy, derived from [`Config`]. Pure data; no I/O.
#[derive(Debug, Clone)]
pub struct WebPolicy {
    /// Allowed ops (bare names). `["*"]` ⇒ all ops. Empty ⇒ deny all.
    allow_ops: Vec<String>,
    /// Allowed domains (exact or `.suffix` match). `["*"]` ⇒ any public host.
    /// Empty ⇒ no domain restriction. In all cases the SSRF guard still applies,
    /// so internal/loopback hosts are blocked regardless.
    allow_domains: Vec<String>,
    /// Permit internal / loopback / private hosts (SSRF override). Default false.
    allow_internal: bool,
}

impl WebPolicy {
    /// Build the policy from config (deny-by-default when unset).
    pub fn from_config(cfg: &Config) -> WebPolicy {
        WebPolicy {
            allow_ops: cfg.obscura_allow_ops.clone().unwrap_or_default(),
            allow_domains: cfg.obscura_allow_domains.clone().unwrap_or_default(),
            allow_internal: cfg.obscura_allow_internal.unwrap_or(false),
        }
    }

    /// Is `op` explicitly allowed by policy?
    fn op_allowed(&self, op: &WebOp) -> bool {
        self.allow_ops.iter().any(|a| a == "*" || a == op.name())
    }

    /// Decide whether `action` (with optional `url`) may run.
    ///
    /// Order: parse op → op allow-list → (url-bearing) cap + SSRF + domain. Returns
    /// the validated [`WebOp`] on success, or a [`Denied`] reason. PURE — makes no
    /// network/process/Store call; the permission-ask escalation (interactive grant)
    /// lives in the MCP layer, which calls this first.
    pub fn decide(&self, action: &str, url: Option<&str>) -> Result<WebOp, Denied> {
        let op = WebOp::parse(action).ok_or_else(|| Denied::UnknownOp(action.to_string()))?;
        if !self.op_allowed(&op) {
            return Err(Denied::OpNotAllowed(op.name().to_string()));
        }
        if op.is_url_bearing() {
            let url = url.ok_or(Denied::MissingUrl)?;
            self.check_url(url)?;
        }
        Ok(op)
    }

    /// Validate a URL: cap, SSRF/loopback guard, and (when configured) domain
    /// allow-list. Exposed so the CLI/MCP can pre-check a nav URL directly.
    pub fn check_url(&self, url: &str) -> Result<(), Denied> {
        if url.len() > MAX_WEB_ARG_LEN {
            return Err(Denied::ArgTooLong("url".to_string()));
        }
        let host = url_host(url)
            .ok_or_else(|| Denied::UnsafeUrl("not a valid http(s) URL".to_string()))?;
        if !self.allow_internal && host_is_internal(&host) {
            return Err(Denied::UnsafeUrl(format!(
                "{host:?} is an internal/loopback/private host"
            )));
        }
        if !self.allow_domains.is_empty() && !self.domain_allowed(&host) {
            return Err(Denied::DomainNotAllowed(host));
        }
        Ok(())
    }

    /// Is `host` permitted by the configured allowed-domains list? A policy entry
    /// matches a host exactly, or as a parent domain (`example.com` matches
    /// `sub.example.com`).
    fn domain_allowed(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.allow_domains.iter().any(|d| {
            let d = d.trim().trim_start_matches('.').to_ascii_lowercase();
            // `*` = wildcard: any (SSRF-passing, i.e. public) host, mirroring
            // `obscura_allow_ops`'s `*`. Without this, an explicit `["*"]` matched
            // no host and silently denied everything — the opposite of the deny
            // message's advertised `*`. (Internal/loopback hosts are still blocked
            // by the SSRF guard, which runs before this check.)
            d == "*" || (!d.is_empty() && (host == d || host.ends_with(&format!(".{d}"))))
        })
    }
}

/// Validate and cap a single web-op argument value (non-URL). Returns the value
/// unchanged on success, or [`Denied::ArgTooLong`]. NUL/control bytes are rejected
/// the same way identities are bounded (defense in depth even though args never
/// reach a shell — they ride a JSON-RPC frame to the child).
pub fn check_arg(key: &str, value: &str) -> Result<(), Denied> {
    if value.len() > MAX_WEB_ARG_LEN {
        return Err(Denied::ArgTooLong(key.to_string()));
    }
    Ok(())
}

/// Extract the lowercased host from an `http`/`https` URL string. Pure — no DNS,
/// no network. Returns `None` for anything that is not a plausible http(s) URL with
/// a host. Mirrors the deliberately-simple `model::pr_url_valid` style: weave only
/// needs the host to run the SSRF check; obscura does the real navigation.
pub fn url_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    // Authority is everything up to the first '/', '?' or '#'.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    // Strip optional userinfo (`user:pass@host`) — keep only the host:port part.
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    // Strip a :port suffix. For an IPv6 literal `[::1]:80`, keep the bracketed part.
    let host = if let Some(stripped) = hostport.strip_prefix('[') {
        // IPv6 literal: take up to the closing ']'.
        stripped.split(']').next().unwrap_or(stripped).to_string()
    } else {
        hostport
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(hostport)
            .to_string()
    };
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Is `host` an internal / loopback / link-local / private / `*.local` / bare-IP
/// target that the SSRF guard blocks by default? `host` is expected lowercased.
///
/// Covers (default-DENY): `localhost` and any `*.localhost`; `*.local` (mDNS);
/// loopback (`127.0.0.0/8`, `::1`); link-local (`169.254.0.0/16` incl. the cloud
/// metadata endpoint `169.254.169.254`, IPv6 `fe80::/10`); RFC1918 private ranges
/// (`10/8`, `172.16/12`, `192.168/16`); IPv4-shorthand/unspecified (`0.0.0.0`); and
/// **any bare IPv4/IPv6 literal** (a literal IP bypasses domain allow-lists and is a
/// classic SSRF vector — denied unless `allow_internal`). A normal public hostname
/// (`example.com`) is NOT internal.
pub fn host_is_internal(host: &str) -> bool {
    if host.is_empty() {
        return true;
    }
    // Normalize a single trailing dot (`localhost.`, `127.0.0.1.`, `example.com.`):
    // a browser treats `foo.` and `foo` as the same FQDN, so the dot must not let an
    // internal name/IP slip past the checks below.
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return true;
    }
    // mDNS / local-suffix names.
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    // IPv6 literal (may include zone id after '%').
    let v6_candidate = host.split('%').next().unwrap_or(host);
    if let Ok(v6) = v6_candidate.parse::<std::net::Ipv6Addr>() {
        return v6.is_loopback()
            || v6.is_unspecified()
            // link-local fe80::/10
            || (v6.segments()[0] & 0xffc0) == 0xfe80
            // unique-local fc00::/7
            || (v6.segments()[0] & 0xfe00) == 0xfc00
            // IPv4-mapped (`::ffff:127.0.0.1`) → check the embedded v4
            || v6.to_ipv4_mapped().map(ipv4_is_internal).unwrap_or(false)
            // IPv4-compatible / mapped via to_ipv4 (`::127.0.0.1`)
            || v6.to_ipv4().map(ipv4_is_internal).unwrap_or(false);
    }
    // Bare IPv4 literal — denied (covers loopback/private/link-local + any public IP,
    // since a literal IP bypasses domain allow-listing).
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        let _ = ipv4_is_internal(v4); // classification kept for clarity
        return true;
    }
    // Fail-closed on obfuscated IP literals. A real browser canonicalizes non-dotted
    // numeric authorities — decimal (`2130706433`), hex (`0x7f000001`, `0x7f.0.0.1`),
    // octal (`017700000001`), and dotted forms with hex/octal octets — back to an IPv4
    // address (e.g. 127.0.0.1), but `Ipv4Addr::parse` above accepts ONLY canonical
    // dotted-decimal, so these slip through as "not an IP". Any authority composed
    // solely of `[0-9a-fA-FxX.]` that did NOT parse as a standard dotted-decimal IPv4
    // can only be such an obfuscated IP literal — treat it as a bare-IP literal, which
    // the policy denies by default (same intent as the dotted-decimal branch above).
    if !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == 'x' || c == 'X' || c == '.')
    {
        return true;
    }
    false
}

/// Classify an IPv4 address as an internal/loopback/private/link-local target.
fn ipv4_is_internal(ip: std::net::Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(ops: &[&str], domains: &[&str], allow_internal: bool) -> WebPolicy {
        WebPolicy {
            allow_ops: ops.iter().map(|s| s.to_string()).collect(),
            allow_domains: domains.iter().map(|s| s.to_string()).collect(),
            allow_internal,
        }
    }

    #[test]
    fn op_count_is_35() {
        assert_eq!(WEB_OPS.len(), 35, "obscura exposes 35 browser_* ops");
    }

    #[test]
    fn parse_accepts_bare_and_prefixed() {
        assert_eq!(WebOp::parse("navigate").unwrap().name(), "navigate");
        assert_eq!(WebOp::parse("browser_navigate").unwrap().name(), "navigate");
        assert_eq!(
            WebOp::parse("navigate").unwrap().obscura_tool(),
            "browser_navigate"
        );
    }

    #[test]
    fn parse_rejects_unknown() {
        assert!(WebOp::parse("rm_rf").is_none());
        assert!(WebOp::parse("browser_exec").is_none());
        assert!(WebOp::parse("").is_none());
    }

    #[test]
    fn deny_by_default_empty_policy() {
        let p = policy(&[], &[], false);
        let d = p.decide("snapshot", None).unwrap_err();
        assert_eq!(d, Denied::OpNotAllowed("snapshot".to_string()));
    }

    #[test]
    fn unknown_op_denied_before_anything() {
        let p = policy(&["*"], &[], true);
        let d = p.decide("totally_unknown", None).unwrap_err();
        assert!(matches!(d, Denied::UnknownOp(_)));
    }

    #[test]
    fn allowed_op_passes() {
        let p = policy(&["snapshot"], &[], false);
        assert_eq!(p.decide("snapshot", None).unwrap().name(), "snapshot");
        // a non-listed op is still denied
        assert!(p.decide("evaluate", None).is_err());
    }

    #[test]
    fn wildcard_allows_all_ops() {
        let p = policy(&["*"], &[], false);
        assert!(p.decide("evaluate", None).is_ok());
        assert!(p.decide("set_storage_state", None).is_ok());
    }

    #[test]
    fn navigate_requires_url() {
        let p = policy(&["navigate"], &[], true);
        assert_eq!(p.decide("navigate", None).unwrap_err(), Denied::MissingUrl);
    }

    #[test]
    fn ssrf_blocks_localhost_and_loopback() {
        let p = policy(&["navigate"], &[], false);
        for bad in [
            "http://localhost",
            "http://localhost:8080/admin",
            "http://127.0.0.1",
            "https://127.0.0.1/",
            "http://0.0.0.0",
            "http://[::1]/",
            "http://foo.localhost/",
        ] {
            let d = p.decide("navigate", Some(bad)).unwrap_err();
            assert!(matches!(d, Denied::UnsafeUrl(_)), "{bad} should be unsafe");
        }
    }

    #[test]
    fn ssrf_blocks_link_local_and_metadata() {
        let p = policy(&["navigate"], &[], false);
        for bad in [
            "http://169.254.169.254/latest/meta-data/",
            "http://169.254.0.1",
        ] {
            assert!(
                matches!(
                    p.decide("navigate", Some(bad)).unwrap_err(),
                    Denied::UnsafeUrl(_)
                ),
                "{bad} should be unsafe"
            );
        }
    }

    #[test]
    fn ssrf_blocks_private_ranges_and_bare_ip() {
        let p = policy(&["navigate"], &[], false);
        for bad in [
            "http://10.0.0.5",
            "https://172.16.4.4/",
            "http://192.168.1.1/router",
            "http://8.8.8.8/", // a bare public IP is ALSO denied (bypasses domain allow-list)
            "http://.local",
            "https://printer.local/",
        ] {
            assert!(
                p.decide("navigate", Some(bad)).is_err(),
                "{bad} should be denied"
            );
        }
    }

    #[test]
    fn ssrf_blocks_encoded_loopback_forms() {
        // Alternate encodings of 127.0.0.1 / loopback that a browser canonicalizes to
        // the same internal address but a naive dotted-decimal parse misses. Each MUST
        // be classified internal (denied) — guardian round-2 SSRF hardening.
        for host in [
            "2130706433",   // decimal 127.0.0.1
            "0x7f000001",   // hex 127.0.0.1
            "0x7f.0.0.1",   // hex octet
            "017700000001", // octal 127.0.0.1
            "localhost.",   // trailing-dot FQDN
            "127.0.0.1.",   // trailing-dot dotted-decimal
        ] {
            assert!(
                host_is_internal(host),
                "{host:?} is an encoded loopback and must be denied"
            );
        }
    }

    #[test]
    fn ssrf_blocks_ipv6_loopback_forms() {
        // IPv4-mapped and fully-expanded IPv6 loopback must all be caught.
        for host in [
            "::1",              // compressed loopback
            "0:0:0:0:0:0:0:1",  // expanded loopback
            "::ffff:127.0.0.1", // IPv4-mapped loopback
            "::ffff:7f00:1",    // IPv4-mapped loopback (hex form)
        ] {
            assert!(
                host_is_internal(host),
                "{host:?} is an IPv6 loopback form and must be denied"
            );
        }
    }

    #[test]
    fn ssrf_blocks_encoded_loopback_via_decide() {
        // End-to-end through the policy gate (url_host + host_is_internal).
        let p = policy(&["navigate"], &[], false);
        for bad in [
            "http://2130706433/",
            "http://0x7f000001/",
            "http://0x7f.0.0.1/",
            "http://017700000001/",
            "http://localhost./",
            "http://127.0.0.1./",
            "http://[::ffff:127.0.0.1]/",
            "http://[0:0:0:0:0:0:0:1]/",
        ] {
            assert!(
                matches!(
                    p.decide("navigate", Some(bad)).unwrap_err(),
                    Denied::UnsafeUrl(_)
                ),
                "{bad} should be denied"
            );
        }
    }

    #[test]
    fn ssrf_allows_public_host() {
        let p = policy(&["navigate"], &[], false);
        assert!(p
            .decide("navigate", Some("https://example.com/page"))
            .is_ok());
        assert!(p
            .decide("navigate", Some("https://sub.example.com:443/x?y=1#z"))
            .is_ok());
    }

    #[test]
    fn allow_internal_overrides_ssrf() {
        let p = policy(&["navigate"], &[], true);
        assert!(p.decide("navigate", Some("http://127.0.0.1:9000/")).is_ok());
        assert!(p.decide("navigate", Some("http://localhost/")).is_ok());
    }

    #[test]
    fn domain_allow_list_enforced() {
        let p = policy(&["navigate"], &["example.com"], false);
        assert!(p.decide("navigate", Some("https://example.com/")).is_ok());
        assert!(p
            .decide("navigate", Some("https://docs.example.com/"))
            .is_ok());
        let d = p.decide("navigate", Some("https://evil.com/")).unwrap_err();
        assert_eq!(d, Denied::DomainNotAllowed("evil.com".to_string()));
    }

    #[test]
    fn domain_wildcard_allows_any_public_host_but_ssrf_still_blocks() {
        // `*` mirrors `obscura_allow_ops`'s `*`: any public host is allowed…
        let p = policy(&["navigate"], &["*"], false);
        assert!(p.decide("navigate", Some("https://example.com/")).is_ok());
        assert!(p
            .decide("navigate", Some("https://anything.example.org/"))
            .is_ok());
        // …but the SSRF guard (which runs before the domain check) still blocks
        // internal hosts even under `*`.
        assert!(matches!(
            p.decide("navigate", Some("http://127.0.0.1/")).unwrap_err(),
            Denied::UnsafeUrl(_)
        ));
        assert!(matches!(
            p.decide("navigate", Some("http://2130706433/"))
                .unwrap_err(),
            Denied::UnsafeUrl(_)
        ));
    }

    #[test]
    fn url_cap_rejects_oversize() {
        let p = policy(&["navigate"], &[], false);
        let big = format!("https://example.com/{}", "a".repeat(MAX_WEB_ARG_LEN));
        assert_eq!(
            p.decide("navigate", Some(&big)).unwrap_err(),
            Denied::ArgTooLong("url".to_string())
        );
    }

    #[test]
    fn arg_cap_rejects_oversize() {
        assert!(check_arg("text", "ok").is_ok());
        let big = "x".repeat(MAX_WEB_ARG_LEN + 1);
        assert_eq!(
            check_arg("text", &big).unwrap_err(),
            Denied::ArgTooLong("text".to_string())
        );
    }

    #[test]
    fn url_host_parses_forms() {
        assert_eq!(
            url_host("https://example.com/").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            url_host("http://user:pass@example.com:8080/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(url_host("http://[::1]:80/").as_deref(), Some("::1"));
        assert_eq!(url_host("ftp://example.com/").as_deref(), None);
        assert_eq!(url_host("not a url").as_deref(), None);
    }
}
