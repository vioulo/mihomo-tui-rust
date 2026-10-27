//! Persistence for imported subscriptions: a JSON index plus one raw
//! snapshot file per profile.
//!
//! Everything here is deliberately boring file plumbing. Subscriptions can
//! embed credentials, so both artifacts are written 0600, and every write
//! goes through the shared atomic replacement ([`crate::atomic`]) so a crash
//! can never leave a truncated `profiles.json` behind — losing the index
//! would orphan every snapshot.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::atomic::write_atomic;
use crate::paths::Paths;

/// One imported subscription, as listed in `profiles.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileMeta {
    /// Stable handle used by the UI and in the snapshot file name; assigned
    /// here, sequential, never reused while the profile still exists.
    pub id: String,
    pub name: String,
    pub url: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// Node count from the last import.
    pub nodes: u32,
    /// Import summary, e.g. `"vless(20), trojan(3)"`.
    pub protocols: String,
    /// Per-block merge choice: `Block::key()` -> `"base" | "subscription"`.
    pub policy: BTreeMap<String, String>,
}

/// Namespace for the subscription persistence functions; no state.
pub struct Store;

impl Store {
    /// Read the profile index. A missing file is a fresh install (empty
    /// list), but a file that fails to parse is an error: silently resetting
    /// it would throw away the user's subscriptions.
    pub fn load_profiles(p: &Paths) -> Result<Vec<ProfileMeta>> {
        let path = p.profiles_json();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            // First run: nothing imported yet.
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Rewrite the whole profile index as 0600.
    pub fn save_profiles(p: &Paths, v: &[ProfileMeta]) -> Result<()> {
        let text = serde_json::to_string_pretty(v).context("serializing profiles")?;
        write_atomic(&p.profiles_json(), &text)
    }

    /// Append a profile, assign its id, and store the downloaded text as its
    /// snapshot. The caller must have parsed `raw` successfully; this
    /// function deliberately does not look inside it.
    pub fn add_profile(
        p: &Paths,
        name: &str,
        url: &str,
        raw: &str,
        nodes: u32,
        protocols: &str,
        now_ms: u64,
    ) -> Result<ProfileMeta> {
        let mut profiles = Self::load_profiles(p)?;
        let id = next_id(&profiles);
        let profile = ProfileMeta {
            id: id.clone(),
            name: name.to_string(),
            url: url.to_string(),
            created_ms: now_ms,
            updated_ms: now_ms,
            nodes,
            protocols: protocols.to_string(),
            policy: BTreeMap::new(),
        };

        // Snapshot first: an index entry without its snapshot would break
        // the switch flow, while the reverse — an orphaned snapshot after a
        // failed index write — is harmless clutter.
        Self::write_snapshot(p, &id, raw)?;
        profiles.push(profile.clone());
        Self::save_profiles(p, &profiles)?;
        Ok(profile)
    }

    /// Remove a profile and its snapshot.
    pub fn delete_profile(p: &Paths, id: &str) -> Result<()> {
        let mut profiles = Self::load_profiles(p)?;
        let before = profiles.len();
        profiles.retain(|profile| profile.id != id);
        anyhow::ensure!(profiles.len() < before, "profile `{id}` not found");
        Self::save_profiles(p, &profiles)?;

        // Best-effort: the snapshot is a derived cache and the deletion has
        // already succeeded, so a failure here must not lie to the caller.
        let _ = fs::remove_file(Self::snapshot_path(p, id));
        Ok(())
    }

    /// Overwrite one profile's merge policy.
    pub fn update_policy(p: &Paths, id: &str, policy: &BTreeMap<String, String>) -> Result<()> {
        let mut profiles = Self::load_profiles(p)?;
        let profile = profiles
            .iter_mut()
            .find(|profile| profile.id == id)
            .with_context(|| format!("profile `{id}` not found"))?;
        profile.policy = policy.clone();
        Self::save_profiles(p, &profiles)
    }

    /// Record a re-download: refresh the meta and swap in the new snapshot.
    pub fn touch_updated(
        p: &Paths,
        id: &str,
        raw: &str,
        nodes: u32,
        protocols: &str,
        now_ms: u64,
    ) -> Result<()> {
        // Fail before touching anything if the id is unknown.
        let mut profiles = Self::load_profiles(p)?;
        let profile = profiles
            .iter_mut()
            .find(|profile| profile.id == id)
            .with_context(|| format!("profile `{id}` not found"))?;
        profile.nodes = nodes;
        profile.protocols = protocols.to_string();
        profile.updated_ms = now_ms;
        Self::write_snapshot(p, id, raw)?;
        Self::save_profiles(p, &profiles)
    }

    /// Where a profile's snapshot lives. `subscriptions/*.yaml` is also the
    /// glob the external-change watcher scans.
    pub fn snapshot_path(p: &Paths, id: &str) -> PathBuf {
        p.subscriptions_dir().join(format!("{id}.yaml"))
    }

    /// Store the raw downloaded text for a profile (0600, atomic).
    pub fn write_snapshot(p: &Paths, id: &str, raw: &str) -> Result<()> {
        write_atomic(&Self::snapshot_path(p, id), raw)
    }

    /// Read back a profile's raw text. A missing snapshot is an error —
    /// callers need the real content to build a config from.
    pub fn read_snapshot(p: &Paths, id: &str) -> Result<String> {
        let path = Self::snapshot_path(p, id);
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    }

    /// Read custom rules from base.yaml: (prepend-rules, append-rules).
    pub fn load_custom_rules(p: &Paths) -> Result<(Vec<String>, Vec<String>)> {
        let path = p.base_yaml();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        let mapping = match value {
            serde_yaml_ng::Value::Mapping(m) => m,
            serde_yaml_ng::Value::Null => return Ok((Vec::new(), Vec::new())),
            _ => return Ok((Vec::new(), Vec::new())),
        };

        let parse_rules = |key: &str| -> Vec<String> {
            mapping
                .get(key)
                .and_then(serde_yaml_ng::Value::as_sequence)
                .map(|seq| {
                    seq.iter()
                        .filter_map(|item| match item {
                            serde_yaml_ng::Value::String(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default()
        };

        Ok((parse_rules("prepend-rules"), parse_rules("append-rules")))
    }

    /// Write custom rules to base.yaml, preserving any other keys.
    pub fn save_custom_rules(p: &Paths, prepend: &[String], append: &[String]) -> Result<()> {
        let path = p.base_yaml();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };

        let mut mapping = if text.trim().is_empty() {
            serde_yaml_ng::Mapping::new()
        } else {
            let val: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?;
            match val {
                serde_yaml_ng::Value::Mapping(m) => m,
                serde_yaml_ng::Value::Null => serde_yaml_ng::Mapping::new(),
                _ => anyhow::bail!("{} is not a YAML mapping", path.display()),
            }
        };

        let to_seq = |rules: &[String]| -> serde_yaml_ng::Value {
            serde_yaml_ng::Value::Sequence(
                rules
                    .iter()
                    .map(|r| serde_yaml_ng::Value::String(r.clone()))
                    .collect(),
            )
        };

        let prepend_key = serde_yaml_ng::Value::String("prepend-rules".into());
        if prepend.is_empty() {
            mapping.remove(&prepend_key);
        } else {
            mapping.insert(prepend_key, to_seq(prepend));
        }

        let append_key = serde_yaml_ng::Value::String("append-rules".into());
        if append.is_empty() {
            mapping.remove(&append_key);
        } else {
            mapping.insert(append_key, to_seq(append));
        }

        let out = serde_yaml_ng::to_string(&mapping).context("serializing base.yaml")?;
        write_atomic(&path, &out)
    }
}

/// The next free id in `s{n}` form (`s1`, `s2`, …): one past the highest
/// number still listed. A deleted profile may hand its number back —
/// nothing references a deleted id.
fn next_id(profiles: &[ProfileMeta]) -> String {
    let highest = profiles
        .iter()
        .filter_map(|profile| profile.id.strip_prefix('s')?.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    format!("s{}", highest + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// A throwaway config root with the directory layout already created,
    /// mirroring what production does once at startup.
    fn paths() -> (TempDir, Paths) {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::resolve(Some(tmp.path().to_str().unwrap())).unwrap();
        paths.ensure().unwrap();
        (tmp, paths)
    }

    #[test]
    fn add_assigns_sequential_ids_and_persists_0600() {
        let (_tmp, paths) = paths();

        let first = Store::add_profile(
            &paths,
            "first",
            "https://a.example/sub",
            "proxies: a",
            1,
            "ss(1)",
            1_000,
        )
        .unwrap();
        let second = Store::add_profile(
            &paths,
            "second",
            "https://b.example/sub",
            "proxies: b",
            20,
            "vless(20), trojan(3)",
            2_000,
        )
        .unwrap();

        // Ids are assigned in sequence as `s{n}`, regardless of the names.
        assert_eq!(first.id, "s1");
        assert_eq!(second.id, "s2");

        // Both artifacts are private to the user.
        let index_mode = fs::metadata(paths.profiles_json())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(index_mode & 0o777, 0o600, "profiles.json is 0600");
        let snapshot_mode = fs::metadata(Store::snapshot_path(&paths, "s2"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(snapshot_mode & 0o777, 0o600, "snapshots are 0600 too");

        // The index survives a reload with every field intact.
        let reloaded = Store::load_profiles(&paths).unwrap();
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded[0].id, "s1");
        assert_eq!(reloaded[0].name, "first");
        assert_eq!(reloaded[0].url, "https://a.example/sub");
        assert_eq!(reloaded[0].created_ms, 1_000);
        assert_eq!(reloaded[0].updated_ms, 1_000);
        assert_eq!(reloaded[0].nodes, 1);
        assert_eq!(reloaded[0].protocols, "ss(1)");
        assert!(
            reloaded[0].policy.is_empty(),
            "a fresh profile starts with no policy"
        );
        assert_eq!(reloaded[1].id, "s2");

        // And the snapshot holds the exact downloaded text.
        assert_eq!(Store::read_snapshot(&paths, "s2").unwrap(), "proxies: b");
    }

    #[test]
    fn snapshot_roundtrip_and_missing_errors() {
        let (_tmp, paths) = paths();

        Store::write_snapshot(&paths, "9", "first write").unwrap();
        // A rewrite is an overwrite, not an append or a second file.
        Store::write_snapshot(&paths, "9", "raw text").unwrap();
        assert_eq!(Store::read_snapshot(&paths, "9").unwrap(), "raw text");
        assert!(
            Store::read_snapshot(&paths, "absent").is_err(),
            "a missing snapshot is an error, not an empty string"
        );
    }

    #[test]
    fn delete_and_policy_update() {
        let (_tmp, paths) = paths();

        let profile = Store::add_profile(
            &paths,
            "sub",
            "https://s.example/sub",
            "proxies: s",
            3,
            "ss(3)",
            1_000,
        )
        .unwrap();

        let mut policy = BTreeMap::new();
        policy.insert("proxies".to_string(), "subscription".to_string());
        Store::update_policy(&paths, &profile.id, &policy).unwrap();

        let reloaded = Store::load_profiles(&paths).unwrap();
        assert_eq!(reloaded[0].policy, policy);

        // Unknown ids are refused rather than silently ignored.
        assert!(Store::update_policy(&paths, "ghost", &policy).is_err());
        assert!(Store::delete_profile(&paths, "ghost").is_err());

        Store::delete_profile(&paths, &profile.id).unwrap();
        assert!(
            Store::load_profiles(&paths).unwrap().is_empty(),
            "delete empties the list"
        );
        assert!(
            Store::read_snapshot(&paths, &profile.id).is_err(),
            "delete removes the snapshot too"
        );
    }

    #[test]
    fn corrupted_profiles_json_is_an_error_not_a_panic() {
        let (_tmp, paths) = paths();

        fs::write(paths.profiles_json(), "not json").unwrap();
        assert!(
            Store::load_profiles(&paths).is_err(),
            "corruption surfaces as Err, never a panic or a silent reset"
        );
    }

    #[test]
    fn a_missing_index_is_an_empty_list_not_an_error() {
        let (_tmp, paths) = paths();

        assert!(
            Store::load_profiles(&paths).unwrap().is_empty(),
            "a fresh install simply has no profiles"
        );
    }

    #[test]
    fn touch_updated_refreshes_meta_and_snapshot() {
        let (_tmp, paths) = paths();

        let profile = Store::add_profile(
            &paths,
            "sub",
            "https://s.example/sub",
            "old raw",
            1,
            "ss(1)",
            1_000,
        )
        .unwrap();

        Store::touch_updated(&paths, &profile.id, "new raw", 5, "vless(5)", 2_000).unwrap();

        let reloaded = Store::load_profiles(&paths).unwrap();
        assert_eq!(reloaded[0].nodes, 5);
        assert_eq!(reloaded[0].protocols, "vless(5)");
        assert_eq!(reloaded[0].updated_ms, 2_000);
        assert_eq!(reloaded[0].created_ms, 1_000, "creation time is preserved");
        assert_eq!(
            Store::read_snapshot(&paths, &profile.id).unwrap(),
            "new raw",
            "the snapshot is swapped in along with the meta"
        );

        assert!(Store::touch_updated(&paths, "ghost", "x", 1, "y", 3_000).is_err());
    }

    #[test]
    fn custom_rules_load_and_save_roundtrip() {
        let (_tmp, paths) = paths();

        // 1. Missing base.yaml yields empty lists
        let (prepend, append) = Store::load_custom_rules(&paths).unwrap();
        assert!(prepend.is_empty());
        assert!(append.is_empty());

        // 2. Save custom rules
        let prepend_sample = vec![
            "DOMAIN-SUFFIX,google.com,PROXY".to_string(),
            "GEOIP,CN,DIRECT".to_string(),
        ];
        let append_sample = vec!["MATCH,DIRECT".to_string()];
        Store::save_custom_rules(&paths, &prepend_sample, &append_sample).unwrap();

        // 3. Load back and verify
        let (p_loaded, a_loaded) = Store::load_custom_rules(&paths).unwrap();
        assert_eq!(p_loaded, prepend_sample);
        assert_eq!(a_loaded, append_sample);

        // 4. Overwrite preserving file
        Store::save_custom_rules(&paths, &[], &["MATCH,PROXY".to_string()]).unwrap();
        let (p_empty, a_updated) = Store::load_custom_rules(&paths).unwrap();
        assert!(p_empty.is_empty());
        assert_eq!(a_updated, vec!["MATCH,PROXY".to_string()]);
    }
}
