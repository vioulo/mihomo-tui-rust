//! The config we actually hand to the controller: render and write.
//!
//! [`crate::merge`] assembles what should run; this module turns it into the
//! `runtime.yaml` mihomo loads. Rendering copies the merged mapping and
//! injects the two settings the TUI itself owns — the external-controller
//! endpoint and its API secret — so the file always matches what the UI
//! dials, no matter what a subscription shipped; an injected value wins over
//! anything already in the mapping, and no injection never removes one.
//! Writing goes through the shared atomic replacement ([`crate::atomic`]):
//! the file holds credentials, so it is 0600, and a crash mid-write must
//! never leave a truncated `runtime.yaml` or a `*.tmp` sibling behind.

use std::path::Path;

use anyhow::{Context, Result};
use serde_yaml_ng::{Mapping, Value};

use crate::atomic;

/// Render `cfg` as the YAML text mihomo should run: the merged config with
/// `external-controller` set to `endpoint` (when given) and `secret` set to
/// `secret` (when given and not empty). Injections override existing values
/// under the same keys; anything else in `cfg` passes through untouched.
pub fn render_runtime(
    cfg: &Mapping,
    endpoint: Option<&str>,
    secret: Option<&str>,
) -> Result<String> {
    let mut out = cfg.clone();
    if let Some(endpoint) = endpoint {
        out.insert(Value::from("external-controller"), Value::from(endpoint));
    }
    // An empty secret is no secret: mihomo would require the literal empty
    // string on every request, which is never what a blank field means.
    if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
        out.insert(Value::from("secret"), Value::from(secret));
    }
    serde_yaml_ng::to_string(&out).context("rendering runtime config")
}

/// Replace `path` with `content` atomically (0600, same-directory tmp file
/// plus rename — [`crate::atomic`] carries the shared mechanism that
/// `store.rs` writes every artifact through).
pub fn write_atomic(path: &Path, content: &str) -> Result<()> {
    atomic::write_atomic(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml_ng::Value;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// Parse a YAML mapping for the tests; panics on anything else so a
    /// broken fixture fails loudly.
    fn map_yaml(text: &str) -> Mapping {
        match serde_yaml_ng::from_str::<Value>(text).unwrap() {
            Value::Mapping(map) => map,
            other => panic!("fixture must be a mapping, got {other:?}"),
        }
    }

    /// Re-read rendered output as a value so assertions work on the parsed
    /// document, never on byte-exact formatting.
    fn parsed(yaml: &str) -> Value {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[test]
    fn runtime_includes_controller_and_secret() {
        let cfg =
            map_yaml("mixed-port: 7890\nproxies: [{name: n, type: ss, server: s, port: 1}]\n");

        let yaml = render_runtime(&cfg, Some("http://127.0.0.1:9090"), Some("s3cret")).unwrap();
        let out = parsed(&yaml);
        assert_eq!(
            out.get("external-controller").and_then(Value::as_str),
            Some("http://127.0.0.1:9090")
        );
        assert_eq!(out.get("secret").and_then(Value::as_str), Some("s3cret"));
        assert_eq!(out.get("mixed-port").and_then(Value::as_u64), Some(7890));

        // Without a secret the key never appears — no empty placeholder.
        let out = parsed(&render_runtime(&cfg, Some("http://127.0.0.1:9090"), None).unwrap());
        assert_eq!(out.get("secret"), None);
        assert_eq!(
            out.get("external-controller").and_then(Value::as_str),
            Some("http://127.0.0.1:9090")
        );

        // An empty secret counts as "no secret", and no endpoint means no
        // controller key either.
        let out = parsed(&render_runtime(&cfg, None, Some("")).unwrap());
        assert_eq!(out.get("secret"), None);
        assert_eq!(out.get("external-controller"), None);

        // Nothing injected: the merged config comes through unchanged.
        let out = parsed(&render_runtime(&cfg, None, None).unwrap());
        assert_eq!(Value::Mapping(cfg.clone()), out);

        // A value a subscription already shipped never wins over the
        // injection — the TUI must dial the controller it was given.
        let conflicting =
            map_yaml("external-controller: \"http://0.0.0.0:9999\"\nsecret: leaked\n");
        let out = parsed(
            &render_runtime(&conflicting, Some("http://127.0.0.1:9090"), Some("s3cret")).unwrap(),
        );
        assert_eq!(
            out.get("external-controller").and_then(Value::as_str),
            Some("http://127.0.0.1:9090")
        );
        assert_eq!(out.get("secret").and_then(Value::as_str), Some("s3cret"));
    }

    #[test]
    fn write_atomic_sets_0600_and_never_leaves_tmp() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.yaml");

        write_atomic(&path, "first\n").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "runtime.yaml is 0600");
        assert_eq!(fs::read_to_string(&path).unwrap(), "first\n");

        // Overwriting an existing file succeeds and replaces the content.
        write_atomic(&path, "second\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second\n");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the rewritten file is 0600 too");

        // Neither round left a tmp sibling behind — the directory holds the
        // target and nothing else.
        let entries: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["runtime.yaml".to_string()], "{entries:?}");
    }

    #[test]
    fn a_failed_write_leaves_no_tmp_behind() {
        let tmp = TempDir::new().unwrap();
        // A directory where the file should be: the rename can never land,
        // so the failure path has a staged tmp file to clean up.
        let target = tmp.path().join("runtime.yaml");
        fs::create_dir(&target).unwrap();

        assert!(write_atomic(&target, "x").is_err());
        let entries: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["runtime.yaml".to_string()], "{entries:?}");
    }
}
