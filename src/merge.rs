//! The configuration block model and the per-block diff behind a switch.
//!
//! A mihomo config is compared as ten named [`Block`]s — proxies, proxy
//! groups, rules, DNS, TUN, the five inbound port keys, and the four small
//! scalars. [`diff_blocks`] compares the base config against a subscription
//! block by block and yields one [`BlockDiff`] per block with a short,
//! deterministic summary for each side, so the confirm page can show what a
//! switch would change without dumping YAML into the terminal.
//!
//! [`Source`] is which side a block should come from when merging (task 7).
//! profiles.json persists choices as `Block::key()` → `Source::label()`
//! strings ("rules" → "subscription"), so both enums carry round-trip
//! conversions ([`Block::from_key`], [`Source::from_label`]) that return
//! `None` for unknown text — a hand-edited profiles.json must never panic.
//!
//! Summaries are deliberately lossy ("3 nodes" says nothing about *which*
//! nodes), so [`BlockDiff::equal`] is computed from the full YAML values and
//! is the only authority on whether a block matches. A YAML `null` counts as
//! absent: an empty `dns:` and no `dns:` at all mean the same thing to
//! mihomo and must not show up as a phantom diff.

use serde_yaml_ng::{Mapping, Value};

/// One named region of a mihomo config, listed in the order the diff and the
/// confirm page present them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Block {
    Proxies,
    ProxyGroups,
    Rules,
    Dns,
    Tun,
    Inbounds,
    LogLevel,
    AllowLan,
    Ipv6,
    Mode,
}

impl Block {
    /// Every block — a diff always covers exactly these, in this order.
    pub const ALL: [Block; 10] = [
        Block::Proxies,
        Block::ProxyGroups,
        Block::Rules,
        Block::Dns,
        Block::Tun,
        Block::Inbounds,
        Block::LogLevel,
        Block::AllowLan,
        Block::Ipv6,
        Block::Mode,
    ];

    /// The top-level config keys this block reads. [`Block::Inbounds`] spans
    /// the five port keys; every other block owns exactly one.
    pub fn keys(self) -> &'static [&'static str] {
        match self {
            Block::Proxies => &["proxies"],
            Block::ProxyGroups => &["proxy-groups"],
            Block::Rules => &["rules"],
            Block::Dns => &["dns"],
            Block::Tun => &["tun"],
            Block::Inbounds => &[
                "port",
                "mixed-port",
                "socks-port",
                "redir-port",
                "tproxy-port",
            ],
            Block::LogLevel => &["log-level"],
            Block::AllowLan => &["allow-lan"],
            Block::Ipv6 => &["ipv6"],
            Block::Mode => &["mode"],
        }
    }

    /// The label shown on the confirm page.
    pub fn title(self) -> &'static str {
        match self {
            Block::Proxies => "Proxies",
            Block::ProxyGroups => "Proxy groups",
            Block::Rules => "Rules",
            Block::Dns => "DNS",
            Block::Tun => "TUN",
            Block::Inbounds => "Inbound ports",
            Block::LogLevel => "Log level",
            Block::AllowLan => "Allow LAN",
            Block::Ipv6 => "IPv6",
            Block::Mode => "Mode",
        }
    }

    /// The stable identifier profiles.json stores a per-block [`Source`]
    /// choice under (`policy: {rules: subscription}`). [`Block::Inbounds`]
    /// has no single top-level key to borrow, so it gets its own name.
    pub fn key(self) -> &'static str {
        match self {
            Block::Proxies => "proxies",
            Block::ProxyGroups => "proxy-groups",
            Block::Rules => "rules",
            Block::Dns => "dns",
            Block::Tun => "tun",
            Block::Inbounds => "inbounds",
            Block::LogLevel => "log-level",
            Block::AllowLan => "allow-lan",
            Block::Ipv6 => "ipv6",
            Block::Mode => "mode",
        }
    }

    /// The block behind a stored policy key, or `None` for unknown text.
    pub fn from_key(key: &str) -> Option<Block> {
        Block::ALL.into_iter().find(|block| block.key() == key)
    }
}

/// Which side of the diff a block should come from when merging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Base,
    Subscription,
}

impl Source {
    /// The string stored in profiles.json and shown on the confirm toggle.
    pub fn label(self) -> &'static str {
        match self {
            Source::Base => "base",
            Source::Subscription => "subscription",
        }
    }

    /// The other side — the confirm page's per-block toggle.
    pub fn toggle(self) -> Source {
        match self {
            Source::Base => Source::Subscription,
            Source::Subscription => Source::Base,
        }
    }

    /// The source behind a stored label, or `None` for unknown text.
    pub fn from_label(label: &str) -> Option<Source> {
        match label {
            "base" => Some(Source::Base),
            "subscription" => Some(Source::Subscription),
            _ => None,
        }
    }
}

/// One block compared between the base config and a subscription.
///
/// `base`/`sub` hold one-line summaries; `None` means that side has no such
/// block (rendered as "-"). Summaries are lossy, so `equal` — computed from
/// the full YAML values — is the authority on whether the blocks match.
#[derive(Debug, Clone)]
pub struct BlockDiff {
    pub block: Block,
    pub base: Option<String>,
    pub sub: Option<String>,
    pub equal: bool,
}

/// Compare the base config against the subscription block by block, in
/// [`Block::ALL`] order — always one [`BlockDiff`] per block, all ten.
pub fn diff_blocks(base: &Mapping, sub: &Mapping) -> Vec<BlockDiff> {
    Block::ALL
        .into_iter()
        .map(|block| BlockDiff {
            block,
            base: side_summary(block, base),
            sub: side_summary(block, sub),
            equal: block_equal(block, base, sub),
        })
        .collect()
}

/// Whether any block differs — the gate for offering a switch confirm at all.
pub fn has_diff(diffs: &[BlockDiff]) -> bool {
    diffs.iter().any(|diff| !diff.equal)
}

/// The one-line summary for one side, or `None` when the block is absent
/// there; the confirm page renders `None` as "-".
fn side_summary(block: Block, map: &Mapping) -> Option<String> {
    if block == Block::Inbounds {
        // The block spans five port keys; bundle the ones present into a
        // scratch mapping so `summarize` can count them.
        let mut ports = Mapping::new();
        for key in block.keys() {
            if let Some(value) = block_value(map, key) {
                ports.insert(Value::from(*key), value.clone());
            }
        }
        return (!ports.is_empty()).then(|| summarize(block, Some(&Value::Mapping(ports))));
    }
    block_value(map, block.keys()[0]).map(|value| summarize(block, Some(value)))
}

/// Exact per-key equality: every key of the block must carry an equal YAML
/// value on both sides, with missing == missing.
fn block_equal(block: Block, base: &Mapping, sub: &Mapping) -> bool {
    block
        .keys()
        .iter()
        .all(|key| block_value(base, key) == block_value(sub, key))
}

/// The top-level value behind one config key, with a YAML `null` treated as
/// absent: an empty `dns:` and no `dns:` at all mean the same thing to
/// mihomo, so they must not show up as a phantom diff.
fn block_value<'a>(map: &'a Mapping, key: &str) -> Option<&'a Value> {
    map.get(key).filter(|value| !value.is_null())
}

/// One-line, deterministic summary of a block's value for the diff table:
/// sequences count their entries ("3 nodes", "1 rule"), scalar blocks print
/// their value, structural blocks just say "present", and an absent block is
/// "-". Never quotes node or group names — they can be long and non-ASCII;
/// the counts are what a switch decision needs.
fn summarize(block: Block, value: Option<&Value>) -> String {
    let Some(value) = value else {
        return "-".to_string();
    };
    match block {
        Block::Proxies => counted(value, "node"),
        Block::ProxyGroups => counted(value, "group"),
        Block::Rules => counted(value, "rule"),
        Block::Inbounds => match value.as_mapping() {
            Some(ports) => plural(ports.len(), "port"),
            None => "present".to_string(),
        },
        Block::LogLevel | Block::AllowLan | Block::Ipv6 | Block::Mode => scalar_text(value),
        Block::Dns | Block::Tun => "present".to_string(),
    }
}

/// "3 rules" for a sequence, "present" for anything else.
fn counted(value: &Value, noun: &str) -> String {
    match value.as_sequence() {
        Some(seq) => plural(seq.len(), noun),
        None => "present".to_string(),
    }
}

/// "{count} {noun}" with plain-English pluralization ("1 rule", "3 rules").
fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// The text of a scalar value ("rule", "true", "7890"); anything structural
/// collapses to "present".
fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        _ => "present".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a YAML mapping for the diff tests; panics on anything else so a
    /// broken fixture fails loudly.
    fn map_yaml(text: &str) -> Mapping {
        match serde_yaml_ng::from_str::<Value>(text).unwrap() {
            Value::Mapping(map) => map,
            other => panic!("fixture must be a mapping, got {other:?}"),
        }
    }

    fn find(diffs: &[BlockDiff], block: Block) -> &BlockDiff {
        diffs
            .iter()
            .find(|diff| diff.block == block)
            .unwrap_or_else(|| panic!("no diff entry for {block:?}"))
    }

    #[test]
    fn block_keys_cover_the_whole_config() {
        assert_eq!(
            Block::Inbounds.keys(),
            [
                "port",
                "mixed-port",
                "socks-port",
                "redir-port",
                "tproxy-port"
            ]
            .as_slice(),
        );

        // ALL is exactly the ten variants, in declaration order.
        assert_eq!(
            Block::ALL,
            [
                Block::Proxies,
                Block::ProxyGroups,
                Block::Rules,
                Block::Dns,
                Block::Tun,
                Block::Inbounds,
                Block::LogLevel,
                Block::AllowLan,
                Block::Ipv6,
                Block::Mode,
            ],
        );

        // Every block round-trips through its policy key, and every block
        // except Inbounds owns exactly the one config key it is named after.
        for block in Block::ALL {
            assert_eq!(Block::from_key(block.key()), Some(block), "{}", block.key());
            if block == Block::Inbounds {
                assert_eq!(block.key(), "inbounds");
            } else {
                assert_eq!(block.keys(), [block.key()].as_slice());
            }
        }
        assert_eq!(Block::from_key("nope"), None);
    }

    #[test]
    fn sources_label_toggle_and_round_trip() {
        assert_eq!(Source::Base.label(), "base");
        assert_eq!(Source::Subscription.label(), "subscription");
        assert_eq!(Source::Base.toggle(), Source::Subscription);
        assert_eq!(Source::Subscription.toggle(), Source::Base);
        for source in [Source::Base, Source::Subscription] {
            assert_eq!(Source::from_label(source.label()), Some(source));
        }
        assert_eq!(Source::from_label("nope"), None);
    }

    #[test]
    fn diff_reports_equal_and_unequal_blocks() {
        let base = map_yaml(
            "proxies: [{name: a}, {name: b}, {name: c}]\n\
             rules:\n\
               - MATCH,PROXY\n\
               - GEOIP,CN\n\
               - MATCH,DIRECT\n",
        );
        let sub = map_yaml("proxies: [{name: a}, {name: b}, {name: c}]\nrules:\n  - MATCH,PROXY\n");

        let diffs = diff_blocks(&base, &sub);

        // One entry per block, in Block::ALL order — never a sparse list.
        assert_eq!(diffs.len(), Block::ALL.len());
        assert_eq!(
            diffs.iter().map(|diff| diff.block).collect::<Vec<_>>(),
            Block::ALL.to_vec(),
        );

        let proxies = find(&diffs, Block::Proxies);
        assert!(proxies.equal, "identical proxies are equal");
        assert_eq!(proxies.base.as_deref(), Some("3 nodes"));
        assert_eq!(proxies.sub.as_deref(), Some("3 nodes"));

        let rules = find(&diffs, Block::Rules);
        assert!(!rules.equal, "different rule lists are a diff");
        assert_eq!(rules.base.as_deref(), Some("3 rules"));
        assert_eq!(rules.sub.as_deref(), Some("1 rule"));
        assert!(has_diff(&diffs));
    }

    #[test]
    fn missing_on_one_side_is_a_diff() {
        let base = Mapping::new();
        let sub = map_yaml("dns:\n  enable: true\n  nameserver: [1.1.1.1]\n");

        let diffs = diff_blocks(&base, &sub);
        let dns = find(&diffs, Block::Dns);

        assert!(!dns.equal, "present on one side only is a diff");
        assert_eq!(dns.base, None, "the absent side has no summary");
        assert_eq!(dns.sub.as_deref(), Some("present"));
        assert!(has_diff(&diffs));

        // The renderer turns an absent side into the "-" placeholder.
        assert_eq!(summarize(Block::Dns, None), "-");
    }

    #[test]
    fn identical_configs_have_no_diff() {
        let config =
            map_yaml("mixed-port: 7890\nlog-level: info\nmode: rule\ndns: {enable: true}\n");

        let diffs = diff_blocks(&config, &config);
        assert!(diffs.iter().all(|diff| diff.equal));
        assert!(!has_diff(&diffs), "identical configs are all equal");

        // Missing == missing: an absent block on both sides invents no diff.
        let tun = find(&diffs, Block::Tun);
        assert!(tun.equal);
        assert_eq!(tun.base, None);
        assert_eq!(tun.sub, None);

        // Scalar blocks summarize as their value.
        assert_eq!(find(&diffs, Block::Mode).base.as_deref(), Some("rule"));
        assert_eq!(find(&diffs, Block::LogLevel).base.as_deref(), Some("info"));
        assert_eq!(find(&diffs, Block::AllowLan).base, None);
    }

    #[test]
    fn a_null_block_means_absent_not_a_diff() {
        // An empty `dns:` and no `dns:` at all are the same thing to mihomo;
        // the diff must not manufacture noise out of YAML nulls.
        let base = map_yaml("mode: rule\ndns:\n");
        let sub = map_yaml("mode: rule\n");

        let diffs = diff_blocks(&base, &sub);
        let dns = find(&diffs, Block::Dns);
        assert!(dns.equal, "`dns:` (null) and no `dns:` are both absent");
        assert_eq!(dns.base, None);
        assert_eq!(dns.sub, None);
        assert!(!has_diff(&diffs));
    }

    #[test]
    fn scalar_blocks_compare_exact_values() {
        let base = map_yaml("mode: rule\nlog-level: info\nallow-lan: true\n");
        let sub = map_yaml("mode: global\nlog-level: info\nallow-lan: false\n");

        let diffs = diff_blocks(&base, &sub);

        let mode = find(&diffs, Block::Mode);
        assert!(!mode.equal);
        assert_eq!(mode.base.as_deref(), Some("rule"));
        assert_eq!(mode.sub.as_deref(), Some("global"));

        let allow_lan = find(&diffs, Block::AllowLan);
        assert!(!allow_lan.equal);
        assert_eq!(allow_lan.base.as_deref(), Some("true"));
        assert_eq!(allow_lan.sub.as_deref(), Some("false"));

        assert!(find(&diffs, Block::LogLevel).equal, "same value is equal");
        assert!(has_diff(&diffs));
    }

    #[test]
    fn inbounds_compare_every_port_key() {
        let base = map_yaml("mixed-port: 7890\n");
        let sub = map_yaml("mixed-port: 7890\nsocks-port: 7891\n");

        let diffs = diff_blocks(&base, &sub);
        let inbounds = find(&diffs, Block::Inbounds);
        assert!(!inbounds.equal, "an extra port key is a diff");
        assert_eq!(inbounds.base.as_deref(), Some("1 port"));
        assert_eq!(inbounds.sub.as_deref(), Some("2 ports"));

        // Same keys, different value under one of the five.
        let changed = diff_blocks(
            &map_yaml("mixed-port: 7890\nsocks-port: 7891\n"),
            &map_yaml("mixed-port: 7890\nsocks-port: 9090\n"),
        );
        assert!(
            !find(&changed, Block::Inbounds).equal,
            "a changed port is a diff"
        );
    }

    #[test]
    fn summaries_are_lossy_but_equal_is_exact() {
        // Same node count, different nodes: the summaries agree, the diff
        // does not — this is why BlockDiff carries its own `equal`.
        let base = map_yaml("proxies: [{name: a}, {name: b}]\n");
        let sub = map_yaml("proxies: [{name: x}, {name: y}]\n");

        let diffs = diff_blocks(&base, &sub);
        let proxies = find(&diffs, Block::Proxies);
        assert_eq!(proxies.base, proxies.sub, "the counts agree");
        assert!(!proxies.equal, "different nodes are a diff regardless");
        assert!(has_diff(&diffs));

        // An empty sequence still counts.
        let empty = diff_blocks(&Mapping::new(), &map_yaml("proxies: []\n"));
        let proxies = find(&empty, Block::Proxies);
        assert!(!proxies.equal);
        assert_eq!(proxies.base, None);
        assert_eq!(proxies.sub.as_deref(), Some("0 nodes"));
    }
}
