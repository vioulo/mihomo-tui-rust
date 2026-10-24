//! Subscription format sniffing.
//!
//! A subscription URL hands back one of two shapes: a full Clash/mihomo
//! config (top-level YAML mapping carrying a `proxies` sequence), or a bare
//! list of share links, optionally base64-wrapped. [`parse_subscription`]
//! sniffs which shape it got and returns the parsed mapping together with an
//! [`ImportReport`], so the UI can show what an import actually brought in —
//! node count, protocol mix, and whatever had to be skipped.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use serde_yaml_ng::{Sequence, Value};

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
/// a mihomo client would export, then base64-wrapped link lists, then bare
/// link lists. The base64 and share-link branches land with the link
/// parsers (task 4); until then anything that is not Clash YAML is rejected
/// with a structured error naming the reason.
pub fn parse_subscription(input: &str) -> Result<ParsedSubscription> {
    let yaml: Value = serde_yaml_ng::from_str(input).map_err(|err| {
        anyhow!("not a Clash/mihomo subscription: input is not valid YAML: {err}")
    })?;

    let Value::Mapping(mapping) = yaml else {
        return Err(anyhow!(
            "not a Clash/mihomo subscription: top-level YAML value is not a mapping"
        ));
    };

    // The `proxies` sequence is what makes a mapping a subscription config
    // rather than any other YAML document. Summarize before moving the
    // mapping into the result.
    let Some(Value::Sequence(proxies)) = mapping.get("proxies") else {
        return Err(anyhow!(
            "not a Clash/mihomo subscription: top-level mapping has no proxies sequence"
        ));
    };
    let report = summarize_proxies(proxies);

    Ok(ParsedSubscription { mapping, report })
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
fn summarize(report: &ImportReport) -> String {
    report
        .protocols
        .iter()
        .map(|(kind, count)| format!("{kind}({count})"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
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
}
