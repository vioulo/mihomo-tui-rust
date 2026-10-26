//! External-change watcher.
//!
//! Subscription files are the user's to edit; the TUI must notice without
//! taking a dependency on a filesystem notification crate. A one-second poll
//! of `(mtime, size)` for `base.yaml` and every `subscriptions/*.yaml` is
//! enough for a human editing a file, and the debounce turns the usual
//! editor write-then-rename burst into a single event.
//!
//! Changes are never applied here: the event only marks the config dirty.
//! The user reviews the diff and confirms, exactly like a normal switch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::app::Event;
use crate::paths::Paths;

/// One second: fast enough to feel live, slow enough to be free.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// `(mtime in nanoseconds, size)` per watched file. Content is deliberately
/// not read: hashing a large snapshot every second would cost more than the
/// false positive of a touch that does not change the text.
type Signature = BTreeMap<PathBuf, (u128, u64)>;

pub fn spawn_config_watcher(paths: Paths, events: mpsc::UnboundedSender<Event>) {
    // Sampling before the task starts removes a startup race: an edit made
    // in the next few milliseconds is a change from this baseline, not part
    // of it.
    let baseline = signature(&paths);
    tokio::spawn(watch(paths, events, POLL_INTERVAL, baseline));
}

async fn watch(
    paths: Paths,
    events: mpsc::UnboundedSender<Event>,
    interval: Duration,
    mut stable: Signature,
) {
    let mut candidate: Option<Signature> = None;

    loop {
        sleep(interval).await;
        let current = signature(&paths);

        if current == stable {
            candidate = None;
            continue;
        }
        if candidate.as_ref() != Some(&current) {
            // First sighting of this state: wait for a second identical
            // sample so an editor's multi-step save is one event.
            candidate = Some(current);
            continue;
        }

        candidate = None;
        for path in changed_paths(&stable, &current) {
            let _ = events.send(Event::ExternalChange {
                path: path.display().to_string(),
            });
        }
        stable = current;
    }
}

fn signature(paths: &Paths) -> Signature {
    let mut files = BTreeMap::new();

    let base = paths.base_yaml();
    if let Some(stamp) = file_signature(&base) {
        files.insert(base, stamp);
    }

    // A missing directory is normal before the first import.
    if let Ok(entries) = std::fs::read_dir(paths.subscriptions_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("yaml")
                && let Some(stamp) = file_signature(&path)
            {
                files.insert(path, stamp);
            }
        }
    }

    files
}

fn file_signature(path: &Path) -> Option<(u128, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    Some((mtime, meta.len()))
}

/// Paths that differ between two samples, including deletions, in a stable
/// order so one save can produce several deterministic events.
fn changed_paths(old: &Signature, new: &Signature) -> Vec<PathBuf> {
    let mut changed: Vec<PathBuf> = new
        .iter()
        .filter(|(path, stamp)| old.get(*path) != Some(*stamp))
        .map(|(path, _)| path.clone())
        .collect();
    changed.extend(old.keys().filter(|path| !new.contains_key(*path)).cloned());
    changed.sort();
    changed.dedup();
    changed
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::sync::mpsc;

    use super::*;
    use crate::api::Client;
    use crate::app::{Command, Event};
    use crate::store::Store;
    use crate::worker::spawn_profile_worker;

    const BASE: &str = r#"
proxies:
  - name: base-node
    type: ss
    server: base.example
    port: 443
proxy-groups:
  - name: PROXY
    type: select
    proxies: [base-node]
rules:
  - MATCH,PROXY
"#;

    const SUB: &str = r#"
proxies:
  - name: sub-node
    type: ss
    server: sub.example
    port: 443
proxy-groups:
  - name: PROXY
    type: select
    proxies: [sub-node]
rules:
  - MATCH,DIRECT
"#;

    struct Fixture {
        _tmp: TempDir,
        paths: Paths,
        commands: mpsc::UnboundedSender<Command>,
        events: mpsc::UnboundedReceiver<Event>,
    }

    /// A full stack without a controller: these tests only read files, so the
    /// client points at a port nothing listens on and is never dialled.
    fn fixture() -> Fixture {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::resolve(Some(tmp.path().to_str().unwrap())).unwrap();
        paths.ensure().unwrap();
        std::fs::write(paths.base_yaml(), BASE).unwrap();
        Store::add_profile(&paths, "one", "https://example/sub", SUB, 1, "ss(1)", 1_000).unwrap();

        let client = Arc::new(Client::new("http://127.0.0.1:1", None).unwrap());
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        spawn_profile_worker(client, paths.clone(), command_rx, event_tx.clone());
        tokio::spawn(watch(
            paths.clone(),
            event_tx,
            Duration::from_millis(20),
            signature(&paths),
        ));

        Fixture {
            _tmp: tmp,
            paths,
            commands,
            events,
        }
    }

    async fn next_event(fixture: &mut Fixture) -> Event {
        tokio::time::timeout(Duration::from_secs(5), fixture.events.recv())
            .await
            .expect("event within the timeout")
            .expect("a sender is still alive")
    }

    async fn next_diff(fixture: &mut Fixture) -> Vec<crate::merge::BlockDiff> {
        loop {
            if let Event::DiffProposal { diffs, .. } = next_event(fixture).await {
                return diffs;
            }
        }
    }

    #[tokio::test]
    async fn external_base_edit_sets_dirty_and_review_applies() {
        let mut fixture = fixture();

        // Same schema, different proxy/group/rules content: what a user
        // editing base.yaml would produce.
        std::fs::write(
            fixture.paths.base_yaml(),
            r#"
proxies:
  - name: edited-node
    type: ss
    server: edited.example
    port: 443
proxy-groups:
  - name: PROXY
    type: select
    proxies: [edited-node]
rules:
  - MATCH,DIRECT
"#,
        )
        .unwrap();

        let event = next_event(&mut fixture).await;
        match event {
            Event::ExternalChange { path } => {
                assert!(path.ends_with("base.yaml"), "{path}");
            }
            other => panic!("expected ExternalChange, got {other:?}"),
        }

        // The review entry point is the ordinary switch flow; the diff must
        // reflect the edited base, not the snapshot taken at startup.
        fixture
            .commands
            .send(Command::SwitchProfile("s1".into()))
            .unwrap();
        let diffs = next_diff(&mut fixture).await;
        let proxies = diffs
            .iter()
            .find(|diff| diff.block == crate::merge::Block::Proxies)
            .expect("a proxies row");
        assert!(!proxies.equal);
        assert_eq!(
            proxies.base.as_deref(),
            Some("1 node"),
            "the diff summary reflects the edited base"
        );
    }

    #[tokio::test]
    async fn invalid_external_yaml_is_reported_not_applied() {
        let mut fixture = fixture();

        // A broken base file is still a change; noticing it is the watcher's
        // job. Refusing to apply it is the worker's.
        std::fs::write(fixture.paths.base_yaml(), ":\n  - [broken\n").unwrap();
        match next_event(&mut fixture).await {
            Event::ExternalChange { path } => assert!(path.ends_with("base.yaml")),
            other => panic!("expected ExternalChange, got {other:?}"),
        }

        // A sentinel runtime proves the worker never rewrites the file when
        // the base cannot be parsed.
        std::fs::write(fixture.paths.runtime_yaml(), "sentinel").unwrap();

        fixture
            .commands
            .send(Command::SwitchProfile("s1".into()))
            .unwrap();

        loop {
            match next_event(&mut fixture).await {
                Event::Toast(kind, text) => {
                    assert_eq!(kind, crate::app::ToastKind::Error);
                    assert!(
                        text.contains("switch failed") && text.contains("parsing"),
                        "parse failure is surfaced: {text}"
                    );
                    break;
                }
                Event::DiffProposal { .. } => panic!("invalid YAML must not produce a diff"),
                _ => {}
            }
        }

        assert_eq!(
            std::fs::read_to_string(fixture.paths.runtime_yaml()).unwrap(),
            "sentinel",
            "runtime.yaml is untouched by a failed parse"
        );
    }
}
