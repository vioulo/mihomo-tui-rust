//! Subscription download, format sniffing, and share-link parsing.
//!
//! A subscription URL hands back one of three shapes: a full Clash/mihomo
//! config (top-level YAML mapping carrying a `proxies` sequence), a
//! base64-wrapped list of share links, or a bare list of share links with
//! one link per line. [`parse_subscription`] sniffs which shape it got and
//! returns the parsed mapping together with an [`ImportReport`], so the UI
//! can show what an import actually brought in — node count, protocol mix,
//! and whatever had to be skipped. Share links are parsed for VLESS, VMess,
//! Trojan, Hysteria2 (including the `hy2://` alias) and AnyTLS, the v1
//! converter protocol list. Links with any other scheme are recorded in the
//! report as skipped, never fatal: the link-list shape sniffs on the generic
//! `scheme://` form, not on the supported list.
//!
//! On top of the parsers sit the two production entry points:
//! [`download`] fetches a subscription URL, and [`import_from_raw`] turns
//! raw text into a stored profile — parse first, persist second, so a
//! subscription that cannot be read never leaves files behind.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use serde_yaml_ng::{Mapping, Sequence, Value};
use url::Url;

use crate::paths::Paths;
use crate::store::{ProfileMeta, Store};

/// What an import brought in, for toasts and the profile list.
#[derive(Debug, Default)]
pub struct ImportReport {
    /// Nodes parsed successfully.
    pub total_nodes: usize,
    /// Node count per protocol, e.g. `{"vless": 2, "trojan": 1}`. A BTreeMap
    /// so [`summarize`] output is deterministic for tests and display.
    pub protocols: BTreeMap<String, usize>,
    /// Entries that could not be parsed, each with the reason; importing
    /// continues past them so one bad node does not sink the subscription.
    pub skipped: Vec<String>,
}

/// The result of sniffing a subscription: the parsed top-level mapping
/// (ready for the merge stage) plus the report describing its contents.
#[derive(Debug)]
pub struct ParsedSubscription {
    pub mapping: serde_yaml_ng::Mapping,
    pub report: ImportReport,
}

/// Sniff the subscription shape and parse it.
///
/// Order matters: Clash YAML is tried first because it is the native format
/// a mihomo client would export, then base64-wrapped link lists (standard or
/// URL-safe alphabet, padding optional), then bare link lists. Links that
/// cannot be parsed are recorded in the report instead of failing the whole
/// import; a structured error only comes back when no shape matches at all.
pub fn parse_subscription(input: &str) -> Result<ParsedSubscription> {
    let yaml_error = match sniff_clash_yaml(input) {
        YamlSniff::Parsed(parsed) => return Ok(parsed),
        YamlSniff::Final(err) => return Err(err),
        YamlSniff::KeepLooking(err) => err,
    };

    if let Some(parsed) = parse_base64_links(input) {
        return Ok(parsed);
    }
    if let Some(parsed) = parse_link_lines(input) {
        return Ok(parsed);
    }

    match yaml_error {
        Some(err) => Err(anyhow!(
            "not a Clash/mihomo subscription: input is not Clash YAML, base64 links, \
             or share links ({err})"
        )),
        None => Err(anyhow!(
            "not a Clash/mihomo subscription: input is not Clash YAML, base64 links, \
             or share links"
        )),
    }
}

/// What the YAML sniff decided; see [`sniff_clash_yaml`].
enum YamlSniff {
    /// A Clash/mihomo subscription mapping.
    Parsed(ParsedSubscription),
    /// Valid YAML but not a subscription (a mapping without `proxies`): no
    /// later branch can match a YAML mapping, so the error is final.
    Final(anyhow::Error),
    /// Keep sniffing; carries the YAML parse error for the final message.
    KeepLooking(Option<anyhow::Error>),
}

/// Try the input as a Clash/mihomo YAML subscription.
fn sniff_clash_yaml(input: &str) -> YamlSniff {
    let yaml: Value = match serde_yaml_ng::from_str(input) {
        Ok(yaml) => yaml,
        Err(err) => {
            return YamlSniff::KeepLooking(Some(anyhow!("input is not valid YAML: {err}")));
        }
    };

    let Value::Mapping(mapping) = yaml else {
        // A bare YAML scalar can still be a base64 blob or a link list.
        return YamlSniff::KeepLooking(None);
    };

    let Some(Value::Sequence(proxies)) = mapping.get("proxies") else {
        return YamlSniff::Final(anyhow!(
            "not a Clash/mihomo subscription: top-level mapping has no proxies sequence"
        ));
    };

    // Summarize before moving the mapping into the result.
    let report = summarize_proxies(proxies);
    YamlSniff::Parsed(ParsedSubscription { mapping, report })
}

/// Base64 branch: decode the whole input and treat the plaintext as a link
/// list. `None` when the input is not base64 or decodes to binary garbage.
fn parse_base64_links(input: &str) -> Option<ParsedSubscription> {
    let text = String::from_utf8(decode_base64(input.trim())?).ok()?;
    parse_link_lines(&text)
}

/// Raw link-list branch: one share link per line; blank lines are ignored.
/// Returns `None` when no line carries a share-link shape, so the sniffer
/// can try another shape instead of reporting an empty import.
fn parse_link_lines(text: &str) -> Option<ParsedSubscription> {
    let mut mapping = Mapping::new();
    let mut proxies = Sequence::new();
    let mut report = ImportReport::default();
    let mut saw_link = false;

    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if looks_like_link(line) {
            saw_link = true;
        }
        match parse_link(line) {
            Ok(node) => {
                if let Some(kind) = node.get("type").and_then(Value::as_str) {
                    *report.protocols.entry(kind.to_string()).or_default() += 1;
                    report.total_nodes += 1;
                }
                proxies.push(node);
            }
            // Reasons name the problem, never the link itself: links carry
            // credentials that must not leak into reports and toasts.
            Err(reason) => report.skipped.push(format!("line {}: {reason}", index + 1)),
        }
    }

    if !saw_link {
        return None;
    }
    mapping.insert(Value::from("proxies"), Value::Sequence(proxies));
    Some(ParsedSubscription { mapping, report })
}

/// Whether the line has the shape of *some* share link: a legal URI scheme
/// (RFC 3986: `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`) followed by
/// `://`. The link-list gate is deliberately protocol-agnostic — a list of
/// schemes outside the supported set (`ss://` and friends) is still a list
/// of share links and belongs in the skip report, not in a fatal error —
/// but strictly shaped, so prose and YAML never sniff as links: the scheme
/// must sit at the start of the line (`foo: bar` has no `://`, and
/// `see https://…` mid-sentence fails the shape check).
fn looks_like_link(line: &str) -> bool {
    let Some((scheme, _)) = line.split_once("://") else {
        return false;
    };
    let mut chars = scheme.chars();
    if !chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
    {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

/// Parse one share link into a mihomo proxy mapping. The error carries a
/// human-readable reason (without the link itself) for the skip report.
fn parse_link(line: &str) -> Result<Value, String> {
    let Some((scheme, _)) = line.split_once("://") else {
        return Err("not a share link (no `scheme://`)".to_string());
    };
    match scheme.to_ascii_lowercase().as_str() {
        "vmess" => parse_vmess(&line[scheme.len() + 3..]),
        kind @ ("vless" | "trojan" | "hysteria2" | "hy2" | "anytls") => {
            parse_userinfo_link(line, kind)
        }
        other => Err(format!("unsupported scheme \"{other}\"")),
    }
}

/// Parse a `vmess://` link: base64-wrapped JSON with v2ray-style fields
/// (`ps`/`add`/`port`/`id`/`aid`/`scy`/`net`/`host`/`path`/`tls`/`sni`).
fn parse_vmess(payload: &str) -> Result<Value, String> {
    let decoded = decode_base64(payload.trim()).ok_or("vmess payload is not valid base64")?;
    let text = String::from_utf8(decoded).map_err(|_| "vmess payload is not UTF-8 text")?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| format!("vmess payload is not valid JSON: {err}"))?;
    let obj = json
        .as_object()
        .ok_or("vmess payload is not a JSON object")?;

    let field = |key: &str| {
        obj.get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let server = field("add").ok_or("vmess link is missing its server (`add`)")?;
    let port = json_port(obj.get("port")).ok_or("vmess link is missing its port")?;
    let uuid = field("id").ok_or("vmess link is missing its uuid (`id`)")?;

    // Links without `ps` fall back to the address; `scy` defaults to auto.
    let name = field("ps")
        .map(str::to_string)
        .unwrap_or_else(|| format!("{server}:{port}"));

    let mut node = Mapping::new();
    put(&mut node, "name", name.into());
    put(&mut node, "type", "vmess".into());
    put(&mut node, "server", server.into());
    put(&mut node, "port", Value::from(port));
    put(&mut node, "uuid", uuid.into());
    put(
        &mut node,
        "alterId",
        Value::from(json_port(obj.get("aid")).unwrap_or(0)),
    );
    put(&mut node, "cipher", field("scy").unwrap_or("auto").into());
    if let Some(network) = field("net") {
        put(&mut node, "network", network.into());
        if network == "ws"
            && let Some(opts) = ws_opts(field("path"), field("host"))
        {
            put(&mut node, "ws-opts", opts);
        }
    }
    if field("tls") == Some("tls") {
        put(&mut node, "tls", true.into());
    }
    if let Some(sni) = field("sni") {
        put(&mut node, "servername", sni.into());
    }
    Ok(Value::Mapping(node))
}

/// Parse a URL-shaped share link (`vless`/`trojan`/`hysteria2`/`hy2`/
/// `anytls`): the userinfo carries the credential, query parameters map onto
/// mihomo fields, and the fragment is the display name.
fn parse_userinfo_link(line: &str, kind: &str) -> Result<Value, String> {
    let url = Url::parse(line).map_err(|err| format!("invalid {kind} link: {err}"))?;

    let server = url
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or(format!("{kind} link is missing its server"))?
        .to_string();
    let port = url
        .port()
        .ok_or(format!("{kind} link is missing its port"))?;
    let credential =
        link_credential(&url, kind).ok_or(format!("{kind} link is missing its credential"))?;

    let name = match url.fragment() {
        Some(fragment) if !fragment.is_empty() => percent_decode(fragment),
        _ => format!("{server}:{port}"),
    };

    let mut node = Mapping::new();
    put(&mut node, "name", name.into());
    put(&mut node, "type", node_type(kind).into());
    put(&mut node, "server", server.into());
    put(&mut node, "port", Value::from(u64::from(port)));
    match kind {
        "vless" => put(&mut node, "uuid", credential.into()),
        _ => put(&mut node, "password", credential.into()),
    }

    // Query keys are matched case-insensitively; values arrive decoded.
    let param = |key: &str| {
        url.query_pairs()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.into_owned())
    };

    if param("security").as_deref() == Some("tls") {
        put(&mut node, "tls", true.into());
    }
    if let Some(sni) = param("sni").filter(|sni| !sni.is_empty()) {
        put(&mut node, "servername", sni.into());
    }
    if param("type").as_deref() == Some("ws") {
        put(&mut node, "network", "ws".into());
        if let Some(opts) = ws_opts(
            param("path").filter(|path| !path.is_empty()).as_deref(),
            param("host").filter(|host| !host.is_empty()).as_deref(),
        ) {
            put(&mut node, "ws-opts", opts);
        }
    }
    if flag(param("insecure")) || flag(param("allowInsecure")) {
        put(&mut node, "skip-cert-verify", true.into());
    }
    if kind == "hysteria2" || kind == "hy2" {
        if let Some(obfs) = param("obfs").filter(|obfs| !obfs.is_empty()) {
            put(&mut node, "obfs", obfs.into());
        }
        if let Some(password) = param("obfs-password").filter(|pw| !pw.is_empty()) {
            put(&mut node, "obfs-password", password.into());
        }
    }
    Ok(Value::Mapping(node))
}

/// mihomo knows hysteria2 by its full name; `hy2` is only a link alias.
fn node_type(kind: &str) -> &str {
    if kind == "hy2" { "hysteria2" } else { kind }
}

/// Pull the credential out of the userinfo. VLESS carries a UUID as the
/// plain username; the other protocols carry a password, written either as
/// `user:pass` (after the colon) or bare `pass@` (username slot).
fn link_credential(url: &Url, kind: &str) -> Option<String> {
    if kind == "vless" {
        let user = url.username();
        return (!user.is_empty()).then(|| percent_decode(user));
    }
    if let Some(password) = url.password() {
        return Some(percent_decode(password));
    }
    let user = url.username();
    (!user.is_empty()).then(|| percent_decode(user))
}

/// Truthy query-flag values (`insecure=1`, `allowInsecure=true`).
fn flag(value: Option<String>) -> bool {
    value.is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// `ws-opts` for a ws transport carrying a path and/or Host header.
fn ws_opts(path: Option<&str>, host: Option<&str>) -> Option<Value> {
    if path.is_none() && host.is_none() {
        return None;
    }
    let mut opts = Mapping::new();
    if let Some(path) = path {
        put(&mut opts, "path", path.into());
    }
    if let Some(host) = host {
        let mut headers = Mapping::new();
        put(&mut headers, "Host", host.into());
        put(&mut opts, "headers", Value::Mapping(headers));
    }
    Some(Value::Mapping(opts))
}

/// Insert a string-keyed entry into a proxy mapping.
fn put(map: &mut Mapping, key: &str, value: Value) {
    map.insert(Value::from(key), value);
}

/// vmess JSON ports and alterIds appear as numbers or strings; accept both.
fn json_port(value: Option<&serde_json::Value>) -> Option<u64> {
    match value? {
        serde_json::Value::Number(number) => number.as_u64(),
        serde_json::Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// Try the standard and URL-safe base64 alphabets, with and without
/// padding — subscription providers emit all four shapes.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
        if let Ok(bytes) = engine.decode(text) {
            return Some(bytes);
        }
    }
    None
}

/// Percent-decode a URL component into text; invalid escapes and non-UTF-8
/// sequences pass through unchanged rather than failing the whole node.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some(byte) = decode_escape(bytes, index) {
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| raw.to_string())
}

/// The byte behind a `%XX` escape starting at `bytes[index]`, if valid.
fn decode_escape(bytes: &[u8], index: usize) -> Option<u8> {
    if bytes.get(index) != Some(&b'%') {
        return None;
    }
    let hi = hex_value(*bytes.get(index + 1)?)?;
    let lo = hex_value(*bytes.get(index + 2)?)?;
    Some(hi * 16 + lo)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Count the protocols in a `proxies` sequence; entries without a string
/// `type` are recorded as skipped with their position, so the report says
/// what was dropped instead of silently losing nodes.
fn summarize_proxies(proxies: &Sequence) -> ImportReport {
    let mut report = ImportReport::default();
    for (index, node) in proxies.iter().enumerate() {
        match node.get("type").and_then(Value::as_str) {
            Some(kind) => {
                *report.protocols.entry(kind.to_string()).or_default() += 1;
                report.total_nodes += 1;
            }
            None => report
                .skipped
                .push(format!("proxies[{index}]: missing a string `type` field")),
        }
    }
    report
}

/// One-line summary for the profile list, e.g. `"vless(2), trojan(1)"`.
/// Deterministic: protocols appear in alphabetical order.
pub(crate) fn summarize(report: &ImportReport) -> String {
    report
        .protocols
        .iter()
        .map(|(kind, count)| format!("{kind}({count})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whole-request budget for [`download`]: connect, redirects, and body.
/// A subscription fetch is an interactive UI action, so one total timeout.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest slice of an error body quoted in a failed-download message.
const BODY_SNIPPET_CHARS: usize = 200;

/// Strip the query and fragment so a token-bearing subscription URL can be
/// quoted in errors and toasts without leaking the credential.
fn redacted_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut parsed) => {
            parsed.set_query(None);
            parsed.set_fragment(None);
            parsed.to_string()
        }
        Err(_) => url.to_string(),
    }
}

/// Download a subscription URL and return the body text.
///
/// Any status outside 2xx is an error carrying the status code and the
/// first [`BODY_SNIPPET_CHARS`] characters of the body: providers answer
/// "expired" or "over quota" with human-readable HTML pages, and that text
/// is what tells the user why an import found nothing. Not covered by unit
/// tests on purpose — they must not touch the network — this is verified
/// where it is wired to the UI.
pub async fn download(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .context("building HTTP client")?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {}", redacted_url(url)))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default().trim().to_string();
        let truncated = body.chars().count() > BODY_SNIPPET_CHARS;
        let mut snippet: String = body.chars().take(BODY_SNIPPET_CHARS).collect();
        if truncated {
            snippet.push('…');
        }
        let target = redacted_url(url);
        if snippet.is_empty() {
            bail!("subscription server returned {status} for {target}");
        }
        bail!("subscription server returned {status} for {target}: {snippet}");
    }

    let body = response
        .text()
        .await
        .with_context(|| format!("reading body of {}", redacted_url(url)))?;
    // A subscription is a few kilobytes at most; refuse anything that would
    // exhaust memory within the request budget.
    const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
    if body.len() > MAX_BODY_BYTES {
        bail!("subscription response exceeded {MAX_BODY_BYTES} bytes");
    }
    Ok(body)
}

/// The production import entry: parse `raw`, then hand it to the store.
///
/// The parse runs before anything touches the disk, so a subscription that
/// cannot be read leaves no files behind — [`Store::add_profile`] only ever
/// sees text that already parsed. The meta's `nodes`/`protocols` come from
/// the parse report, and both timestamps are stamped with the current clock
/// (this is the production path; tests assert presence, not values).
///
/// A parse that yields zero usable nodes but recorded skips is a failed
/// import too — every entry was rejected, so storing an empty profile would
/// only move the failure into the UI — and its error carries the skip
/// summary so the user sees why nothing arrived.
pub fn import_from_raw(paths: &Paths, name: &str, url: &str, raw: &str) -> Result<ProfileMeta> {
    let parsed =
        parse_subscription(raw).with_context(|| format!("importing subscription `{name}`"))?;

    if parsed.report.total_nodes == 0 && !parsed.report.skipped.is_empty() {
        bail!(
            "importing subscription `{name}`: 0 nodes, {} skipped: {}",
            parsed.report.skipped.len(),
            parsed.report.skipped.join("; ")
        );
    }

    let now_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_millis(),
    )
    .unwrap_or(u64::MAX);

    Store::add_profile(
        paths,
        name,
        url,
        raw,
        u32::try_from(parsed.report.total_nodes).unwrap_or(u32::MAX),
        &summarize(&parsed.report),
        now_ms,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::paths::Paths;
    use crate::store::Store;

    use super::*;

    /// Minimal but realistic Clash config: three proxies plus a group that
    /// references them.
    const SAMPLE: &str = "\
proxies:
  - name: a
    type: vless
    server: 1.1.1.1
    port: 443
  - name: b
    type: vless
    server: 2.2.2.2
    port: 443
  - name: c
    type: trojan
    server: 3.3.3.3
    port: 443
proxy-groups:
  - name: PROXY
    type: select
    proxies: [a, b, c]
";

    #[test]
    fn detects_clash_yaml_and_reports_protocols() {
        let parsed = parse_subscription(SAMPLE).unwrap();

        // The whole top-level mapping survives for the merge stage.
        let proxies = parsed
            .mapping
            .get("proxies")
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(proxies.len(), 3);
        assert!(parsed.mapping.contains_key("proxy-groups"));

        assert_eq!(parsed.report.total_nodes, 3);
        assert_eq!(parsed.report.protocols.get("vless"), Some(&2));
        assert_eq!(parsed.report.protocols.get("trojan"), Some(&1));
        assert!(parsed.report.skipped.is_empty(), "nothing to skip here");

        // The one-line summary shown in the profile list, deterministic via
        // BTreeMap ordering (format "vless(2), trojan(1)").
        assert_eq!(summarize(&parsed.report), "trojan(1), vless(2)");
    }

    #[test]
    fn invalid_yaml_is_a_structured_error() {
        // A tab cannot start a YAML token and the flow sequence never closes;
        // this must surface as Err with a reason, never a panic.
        let err = parse_subscription("\t不是: yaml: [").unwrap_err();
        assert!(
            err.to_string().contains("YAML"),
            "names the failing format: {err}"
        );
    }

    #[test]
    fn yaml_without_proxies_is_rejected() {
        // A valid mapping that is not a subscription: rejected with the
        // specific reason instead of an empty import.
        let err = parse_subscription("{\"mixed-port\": 7890}").unwrap_err();
        assert!(
            err.to_string().contains("no proxies"),
            "names the problem: {err}"
        );
    }

    // ---- base64 and share-link parsing ----

    /// One link per supported protocol, each carrying the fields the merge
    /// stage relies on (ws transport, TLS/SNI, obfs, credentials).
    fn five_links() -> String {
        let vmess_json = r#"{"v":"2","ps":"vm-节点","add":"vm.example.com","port":"8443","id":"11111111-2222-3333-4444-555555555555","aid":"0","scy":"auto","net":"ws","type":"none","host":"cdn.example.com","path":"/ws","tls":"tls","sni":"vm-sni.example.com"}"#;
        [
            format!("vmess://{}", STANDARD.encode(vmess_json)),
            "vless://11111111-2222-3333-4444-555555555555@vl.example.com:8443?security=tls&sni=vl-sni.example.com&type=ws&path=%2Fws&host=vl-cdn.example.com#vl-node".to_string(),
            "trojan://tpw@tj.example.com:8443?security=tls&sni=tj-sni.example.com#tj-node"
                .to_string(),
            "hysteria2://hpw@hy.example.com:8443?obfs=salamander&obfs-password=hy-secret&insecure=1#hy-node"
                .to_string(),
            "anytls://apw@at.example.com:8443?insecure=1#at-node".to_string(),
        ]
        .join("\n")
    }

    fn proxies_of(parsed: &ParsedSubscription) -> &Sequence {
        parsed
            .mapping
            .get("proxies")
            .unwrap()
            .as_sequence()
            .unwrap()
    }

    fn field<'a>(node: &'a Value, key: &str) -> &'a Value {
        node.get(key)
            .unwrap_or_else(|| panic!("missing `{key}` field"))
    }

    fn text<'a>(node: &'a Value, key: &str) -> &'a str {
        field(node, key)
            .as_str()
            .unwrap_or_else(|| panic!("`{key}` is not a string"))
    }

    #[test]
    fn parses_base64_list_with_all_five_protocols() {
        let links = five_links();
        // Standard alphabet, URL-safe alphabet, and padding-free: all three
        // encodings must sniff and decode to the same five nodes.
        let blobs = [
            ("standard", STANDARD.encode(&links)),
            ("url-safe", URL_SAFE.encode(&links)),
            ("url-safe-no-pad", URL_SAFE_NO_PAD.encode(&links)),
        ];
        for (label, blob) in blobs {
            let parsed = parse_subscription(&blob).unwrap_or_else(|err| panic!("{label}: {err}"));
            assert_eq!(parsed.report.total_nodes, 5, "{label}");
            assert!(
                parsed.report.skipped.is_empty(),
                "{label}: {:?}",
                parsed.report.skipped
            );
            for (kind, count) in [
                ("vmess", 1),
                ("vless", 1),
                ("trojan", 1),
                ("hysteria2", 1),
                ("anytls", 1),
            ] {
                assert_eq!(parsed.report.protocols.get(kind), Some(&count), "{label}");
            }

            let proxies = proxies_of(&parsed);

            let vm = &proxies[0];
            assert_eq!(text(vm, "name"), "vm-节点", "{label}");
            assert_eq!(text(vm, "type"), "vmess");
            assert_eq!(text(vm, "server"), "vm.example.com");
            assert_eq!(field(vm, "port").as_u64(), Some(8443));
            assert_eq!(text(vm, "uuid"), "11111111-2222-3333-4444-555555555555");
            assert_eq!(field(vm, "alterId").as_u64(), Some(0));
            assert_eq!(text(vm, "cipher"), "auto");
            assert_eq!(text(vm, "network"), "ws");
            assert_eq!(field(vm, "tls").as_bool(), Some(true));
            assert_eq!(text(vm, "servername"), "vm-sni.example.com");
            assert_eq!(text(field(vm, "ws-opts"), "path"), "/ws");
            assert_eq!(
                text(field(field(vm, "ws-opts"), "headers"), "Host"),
                "cdn.example.com"
            );

            let vl = &proxies[1];
            assert_eq!(text(vl, "type"), "vless");
            assert_eq!(text(vl, "server"), "vl.example.com");
            assert_eq!(field(vl, "port").as_u64(), Some(8443));
            assert_eq!(text(vl, "uuid"), "11111111-2222-3333-4444-555555555555");
            assert_eq!(field(vl, "tls").as_bool(), Some(true));
            assert_eq!(text(vl, "servername"), "vl-sni.example.com");
            assert_eq!(text(vl, "network"), "ws");
            assert_eq!(text(field(vl, "ws-opts"), "path"), "/ws");
            assert_eq!(
                text(field(field(vl, "ws-opts"), "headers"), "Host"),
                "vl-cdn.example.com"
            );

            let tj = &proxies[2];
            assert_eq!(text(tj, "type"), "trojan");
            assert_eq!(text(tj, "server"), "tj.example.com");
            assert_eq!(field(tj, "port").as_u64(), Some(8443));
            assert_eq!(text(tj, "password"), "tpw");

            let hy = &proxies[3];
            assert_eq!(text(hy, "type"), "hysteria2");
            assert_eq!(text(hy, "password"), "hpw");
            assert_eq!(text(hy, "obfs"), "salamander");
            assert_eq!(text(hy, "obfs-password"), "hy-secret");
            assert_eq!(field(hy, "skip-cert-verify").as_bool(), Some(true));

            let at = &proxies[4];
            assert_eq!(text(at, "type"), "anytls");
            assert_eq!(text(at, "password"), "apw");
            assert_eq!(field(at, "skip-cert-verify").as_bool(), Some(true));
        }
    }

    #[test]
    fn link_names_come_from_fragment_and_are_percent_decoded() {
        let named = parse_link("trojan://pw@h:1#🇭🇰%20HK%2001").unwrap();
        assert_eq!(text(&named, "name"), "🇭🇰 HK 01");

        // No fragment: the name falls back to `server:port`.
        let unnamed = parse_link("vless://11111111-2222-3333-4444-555555555555@h:1").unwrap();
        assert_eq!(text(&unnamed, "name"), "h:1");
    }

    #[test]
    fn unsupported_scheme_and_missing_fields_are_skipped_not_fatal() {
        let input = "\
ss://YWVzLTEyODpnY21AdGVzdA@ss.example.com:443#ss-node
vless://11111111-2222-3333-4444-555555555556@9.9.9.9#no-port
trojan://ok@8.8.8.8:443?security=tls#kept-node
";
        let parsed = parse_subscription(input).unwrap();

        // The trojan node survives; the other two are reported, not fatal.
        assert_eq!(parsed.report.total_nodes, 1);
        assert_eq!(parsed.report.protocols.get("trojan"), Some(&1));
        let proxies = proxies_of(&parsed);
        assert_eq!(proxies.len(), 1);
        assert_eq!(text(&proxies[0], "name"), "kept-node");

        assert_eq!(
            parsed.report.skipped.len(),
            2,
            "{:?}",
            parsed.report.skipped
        );
        assert!(
            parsed
                .report
                .skipped
                .iter()
                .any(|reason| reason.contains("ss")),
            "names the unsupported scheme: {:?}",
            parsed.report.skipped
        );
        assert!(
            parsed
                .report
                .skipped
                .iter()
                .any(|reason| reason.contains("port")),
            "names the missing port: {:?}",
            parsed.report.skipped
        );
    }

    #[test]
    fn pure_unknown_scheme_list_is_ok_and_all_skipped() {
        // A list of `ss://` lines is still a share-link list: the link-list
        // gate is any `scheme://` shape, so this imports as Ok with every
        // line recorded in `skipped` — never the fatal "not share links"
        // error, and never an empty-handed Err.
        let input = "\
ss://YWVzLTEyODpnY21AdGVzdA@ss.example.com:443#ss-one
ss://YWVzLTEyODpnY21AdGVzdA@ss2.example.com:443#ss-two
";
        let parsed = parse_subscription(input).unwrap();

        assert_eq!(parsed.report.total_nodes, 0);
        assert!(parsed.report.protocols.is_empty(), "no known protocol");
        let proxies = proxies_of(&parsed);
        assert!(proxies.is_empty(), "nothing parsed, proxies stay empty");

        assert_eq!(
            parsed.report.skipped.len(),
            2,
            "{:?}",
            parsed.report.skipped
        );
        for (index, reason) in parsed.report.skipped.iter().enumerate() {
            assert!(
                reason.starts_with(&format!("line {}: ", index + 1))
                    && reason.contains("unsupported scheme")
                    && reason.contains("ss"),
                "names the line and the scheme: {reason}"
            );
        }
    }

    #[test]
    fn hy2_alias_parses_as_hysteria2() {
        let node = parse_link("hy2://pw@h:1?obfs=salamander&obfs-password=op#名").unwrap();
        assert_eq!(text(&node, "type"), "hysteria2");
        assert_eq!(text(&node, "password"), "pw");
        assert_eq!(text(&node, "obfs"), "salamander");
        assert_eq!(text(&node, "obfs-password"), "op");
        assert_eq!(text(&node, "name"), "名");
    }

    // ---- import flow: parse → Store::add_profile ----

    /// A throwaway config root with the directory layout already created,
    /// mirroring what production does once at startup.
    fn paths() -> (TempDir, Paths) {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::resolve(Some(tmp.path().to_str().unwrap())).unwrap();
        paths.ensure().unwrap();
        (tmp, paths)
    }

    #[tokio::test]
    async fn import_from_raw_stores_snapshot_and_meta() {
        let (_tmp, paths) = paths();

        let profile = import_from_raw(&paths, "甲", "https://sub.example", SAMPLE).unwrap();

        // The meta comes from the parse report; the clock is real, so only
        // its presence is asserted, never a concrete timestamp.
        assert_eq!(profile.id, "s1");
        assert_eq!(profile.name, "甲");
        assert_eq!(profile.url, "https://sub.example");
        assert_eq!(profile.nodes, 3);
        assert_eq!(profile.protocols, "trojan(1), vless(2)");
        assert!(profile.created_ms > 0, "stamped from the current clock");
        assert_eq!(
            profile.created_ms, profile.updated_ms,
            "a fresh import stamps both"
        );
        assert!(profile.policy.is_empty());

        // The snapshot holds the exact downloaded text.
        assert_eq!(Store::read_snapshot(&paths, &profile.id).unwrap(), SAMPLE);

        // And the profile shows up in the index.
        let profiles = Store::load_profiles(&paths).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, profile.id);
        assert_eq!(profiles[0].nodes, 3);
        assert_eq!(profiles[0].protocols, "trojan(1), vless(2)");
    }

    #[tokio::test]
    async fn failed_parse_leaves_no_files_behind() {
        let (_tmp, paths) = paths();

        let err = import_from_raw(&paths, "坏", "https://sub.example", "plain prose, no links")
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("importing subscription") && message.contains("not a Clash/mihomo"),
            "names the profile and the failing format: {message}"
        );

        // Nothing reached the disk: no index, no snapshot.
        assert!(
            !paths.profiles_json().exists(),
            "profiles.json was never created"
        );
        assert_eq!(
            fs::read_dir(paths.subscriptions_dir()).unwrap().count(),
            0,
            "no snapshot files either"
        );
    }

    #[tokio::test]
    async fn all_entries_skipped_fails_with_the_skip_summary() {
        // A parse that yields zero usable nodes is a failed import too, and
        // its error carries the skip report — this is the only parse failure
        // that has a report to show.
        let (_tmp, paths) = paths();

        let raw = "\
ss://YWVzLTEyODpnY21AdGVzdA@ss.example.com:443#one
ss://YWVzLTEyODpnY21AdGVzdA@ss2.example.com:443#two
";
        let err = import_from_raw(&paths, "全跳", "https://sub.example", raw).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("skipped"), "reports the skips: {message}");
        assert!(
            message.contains(r#"unsupported scheme "ss""#) && message.contains("line 1"),
            "carries the skip reasons: {message}"
        );

        // Still nothing on disk.
        assert!(!paths.profiles_json().exists());
        assert_eq!(fs::read_dir(paths.subscriptions_dir()).unwrap().count(), 0);
    }

    #[test]
    fn hostile_input_never_panics() {
        // Empty and non-link text: structured errors, never a panic.
        assert!(parse_subscription("").is_err());
        assert!(parse_subscription("AAAA").is_err());

        // 1 MB of pseudo-random text must fail (or come back all-skipped)
        // promptly instead of panicking or spinning.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut binary = String::with_capacity(1 << 20);
        for _ in 0..(1 << 20) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            binary.push(char::from_u32((state & 0x10FFFF) as u32).unwrap_or('\u{FFFD}'));
        }
        if let Ok(parsed) = parse_subscription(&binary) {
            assert_eq!(
                parsed.report.total_nodes, 0,
                "random text cannot yield nodes"
            );
        }

        // A single 1 MB line with no scheme in it.
        let long_line = "a".repeat(1 << 20);
        if let Ok(parsed) = parse_subscription(&long_line) {
            assert_eq!(parsed.report.total_nodes, 0);
        }
    }
}
