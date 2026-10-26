//! The configuration block model, the per-block diff behind a switch, and
//! the merge engine that assembles the running config.
//!
//! A mihomo config is compared as ten named [`Block`]s — proxies, proxy
//! groups, rules, DNS, TUN, the five inbound port keys, and the four small
//! scalars. [`diff_blocks`] compares the base config against a subscription
//! block by block and yields one [`BlockDiff`] per block with a short,
//! deterministic summary for each side, so the confirm page can show what a
//! switch would change without dumping YAML into the terminal.
//!
//! [`merge`] then assembles the top-level mapping block by block: every
//! block comes from the side the choice map picks, defaulting to the base
//! side when the map says nothing. A subscription carries only what its
//! provider published, so choosing a block it does not have falls back to a
//! structural stand-in — empty proxies or rules, and synthesized
//! [`default_groups`] for a link-only subscription. [`patch_references`]
//! afterwards drops references to policies that do not exist (unknown group
//! members, rules aimed at unknown policies) and returns one human-readable
//! note per fix; [`validate`] is the last gate and reports the structural
//! problems mihomo would refuse to start on.
//!
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

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
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
    /// Rendered by the confirmation overlay.
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

/// Policies mihomo always provides on top of the config's own proxies and
/// groups; rules and group members aimed at these are never dangling.
const BUILTIN_POLICIES: [&str; 5] = ["DIRECT", "REJECT", "REJECT-DROP", "PASS", "COMPATIBLE"];

/// Assemble the running config from the base config and a subscription,
/// block by block: each [`Block`] is copied from whichever side `choices`
/// names, defaulting to the base side for blocks the map does not mention.
///
/// A choice is absolute — the side that was not picked contributes nothing,
/// and a block absent on both sides stays absent. Choosing a block the
/// subscription does not carry falls back to a structural stand-in, because
/// a link-only subscription publishes proxies and nothing else:
/// - Proxies → an empty list;
/// - Rules → an empty list;
/// - ProxyGroups → [`default_groups`] built from the merged node names.
///
/// Everything else (DNS, TUN, ports, scalars) is simply left out when the
/// chosen side has no value. The result is assembled, not yet trustworthy:
/// run [`patch_references`] and then [`validate`] before rendering it.
///
/// Infallible today — the copy cannot fail and malformed values are passed
/// through for [`validate`] to report — but kept `Result` so the profile
/// worker can propagate future schema failures without a signature change.
pub fn merge(base: &Mapping, sub: &Mapping, choices: &BTreeMap<Block, Source>) -> Result<Mapping> {
    let mut out = Mapping::new();
    for block in Block::ALL {
        let side = choices.get(&block).copied().unwrap_or(Source::Base);
        let from = match side {
            Source::Base => base,
            Source::Subscription => sub,
        };
        let mut present = false;
        for key in block.keys() {
            if let Some(value) = block_value(from, key) {
                out.insert(Value::from(*key), value.clone());
                present = true;
            }
        }
        if present || side != Source::Subscription {
            continue;
        }
        // The subscription was picked but does not carry the block.
        match block {
            Block::Proxies => {
                out.insert(Value::from("proxies"), Value::Sequence(Vec::new()));
            }
            Block::Rules => {
                out.insert(Value::from("rules"), Value::Sequence(Vec::new()));
            }
            Block::ProxyGroups => {
                // Block::ALL order guarantees proxies was already merged, so
                // the synthesized groups reference exactly those nodes.
                let node_names = defined_names(out.get("proxies"));
                out.insert(
                    Value::from("proxy-groups"),
                    Value::Sequence(default_groups(&node_names)),
                );
            }
            _ => {}
        }
    }
    Ok(out)
}

/// The two groups a bare subscription needs: `PROXY`, a select group the UI
/// can drive (AUTO first, then DIRECT, then every node), and `AUTO`, a
/// url-test group over exactly the nodes with mihomo's usual health check
/// (gstatic generate_204 every 300s). Names are copied verbatim — emoji,
/// spaces and slashes included; nothing is renamed or deduplicated.
pub fn default_groups(node_names: &[String]) -> Vec<Value> {
    let mut select = Mapping::new();
    select.insert(Value::from("name"), Value::from("PROXY"));
    select.insert(Value::from("type"), Value::from("select"));
    let mut select_members = vec![Value::from("AUTO"), Value::from("DIRECT")];
    select_members.extend(node_names.iter().map(|name| Value::from(name.as_str())));
    select.insert(Value::from("proxies"), Value::Sequence(select_members));

    let mut auto = Mapping::new();
    auto.insert(Value::from("name"), Value::from("AUTO"));
    auto.insert(Value::from("type"), Value::from("url-test"));
    auto.insert(
        Value::from("proxies"),
        Value::Sequence(
            node_names
                .iter()
                .map(|name| Value::from(name.as_str()))
                .collect(),
        ),
    );
    auto.insert(
        Value::from("url"),
        Value::from("http://www.gstatic.com/generate_204"),
    );
    auto.insert(Value::from("interval"), Value::from(300_u64));

    vec![Value::Mapping(select), Value::Mapping(auto)]
}

/// Drop references to policies that do not exist, returning one
/// human-readable note per fix in deterministic (document) order:
/// - every group member that names no node, group or built-in policy is
///   removed, with a note quoting the group and the member;
/// - every rule aimed at an unknown policy is dropped, with a note quoting
///   the rule and the policy.
///
/// Definitions are never touched: duplicated node entries stay duplicated
/// and names stay verbatim (emoji, spaces, slashes) — resolution works on a
/// set of exact names, so nothing is silently renamed. Rules that cannot be
/// parsed (logic rules like `AND`/`SUB-RULE`, non-string entries) are left
/// alone rather than guessed at; [`validate`] reports what remains broken.
pub fn patch_references(cfg: &mut Mapping) -> Vec<String> {
    let mut notes = Vec::new();
    // Both sets are read up front: groups may reference groups, and rules
    // may target bare nodes as well as groups.
    let node_names: BTreeSet<String> = defined_names(cfg.get("proxies")).into_iter().collect();
    let group_names: BTreeSet<String> =
        defined_names(cfg.get("proxy-groups")).into_iter().collect();

    if let Some(groups) = cfg.get_mut("proxy-groups").and_then(Value::as_sequence_mut) {
        for group in groups.iter_mut() {
            let Some(group) = group.as_mapping_mut() else {
                continue;
            };
            let group_name = group
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            let Some(members) = group.get_mut("proxies").and_then(Value::as_sequence_mut) else {
                continue;
            };
            let mut kept = Vec::with_capacity(members.len());
            for member in members.drain(..) {
                match member.as_str() {
                    Some(name) if is_known_policy(name, &node_names, &group_names) => {
                        kept.push(member);
                    }
                    Some(name) => notes.push(format!(
                        "group \"{group_name}\": removed unknown member \"{name}\""
                    )),
                    None => notes.push(format!(
                        "group \"{group_name}\": removed a member that is not a name"
                    )),
                }
            }
            *members = kept;
        }
    }

    if let Some(rules) = cfg.get_mut("rules").and_then(Value::as_sequence_mut) {
        let mut kept = Vec::with_capacity(rules.len());
        for rule in rules.drain(..) {
            let policy = rule.as_str().and_then(rule_policy);
            let known = policy.is_none_or(|name| is_known_policy(name, &node_names, &group_names));
            if known {
                kept.push(rule);
            } else {
                notes.push(format!(
                    "dropped rule \"{}\": unknown policy \"{}\"",
                    rule.as_str().unwrap_or_default(),
                    policy.unwrap_or_default(),
                ));
            }
        }
        *rules = kept;
    }

    notes
}

/// Check the structural invariants mihomo refuses to start without:
/// - `proxies` is a non-empty list and every node has a `name`, a `type`
///   and a `server`;
/// - every group (when `proxy-groups` exists at all) has a `name`, a `type`
///   and a non-empty members list.
///
/// All problems are collected, and each message names the offending object
/// (its `name` where one exists, its position otherwise). `Ok(())` means the
/// shape is sound — not that every reference resolves; that is
/// [`patch_references`]'s job and it runs first.
pub fn validate(cfg: &Mapping) -> std::result::Result<(), Vec<String>> {
    let mut errors = Vec::new();

    match cfg.get("proxies") {
        Some(Value::Sequence(items)) if !items.is_empty() => {
            for (index, node) in items.iter().enumerate() {
                validate_named(node, index, "proxies", "node", &mut errors);
                validate_node_fields(node, index, &mut errors);
            }
        }
        Some(Value::Sequence(_)) => {
            errors.push("proxies is empty — at least one node is required".to_string());
        }
        Some(_) => errors.push("proxies is not a list".to_string()),
        None => errors.push("proxies is missing — at least one node is required".to_string()),
    }

    match cfg.get("proxy-groups") {
        Some(Value::Sequence(groups)) => {
            for (index, group) in groups.iter().enumerate() {
                validate_named(group, index, "proxy-groups", "group", &mut errors);
                validate_group_fields(group, index, &mut errors);
            }
        }
        // Groups are optional: rules may target bare proxies directly.
        Some(_) => errors.push("proxy-groups is not a list".to_string()),
        None => {}
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// The `name` of every mapping in a top-level list, in document order;
/// non-mapping items and nameless ones contribute nothing.
fn defined_names(value: Option<&Value>) -> Vec<String> {
    let Some(items) = value.and_then(Value::as_sequence) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| item.get("name").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// Whether `name` resolves to something mihomo can route to: a defined node,
/// a defined group, or a built-in policy.
fn is_known_policy(name: &str, nodes: &BTreeSet<String>, groups: &BTreeSet<String>) -> bool {
    BUILTIN_POLICIES.contains(&name) || nodes.contains(name) || groups.contains(name)
}

/// The target policy of a rule line: the third comma-separated field for
/// every rule type except `MATCH` (where it is the second). `None` for
/// logic rules (`AND`/`OR`/`NOT`/`SUB-RULE`, whose policies sit inside
/// sub-expressions a comma split cannot see) and anything else unparseable —
/// the caller leaves those untouched instead of guessing.
fn rule_policy(rule: &str) -> Option<&str> {
    let fields: Vec<&str> = rule.split(',').collect();
    let kind = fields.first()?.trim();
    if kind.contains('(') {
        return None;
    }
    let index = match kind.to_ascii_uppercase().as_str() {
        "MATCH" => 1,
        "AND" | "OR" | "NOT" | "SUB-RULE" => return None,
        _ => 2,
    };
    fields.get(index).map(|policy| policy.trim())
}

/// "`proxies[0] (name)`" — position always, name when there is one; the
/// prefix every validation error starts with so messages can be traced back
/// to the offending object.
fn object_label(object: &Mapping, index: usize, list_key: &str) -> String {
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if name.is_empty() {
        format!("{list_key}[{index}]")
    } else {
        format!("{list_key}[{index}] ({name})")
    }
}

/// The `name` check shared by nodes and groups.
fn validate_named(
    object: &Value,
    index: usize,
    list_key: &str,
    kind: &str,
    errors: &mut Vec<String>,
) {
    let Some(object) = object.as_mapping() else {
        errors.push(format!("{list_key}[{index}]: {kind} is not a mapping"));
        return;
    };
    let label = object_label(object, index, list_key);
    let named = object.get("name").and_then(Value::as_str);
    if named.is_none_or(|name| name.trim().is_empty()) {
        errors.push(format!("{label}: {kind} has no name"));
    }
}

/// The node-only field checks: `type` and `server` must exist, or mihomo
/// cannot tell what kind of proxy this is or where to reach it.
fn validate_node_fields(node: &Value, index: usize, errors: &mut Vec<String>) {
    let Some(node) = node.as_mapping() else {
        return; // already reported by validate_named
    };
    let label = object_label(node, index, "proxies");
    if node
        .get("type")
        .and_then(Value::as_str)
        .is_none_or(|kind| kind.trim().is_empty())
    {
        errors.push(format!("{label}: node has no type"));
    }
    if node
        .get("server")
        .and_then(Value::as_str)
        .is_none_or(|server| server.trim().is_empty())
    {
        errors.push(format!("{label}: node has no server"));
    }
}

/// The group-only field checks: `type` must exist, or mihomo cannot tell
/// what kind of group this is and refuses to start; the members list must
/// be non-empty, or the group routes nowhere.
fn validate_group_fields(group: &Value, index: usize, errors: &mut Vec<String>) {
    let Some(group) = group.as_mapping() else {
        return; // already reported by validate_named
    };
    let label = object_label(group, index, "proxy-groups");
    if group
        .get("type")
        .and_then(Value::as_str)
        .is_none_or(|kind| kind.trim().is_empty())
    {
        errors.push(format!("{label}: group has no type"));
    }
    let members = group.get("proxies").and_then(Value::as_sequence);
    if members.is_none_or(|members| members.is_empty()) {
        errors.push(format!("{label}: group has no members"));
    }
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

    // ---- task 7: merge, default_groups, patch_references, validate ----

    use std::collections::BTreeMap;

    /// A choice map built from (block, side) pairs.
    fn choices(pairs: &[(Block, Source)]) -> BTreeMap<Block, Source> {
        pairs.iter().copied().collect()
    }

    /// The mapping at `proxy-groups[index]`; panics so a broken fixture fails
    /// loudly instead of asserting on the wrong thing.
    fn group(cfg: &Mapping, index: usize) -> &Mapping {
        cfg.get("proxy-groups")
            .and_then(Value::as_sequence)
            .and_then(|groups| groups.get(index))
            .and_then(Value::as_mapping)
            .unwrap_or_else(|| panic!("no mapping group at proxy-groups[{index}]"))
    }

    /// The string members of one group's `proxies` list.
    fn members(group: &Mapping) -> Vec<&str> {
        group
            .get("proxies")
            .and_then(Value::as_sequence)
            .unwrap_or_else(|| panic!("group has no members list"))
            .iter()
            .map(|member| {
                member
                    .as_str()
                    .unwrap_or_else(|| panic!("member {member:?} is not a name"))
            })
            .collect()
    }

    /// The string rules of a config.
    fn rule_lines(cfg: &Mapping) -> Vec<&str> {
        cfg.get("rules")
            .and_then(Value::as_sequence)
            .unwrap_or_else(|| panic!("config has no rules list"))
            .iter()
            .map(|rule| {
                rule.as_str()
                    .unwrap_or_else(|| panic!("rule {rule:?} is not a string"))
            })
            .collect()
    }

    #[test]
    fn merge_follows_per_block_choices() {
        let base = map_yaml(
            "mixed-port: 7890\n\
             log-level: info\n\
             mode: rule\n\
             dns: {enable: true, nameserver: [1.1.1.1]}\n\
             rules: [\"MATCH,DIRECT\"]\n",
        );
        let sub = map_yaml(
            "socks-port: 7891\n\
             dns: {enable: false}\n\
             rules: [\"MATCH,PROXY\", \"GEOIP,CN,DIRECT\"]\n",
        );

        // Rules and DNS follow their explicit choices; every block missing
        // from the map defaults to the Base side.
        let merged = merge(
            &base,
            &sub,
            &choices(&[
                (Block::Rules, Source::Subscription),
                (Block::Dns, Source::Base),
            ]),
        )
        .unwrap();

        assert_eq!(
            merged.get("rules"),
            sub.get("rules"),
            "Rules follows Subscription"
        );
        assert_eq!(merged.get("dns"), base.get("dns"), "Dns follows Base");
        assert_eq!(
            merged.get("mixed-port").and_then(Value::as_u64),
            Some(7890),
            "blocks missing from choices default to Base"
        );
        assert_eq!(
            merged.get("log-level").and_then(Value::as_str),
            Some("info")
        );
        assert_eq!(merged.get("mode").and_then(Value::as_str), Some("rule"));
        assert_eq!(
            merged.get("socks-port"),
            None,
            "the side that was not chosen contributes nothing"
        );
        assert_eq!(
            merged.get("proxies"),
            None,
            "absent on both sides stays absent"
        );

        // An explicit choice flips just that block — Inbounds spans all five
        // port keys, so Subscription brings socks-port and drops mixed-port.
        let merged = merge(
            &base,
            &sub,
            &choices(&[
                (Block::Inbounds, Source::Subscription),
                (Block::Dns, Source::Subscription),
            ]),
        )
        .unwrap();
        assert_eq!(merged.get("socks-port").and_then(Value::as_u64), Some(7891));
        assert_eq!(merged.get("mixed-port"), None);
        assert_eq!(merged.get("dns"), sub.get("dns"));
        assert_eq!(
            merged.get("rules"),
            base.get("rules"),
            "blocks absent from this map default to Base again"
        );
    }

    #[test]
    fn link_only_subscription_gets_default_groups() {
        let base = map_yaml("mixed-port: 7890\nlog-level: info\n");
        let sub = map_yaml(
            "proxies:\n\
             - {name: \"🇭🇰 HK 01\", type: ss, server: h, port: 1}\n\
             - {name: us, type: ss, server: u, port: 2}\n",
        );
        let merged = merge(
            &base,
            &sub,
            &choices(&[
                (Block::Proxies, Source::Subscription),
                (Block::ProxyGroups, Source::Subscription),
                (Block::Rules, Source::Subscription),
            ]),
        )
        .unwrap();

        // The proxies list is the subscription's verbatim, and blocks the
        // choice map never mentions still default to Base.
        assert_eq!(merged.get("proxies"), sub.get("proxies"));
        assert_eq!(merged.get("mixed-port").and_then(Value::as_u64), Some(7890));
        assert_eq!(
            merged.get("log-level").and_then(Value::as_str),
            Some("info")
        );

        let groups = merged
            .get("proxy-groups")
            .and_then(Value::as_sequence)
            .expect("default groups are a sequence");
        assert_eq!(groups.len(), 2, "PROXY and AUTO, nothing else");

        let select = groups[0].as_mapping().unwrap();
        assert_eq!(select.get("name").and_then(Value::as_str), Some("PROXY"));
        assert_eq!(select.get("type").and_then(Value::as_str), Some("select"));
        assert_eq!(
            select
                .get("proxies")
                .and_then(Value::as_sequence)
                .map(|members| members.iter().map(Value::as_str).collect::<Vec<_>>()),
            Some(vec![
                Some("AUTO"),
                Some("DIRECT"),
                Some("🇭🇰 HK 01"),
                Some("us")
            ]),
        );

        let auto = groups[1].as_mapping().unwrap();
        assert_eq!(auto.get("name").and_then(Value::as_str), Some("AUTO"));
        assert_eq!(auto.get("type").and_then(Value::as_str), Some("url-test"));
        assert_eq!(
            auto.get("proxies")
                .and_then(Value::as_sequence)
                .map(|members| members.iter().map(Value::as_str).collect::<Vec<_>>()),
            Some(vec![Some("🇭🇰 HK 01"), Some("us")]),
            "the url-test group tests exactly the nodes"
        );
        assert_eq!(
            auto.get("url").and_then(Value::as_str),
            Some("http://www.gstatic.com/generate_204")
        );
        assert_eq!(auto.get("interval").and_then(Value::as_u64), Some(300));

        // The subscription carries no rules, so Rules/Subscription yields an
        // empty list — a real key, never a missing one.
        assert_eq!(merged.get("rules"), Some(&Value::Sequence(Vec::new())));
    }

    #[test]
    fn default_groups_have_the_pinned_shape() {
        let groups = default_groups(&["🇭🇰 HK 01".to_string(), "a/b".to_string()]);
        assert_eq!(groups.len(), 2);

        let select = groups[0].as_mapping().unwrap();
        assert_eq!(select.get("name").and_then(Value::as_str), Some("PROXY"));
        assert_eq!(select.get("type").and_then(Value::as_str), Some("select"));
        let select_members: Vec<&str> = select
            .get("proxies")
            .and_then(Value::as_sequence)
            .unwrap()
            .iter()
            .map(Value::as_str)
            .collect::<Vec<_>>()
            .into_iter()
            .map(Option::unwrap)
            .collect();
        assert_eq!(select_members, ["AUTO", "DIRECT", "🇭🇰 HK 01", "a/b"]);

        let auto = groups[1].as_mapping().unwrap();
        assert_eq!(auto.get("name").and_then(Value::as_str), Some("AUTO"));
        assert_eq!(auto.get("type").and_then(Value::as_str), Some("url-test"));
        let auto_members: Vec<&str> = auto
            .get("proxies")
            .and_then(Value::as_sequence)
            .unwrap()
            .iter()
            .map(Value::as_str)
            .collect::<Vec<_>>()
            .into_iter()
            .map(Option::unwrap)
            .collect();
        assert_eq!(
            auto_members,
            ["🇭🇰 HK 01", "a/b"],
            "no DIRECT in the url-test group"
        );
        assert_eq!(
            auto.get("url").and_then(Value::as_str),
            Some("http://www.gstatic.com/generate_204")
        );
        assert_eq!(auto.get("interval").and_then(Value::as_u64), Some(300));

        // An empty node list still yields both groups with sane members.
        let groups = default_groups(&[]);
        let select = groups[0].as_mapping().unwrap();
        assert_eq!(members(select), ["AUTO", "DIRECT"]);
        assert_eq!(members(groups[1].as_mapping().unwrap()), Vec::<&str>::new());
    }

    #[test]
    fn dangling_group_members_are_patched_with_notes() {
        let mut cfg = map_yaml(
            "proxies:\n\
             - {name: \"🇭🇰 HK 01\", type: ss, server: h, port: 1}\n\
             - {name: a/b, type: ss, server: s, port: 2}\n\
             - {name: a/b, type: ss, server: d, port: 3}\n\
             - {name: jp, type: ss, server: j, port: 4}\n\
             proxy-groups:\n\
             - {name: PROXY, type: select, proxies: [AUTO, DIRECT, \"🇭🇰 HK 01\", a/b, ghost-node]}\n\
             - {name: AUTO, type: url-test, proxies: [\"🇭🇰 HK 01\", ghost-node, a/b]}\n\
             rules: [\"MATCH,PROXY\"]\n",
        );

        let notes = patch_references(&mut cfg);

        assert_eq!(notes.len(), 2, "one note per removed member: {notes:?}");
        assert!(
            notes[0].contains("PROXY") && notes[0].contains("ghost-node"),
            "the note names the group and the member: {:?}",
            notes[0]
        );
        assert!(
            notes[1].contains("AUTO") && notes[1].contains("ghost-node"),
            "notes follow document order: {:?}",
            notes[1]
        );

        assert_eq!(
            members(group(&cfg, 0)),
            ["AUTO", "DIRECT", "🇭🇰 HK 01", "a/b"],
            "only the unknown member is removed"
        );
        assert_eq!(members(group(&cfg, 1)), ["🇭🇰 HK 01", "a/b"]);

        // Review focus 5: emoji/slash names survive verbatim and duplicate
        // node entries stay duplicated — patching works on a name set and
        // never renames or deduplicates definitions.
        let proxies = cfg.get("proxies").and_then(Value::as_sequence).unwrap();
        assert_eq!(proxies.len(), 4);
        assert_eq!(proxies[1].get("name").and_then(Value::as_str), Some("a/b"));
        assert_eq!(proxies[2].get("name").and_then(Value::as_str), Some("a/b"));
        assert_eq!(
            proxies[0].get("name").and_then(Value::as_str),
            Some("🇭🇰 HK 01")
        );
        assert_eq!(
            rule_lines(&cfg),
            ["MATCH,PROXY"],
            "rules are untouched here"
        );
    }

    #[test]
    fn rules_referencing_missing_groups_are_dropped() {
        let mut cfg = map_yaml(
            "proxies:\n\
             - {name: \"🇭🇰 HK 01\", type: ss, server: h, port: 1}\n\
             proxy-groups:\n\
             - {name: PROXY, type: select, proxies: [DIRECT, \"🇭🇰 HK 01\"]}\n\
             rules:\n\
             - \"DOMAIN,example.com,PROXY\"\n\
             - \"DOMAIN,bad.com,ghost\"\n\
             - \"MATCH,DIRECT\"\n\
             - \"IP-CIDR,10.0.0.0/8,PROXY,no-resolve\"\n\
             - \"DOMAIN,hk.example,🇭🇰 HK 01\"\n\
             - \"AND,((DOMAIN,a.com),(DOMAIN,b.com)),PROXY\"\n",
        );

        let notes = patch_references(&mut cfg);

        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("ghost") && notes[0].contains("DOMAIN,bad.com,ghost"),
            "the note quotes the rule and the missing policy: {:?}",
            notes[0]
        );
        assert_eq!(
            rule_lines(&cfg),
            [
                "DOMAIN,example.com,PROXY",
                "MATCH,DIRECT",
                "IP-CIDR,10.0.0.0/8,PROXY,no-resolve",
                "DOMAIN,hk.example,🇭🇰 HK 01",
                "AND,((DOMAIN,a.com),(DOMAIN,b.com)),PROXY",
            ],
            "built-ins, node targets and logic rules all survive"
        );
    }

    #[test]
    fn patch_tolerates_malformed_shapes() {
        let mut cfg = map_yaml(
            "proxies: oops\n\
             proxy-groups:\n\
             - just-a-string\n\
             - {name: G, type: select, proxies: [DIRECT, ghost, 7]}\n\
             - {name: H, type: select}\n\
             rules:\n\
             - 42\n\
             - \"MATCH,ghost\"\n",
        );

        let notes = patch_references(&mut cfg);

        // No panic; the parseable parts are still patched.
        assert_eq!(notes.len(), 3, "{notes:?}");
        let groups = cfg
            .get("proxy-groups")
            .and_then(Value::as_sequence)
            .unwrap();
        assert_eq!(groups.len(), 3, "malformed entries are left in place");
        assert_eq!(members(groups[1].as_mapping().unwrap()), ["DIRECT"]);
        assert_eq!(
            cfg.get("rules").and_then(Value::as_sequence).map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn validate_rejects_empty_proxies_and_missing_fields() {
        let errors = validate(&map_yaml("rules: [\"MATCH,DIRECT\"]\n")).unwrap_err();
        assert!(
            errors.iter().any(|error| error.contains("proxies")),
            "missing proxies is an error: {errors:?}"
        );

        let errors = validate(&map_yaml("proxies: []\n")).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error.contains("proxies") && error.contains("empty")),
            "empty proxies is an error: {errors:?}"
        );

        // A node without server is rejected and the message carries its name.
        let errors = validate(&map_yaml("proxies: [{name: broken, type: ss}]\n")).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error.contains("broken") && error.contains("server")),
            "{errors:?}"
        );

        // So is a node without a type.
        let errors = validate(&map_yaml(
            "proxies: [{name: notyped, server: s, port: 1}]\n",
        ))
        .unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error.contains("notyped") && error.contains("type")),
            "{errors:?}"
        );

        // A node without a name is reported by position, never by guessing.
        let errors =
            validate(&map_yaml("proxies: [{type: ss, server: s, port: 1}]\n")).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error.contains("proxies[0]") && error.contains("name")),
            "{errors:?}"
        );

        // A group without members is rejected and the message carries its name.
        let cfg = map_yaml(
            "proxies: [{name: ok, type: ss, server: s, port: 1}]\n\
             proxy-groups: [{name: LONELY, type: select}]\n",
        );
        let errors = validate(&cfg).unwrap_err();
        assert!(
            errors.iter().any(|error| error.contains("LONELY")),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_rejects_a_group_without_type() {
        // mihomo cannot start a group it cannot classify: name and members
        // alone are not enough, and the error must name the group.
        let cfg = map_yaml(
            "proxies: [{name: ok, type: ss, server: s, port: 1}]\n\
             proxy-groups: [{name: NOTYPED, proxies: [ok]}]\n",
        );
        let errors = validate(&cfg).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error.contains("NOTYPED") && error.contains("type")),
            "group without type is an error naming the group: {errors:?}"
        );
    }

    #[test]
    fn validate_accepts_a_well_formed_config() {
        let cfg = map_yaml(
            "mixed-port: 7890\n\
             proxies: [{name: ok, type: ss, server: s, port: 1}]\n\
             proxy-groups:\n\
             - {name: PROXY, type: select, proxies: [AUTO, DIRECT, ok]}\n\
             - {name: AUTO, type: url-test, proxies: [ok]}\n\
             rules: [\"MATCH,PROXY\"]\n",
        );
        assert_eq!(validate(&cfg), Ok(()));

        // Groups are optional when rules target bare proxies.
        let minimal = map_yaml(
            "proxies: [{name: ok, type: ss, server: s, port: 1}]\nrules: [\"MATCH,ok\"]\n",
        );
        assert_eq!(validate(&minimal), Ok(()));
    }
}
