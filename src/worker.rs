//! The profile worker: the only task that mutates config files or asks the
//! controller to reload.
//!
//! Commands arrive one at a time over a dedicated channel, so two switches
//! can never interleave their writes. Applying a profile is a small
//! pipeline: merge the base config with a subscription snapshot, repair
//! dangling references, validate, write `runtime.yaml`, reload through the
//! controller, then check `/version`. Anything that goes wrong after the
//! controller was touched rolls back to the last config that passed that
//! whole sequence.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde_yaml_ng::{Mapping, Value};
use tokio::sync::mpsc;

use crate::api::Client;
use crate::app::{Command, Event, ToastKind};
use crate::merge::{self, Block, Source};
use crate::paths::Paths;
use crate::runtime;
use crate::store::{ProfileMeta, Store};
use crate::subscription;

/// Start the worker on the current runtime. The receiver is owned by the
/// worker for its whole life: dropping every sender is how the task ends.
pub fn spawn_profile_worker(
    client: Arc<Client>,
    paths: Paths,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<Event>,
) {
    tokio::spawn(async move {
        let mut worker = ProfileWorker {
            client,
            paths,
            events,
            last_good: None,
            current: None,
        };
        while let Some(command) = commands.recv().await {
            worker.handle(command).await;
        }
    });
}

struct ProfileWorker {
    client: Arc<Client>,
    paths: Paths,
    events: mpsc::UnboundedSender<Event>,
    /// Last runtime YAML that both reloaded and passed the health check.
    /// Rollback is only ever attempted against this, never against a guess.
    last_good: Option<String>,
    /// Profile id that produced `last_good`, so deleting the active profile
    /// can warn that the running config is now stale.
    current: Option<String>,
}

/// Why an apply did not happen. The split matters to the UI: a local
/// failure never touched the controller, while a remote failure may have.
enum ApplyFailure {
    /// Nothing reached the controller; the running config is unchanged.
    Local(anyhow::Error),
    /// The controller rejected the new config or failed the health check.
    Remote { error: String, rolled_back: bool },
}

impl From<anyhow::Error> for ApplyFailure {
    fn from(err: anyhow::Error) -> Self {
        Self::Local(err)
    }
}

impl ProfileWorker {
    async fn handle(&mut self, command: Command) {
        match command {
            Command::SwitchProfile(id) => self.switch(&id).await,
            Command::ConfirmSwitch {
                profile_id,
                choices,
                remember,
            } => self.confirm(&profile_id, &choices, remember).await,
            Command::AddProfile { name, url } => self.add(&name, &url).await,
            Command::UpdateProfile(id) => self.update(&id).await,
            Command::DeleteProfile(id) => self.delete(&id).await,
            Command::SaveRules { prepend, append } => self.save_rules(prepend, append).await,
            // Everything else belongs to `execute` in main; receiving one
            // here would be a routing bug, not a reason to crash.
            _ => {}
        }
    }

    async fn save_rules(&mut self, prepend: Vec<String>, append: Vec<String>) {
        if let Err(err) = Store::save_custom_rules(&self.paths, &prepend, &append) {
            self.error_toast("failed to save custom rules", &err);
            return;
        }
        self.send(Event::RulesUpdated { prepend, append });
        self.toast(ToastKind::Success, "Custom rules saved to base.yaml");
        if let Some(id) = self.current.clone() {
            self.switch(&id).await;
        }
    }

    fn send(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn toast(&self, kind: ToastKind, text: impl Into<String>) {
        self.send(Event::Toast(kind, text.into()));
    }

    fn error_toast(&self, context: &str, err: &anyhow::Error) {
        self.toast(
            ToastKind::Error,
            format!("{context}: {}", crate::short_reason(err)),
        );
    }

    /// Re-read the index after every mutation so the UI never shows a list
    /// the worker has already moved past.
    fn emit_profiles(&self) {
        match Store::load_profiles(&self.paths) {
            Ok(profiles) => self.send(Event::ProfilesUpdated(profiles)),
            Err(err) => self.error_toast("reading subscriptions", &err),
        }
    }

    fn profile(&self, id: &str) -> Result<ProfileMeta> {
        Store::load_profiles(&self.paths)?
            .into_iter()
            .find(|profile| profile.id == id)
            .with_context(|| format!("subscription `{id}` not found"))
    }

    /// The user-owned config, or an empty mapping on first run. A missing
    /// file is normal; anything else that is not a mapping is an error the
    /// user has to fix.
    fn base_mapping(&self) -> Result<Mapping> {
        let path = self.paths.base_yaml();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Mapping::new()),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let value: Value = serde_yaml_ng::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        match value {
            Value::Mapping(map) => Ok(map),
            Value::Null => Ok(Mapping::new()),
            _ => bail!("{} must contain a YAML mapping", path.display()),
        }
    }

    fn subscription_mapping(&self, profile: &ProfileMeta) -> Result<Mapping> {
        let raw = Store::read_snapshot(&self.paths, &profile.id)?;
        Ok(subscription::parse_subscription(&raw)?.mapping)
    }

    /// `enter`, `[` and `]`: show the diff when there is one, otherwise
    /// apply straight away and say so.
    async fn switch(&mut self, id: &str) {
        let proposal = self.profile(id).and_then(|profile| {
            let base = self.base_mapping()?;
            let sub = self.subscription_mapping(&profile)?;
            Ok((profile, merge::diff_blocks(&base, &sub)))
        });

        let (profile, diffs) = match proposal {
            Ok(pair) => pair,
            Err(err) => {
                self.error_toast("switch failed", &err);
                return;
            }
        };

        if merge::has_diff(&diffs) {
            self.send(Event::DiffProposal {
                profile_id: profile.id,
                diffs,
                policy: profile.policy,
            });
            return;
        }

        match self.apply(id, &BTreeMap::new(), false).await {
            Ok(notes) => {
                let mut text = "switched (no diff)".to_string();
                if !notes.is_empty() {
                    text.push_str(&format!("; patched: {}", notes.join("; ")));
                }
                self.toast(ToastKind::Success, text);
            }
            Err(failure) => self.report_failure(failure),
        }
    }

    async fn confirm(&mut self, id: &str, choices: &BTreeMap<Block, Source>, remember: bool) {
        match self.apply(id, choices, remember).await {
            Ok(notes) => {
                self.send(Event::ReloadResult {
                    ok: true,
                    rolled_back: false,
                    error: None,
                });
                // Later than the result on purpose: a repair notice is the
                // more actionable message when both arrive together.
                if !notes.is_empty() {
                    self.toast(ToastKind::Info, format!("patched: {}", notes.join("; ")));
                }
            }
            Err(failure) => self.report_failure(failure),
        }
    }

    /// The whole generate → reload → verify sequence. On success the worker
    /// records the config as the next rollback target; on failure the caller
    /// gets a classified error and the worker stays on the old target.
    async fn apply(
        &mut self,
        id: &str,
        choices: &BTreeMap<Block, Source>,
        remember: bool,
    ) -> std::result::Result<Vec<String>, ApplyFailure> {
        let profile = self.profile(id)?;
        let base = self.base_mapping()?;
        let sub = self.subscription_mapping(&profile)?;
        let mut cfg = merge::merge(&base, &sub, choices)?;
        let notes = merge::patch_references(&mut cfg);
        merge::validate(&cfg).map_err(|errors| anyhow!(errors.join("; ")))?;

        let address = controller_address(self.client.endpoint());
        let yaml = runtime::render_runtime(&cfg, Some(address.as_str()), self.client.secret())?;
        runtime::write_atomic(&self.paths.runtime_yaml(), &yaml)?;

        if let Err(err) = self.client.reload_config(&yaml).await {
            let error = format!("reload failed: {}", crate::short_reason(&err));
            return Err(self.rollback(error).await);
        }
        if let Err(err) = self.client.version().await {
            let error = format!("health check failed: {}", crate::short_reason(&err));
            return Err(self.rollback(error).await);
        }

        self.last_good = Some(yaml);
        self.current = Some(id.to_string());

        if remember {
            let policy = choices
                .iter()
                .map(|(block, source)| (block.key().to_string(), source.label().to_string()))
                .collect();
            if let Err(err) = Store::update_policy(&self.paths, id, &policy) {
                self.error_toast("switch applied, but saving its policy failed", &err);
            }
        }
        self.emit_profiles();
        Ok(notes)
    }

    /// Restore the last config that passed reload plus health check. Both
    /// the file and the controller are restored; a failure in either leaves
    /// `rolled_back: false` so the UI can surface the full story.
    async fn rollback(&self, error: String) -> ApplyFailure {
        let Some(good) = self.last_good.clone() else {
            return ApplyFailure::Remote {
                error,
                rolled_back: false,
            };
        };

        if let Err(err) = runtime::write_atomic(&self.paths.runtime_yaml(), &good) {
            return ApplyFailure::Remote {
                error: format!(
                    "{error}; rollback could not restore runtime.yaml: {}",
                    crate::short_reason(&err)
                ),
                rolled_back: false,
            };
        }
        if let Err(err) = self.client.reload_config(&good).await {
            return ApplyFailure::Remote {
                error: format!(
                    "{error}; rollback reload failed: {}",
                    crate::short_reason(&err)
                ),
                rolled_back: false,
            };
        }
        if let Err(err) = self.client.version().await {
            return ApplyFailure::Remote {
                error: format!(
                    "{error}; rollback health check failed: {}",
                    crate::short_reason(&err)
                ),
                rolled_back: false,
            };
        }

        ApplyFailure::Remote {
            error: format!("{error}; rolled back to the last good config"),
            rolled_back: true,
        }
    }

    fn report_failure(&self, failure: ApplyFailure) {
        match failure {
            ApplyFailure::Local(err) => self.error_toast("switch failed", &err),
            ApplyFailure::Remote { error, rolled_back } => {
                self.send(Event::ReloadResult {
                    ok: false,
                    rolled_back,
                    error: Some(error.clone()),
                });
                // A rollback was possible but could not restore service:
                // the controller is no longer trustworthy, so say so.
                if !rolled_back && self.last_good.is_some() {
                    self.send(Event::Fatal(error));
                }
            }
        }
    }

    async fn add(&mut self, name: &str, url: &str) {
        let name = display_name(name, url);
        let imported = {
            let paths = &self.paths;
            async {
                let raw = subscription::download(url).await?;
                subscription::import_from_raw(paths, &name, url, &raw)
            }
            .await
        };

        match imported {
            Ok(profile) => {
                self.emit_profiles();
                self.toast(
                    ToastKind::Success,
                    format!(
                        "imported {} ({} nodes, {})",
                        profile.name, profile.nodes, profile.protocols
                    ),
                );
            }
            Err(err) => self.error_toast("import failed", &err),
        }
    }

    /// Re-download a snapshot, then go through the normal switch flow so a
    /// changed subscription still gets a diff and confirmation.
    async fn update(&mut self, id: &str) {
        let profile = match self.profile(id) {
            Ok(profile) => profile,
            Err(err) => {
                self.error_toast("update failed", &err);
                return;
            }
        };

        let raw = match subscription::download(&profile.url).await {
            Ok(raw) => raw,
            Err(err) => {
                self.error_toast("update failed", &err);
                return;
            }
        };
        let parsed = match subscription::parse_subscription(&raw) {
            Ok(parsed) => parsed,
            Err(err) => {
                self.error_toast("update failed", &err);
                return;
            }
        };

        let nodes = u32::try_from(parsed.report.total_nodes).unwrap_or(u32::MAX);
        let protocols = subscription::summarize(&parsed.report);
        if let Err(err) = Store::touch_updated(&self.paths, id, &raw, nodes, &protocols, now_ms()) {
            self.error_toast("update failed", &err);
            return;
        }

        self.emit_profiles();
        self.switch(id).await;
    }

    async fn delete(&mut self, id: &str) {
        match Store::delete_profile(&self.paths, id) {
            Ok(()) => {
                let was_current = self.current.as_deref() == Some(id);
                if was_current {
                    // Keep `last_good`: it is still the config the
                    // controller is running and remains a valid rollback.
                    self.current = None;
                }
                self.emit_profiles();
                if was_current {
                    self.toast(
                        ToastKind::Info,
                        "deleted the active subscription; the running config is unchanged",
                    );
                } else {
                    self.toast(ToastKind::Success, format!("deleted subscription {id}"));
                }
            }
            Err(err) => self.error_toast("delete failed", &err),
        }
    }
}

/// A blank name is allowed by the form; fall back to the host so the list
/// still has something recognisable to show.
fn display_name(raw: &str, url: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "subscription".to_string())
}

/// mihomo's `external-controller` is `host:port`, not a URL; keep the
/// address the client dials but strip the scheme and trailing slash.
fn controller_address(endpoint: &str) -> String {
    match url::Url::parse(endpoint) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_string(),
            _ => endpoint.trim_end_matches('/').to_string(),
        },
        Err(_) => endpoint.trim_end_matches('/').to_string(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_yaml_ng::Value;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use super::*;
    use crate::store::ProfileMeta;

    const BASE: &str = r#"
mixed-port: 7890
log-level: info
proxies:
  - name: base-node
    type: ss
    server: base.example
    port: 443
    cipher: aes-128-gcm
    password: pw
proxy-groups:
  - name: PROXY
    type: select
    proxies: [base-node]
rules:
  - MATCH,PROXY
"#;

    const SUB_ONE: &str = r#"
proxies:
  - name: sub-node
    type: ss
    server: sub.example
    port: 443
    cipher: aes-128-gcm
    password: pw
proxy-groups:
  - name: PROXY
    type: select
    proxies: [sub-node]
rules:
  - MATCH,DIRECT
"#;

    const SUB_TWO: &str = r#"
proxies:
  - name: sub2-node
    type: ss
    server: sub2.example
    port: 443
    cipher: aes-128-gcm
    password: pw
proxy-groups:
  - name: PROXY
    type: select
    proxies: [sub2-node]
rules:
  - MATCH,DIRECT
"#;

    /// Scripted `PUT /configs`: succeeds except for a fixed window of calls.
    #[derive(Clone)]
    struct ReloadScript {
        calls: Arc<AtomicUsize>,
        fail_at: usize,
        fail_times: usize,
    }

    impl ReloadScript {
        fn new(fail_at: usize, fail_times: usize) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                fail_at,
                fail_times,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Respond for ReloadScript {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call >= self.fail_at && call - self.fail_at < self.fail_times {
                ResponseTemplate::new(500).set_body_string("reload rejected")
            } else {
                ResponseTemplate::new(204)
            }
        }
    }

    struct Harness {
        _tmp: TempDir,
        paths: Paths,
        server: MockServer,
        commands: mpsc::UnboundedSender<Command>,
        events: mpsc::UnboundedReceiver<Event>,
        reloads: ReloadScript,
    }

    async fn harness(base: &str, reloads: ReloadScript) -> Harness {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::resolve(Some(tmp.path().to_str().unwrap())).unwrap();
        paths.ensure().unwrap();
        std::fs::write(paths.base_yaml(), base).unwrap();

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/configs"))
            .respond_with(reloads.clone())
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/version"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"version": "1.19.0"})),
            )
            .mount(&server)
            .await;

        let client = Arc::new(Client::new(&server.uri(), None).unwrap());
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        spawn_profile_worker(client, paths.clone(), command_rx, event_tx);

        Harness {
            _tmp: tmp,
            paths,
            server,
            commands: command_tx,
            events: event_rx,
            reloads,
        }
    }

    fn add_profile(paths: &Paths, name: &str, url: &str, raw: &str) -> ProfileMeta {
        Store::add_profile(paths, name, url, raw, 1, "ss(1)", 1_000).unwrap()
    }

    fn subscription_choices() -> BTreeMap<Block, Source> {
        let mut choices = BTreeMap::new();
        choices.insert(Block::Proxies, Source::Subscription);
        choices.insert(Block::ProxyGroups, Source::Subscription);
        choices.insert(Block::Rules, Source::Subscription);
        choices
    }

    async fn next_event(harness: &mut Harness) -> Event {
        tokio::time::timeout(Duration::from_secs(10), harness.events.recv())
            .await
            .expect("worker produced an event")
            .expect("worker is still running")
    }

    async fn next_diff(harness: &mut Harness) -> Vec<merge::BlockDiff> {
        loop {
            if let Event::DiffProposal { diffs, .. } = next_event(harness).await {
                return diffs;
            }
        }
    }

    async fn next_reload(harness: &mut Harness) -> (bool, bool, Option<String>) {
        loop {
            if let Event::ReloadResult {
                ok,
                rolled_back,
                error,
            } = next_event(harness).await
            {
                return (ok, rolled_back, error);
            }
        }
    }

    async fn next_profiles(harness: &mut Harness) -> Vec<ProfileMeta> {
        loop {
            if let Event::ProfilesUpdated(profiles) = next_event(harness).await {
                return profiles;
            }
        }
    }

    fn put_payloads(requests: &[Request]) -> Vec<serde_json::Value> {
        requests
            .iter()
            .filter(|request| request.method.as_str() == "PUT")
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }

    fn server_address(server: &MockServer) -> String {
        let url = url::Url::parse(&server.uri()).unwrap();
        format!("{}:{}", url.host_str().unwrap(), url.port().unwrap())
    }

    #[tokio::test]
    async fn confirm_switch_applies_and_records_policy() {
        let mut harness = harness(BASE, ReloadScript::new(usize::MAX, 0)).await;
        add_profile(&harness.paths, "sub one", "https://example/sub", SUB_ONE);

        harness
            .commands
            .send(Command::SwitchProfile("s1".into()))
            .unwrap();
        let diffs = next_diff(&mut harness).await;
        assert!(
            diffs
                .iter()
                .any(|diff| diff.block == Block::Proxies && !diff.equal),
            "the proxies block differs: {diffs:?}"
        );

        harness
            .commands
            .send(Command::ConfirmSwitch {
                profile_id: "s1".into(),
                choices: subscription_choices(),
                remember: true,
            })
            .unwrap();
        let (ok, rolled_back, error) = next_reload(&mut harness).await;
        assert!(ok, "apply succeeded: {error:?}");
        assert!(!rolled_back);

        // The controller received the merged config, not merely a valid one.
        let requests = harness.server.received_requests().await.unwrap();
        let payloads = put_payloads(&requests);
        assert_eq!(payloads.len(), 1, "one reload, no rollback needed");
        let payload = payloads[0]["payload"].as_str().unwrap();
        assert!(payload.contains("sub-node"), "payload: {payload}");
        assert!(!payload.contains("base-node"));

        // runtime.yaml is private, current, and dials the same controller.
        let runtime_path = harness.paths.runtime_yaml();
        let mode = std::fs::metadata(&runtime_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "runtime.yaml is 0600");
        let runtime: Value =
            serde_yaml_ng::from_str(&std::fs::read_to_string(&runtime_path).unwrap()).unwrap();
        assert_eq!(
            runtime.get("external-controller").and_then(Value::as_str),
            Some(server_address(&harness.server).as_str())
        );
        let proxies = runtime.get("proxies").and_then(Value::as_sequence).unwrap();
        assert_eq!(
            proxies[0].get("name").and_then(Value::as_str),
            Some("sub-node")
        );

        // Remembering the choices writes them into the profile index.
        let profiles = Store::load_profiles(&harness.paths).unwrap();
        let policy = &profiles[0].policy;
        assert_eq!(
            policy.get("proxies").map(String::as_str),
            Some("subscription")
        );
        assert_eq!(
            policy.get("rules").map(String::as_str),
            Some("subscription")
        );
    }

    #[tokio::test]
    async fn reload_failure_rolls_back_to_last_good() {
        // First apply succeeds, the second is rejected, the rollback reload
        // succeeds again.
        let mut harness = harness(BASE, ReloadScript::new(2, 1)).await;
        add_profile(&harness.paths, "one", "https://example/one", SUB_ONE);
        add_profile(&harness.paths, "two", "https://example/two", SUB_TWO);

        for id in ["s1", "s2"] {
            harness
                .commands
                .send(Command::SwitchProfile(id.into()))
                .unwrap();
            next_diff(&mut harness).await;
            harness
                .commands
                .send(Command::ConfirmSwitch {
                    profile_id: id.into(),
                    choices: subscription_choices(),
                    remember: false,
                })
                .unwrap();
            let (ok, rolled_back, error) = next_reload(&mut harness).await;
            if id == "s1" {
                assert!(ok, "first apply works: {error:?}");
            } else {
                assert!(!ok);
                assert!(rolled_back, "the old config was restored: {error:?}");
                assert!(error.unwrap().contains("reload failed"));
            }
        }

        assert_eq!(harness.reloads.calls(), 3, "apply, failed apply, rollback");
        let requests = harness.server.received_requests().await.unwrap();
        let payloads = put_payloads(&requests);
        assert_eq!(payloads.len(), 3);
        let last = payloads.last().unwrap()["payload"].as_str().unwrap();
        assert!(
            last.contains("sub-node"),
            "rollback resubmits the last good config"
        );
        assert!(!last.contains("sub2-node"));

        let runtime = std::fs::read_to_string(harness.paths.runtime_yaml()).unwrap();
        assert!(
            runtime.contains("sub-node"),
            "runtime.yaml matches what was rolled back"
        );
    }

    #[tokio::test]
    async fn rollback_failure_surfaces_error() {
        let mut harness = harness(BASE, ReloadScript::new(1, usize::MAX)).await;
        add_profile(&harness.paths, "one", "https://example/one", SUB_ONE);

        harness
            .commands
            .send(Command::SwitchProfile("s1".into()))
            .unwrap();
        next_diff(&mut harness).await;
        harness
            .commands
            .send(Command::ConfirmSwitch {
                profile_id: "s1".into(),
                choices: subscription_choices(),
                remember: false,
            })
            .unwrap();

        let (ok, rolled_back, error) = next_reload(&mut harness).await;
        assert!(!ok);
        assert!(!rolled_back, "there was no last good config to restore");
        let error = error.unwrap();
        assert!(error.contains("500"), "the reason survives: {error}");
        assert_eq!(harness.reloads.calls(), 1, "no rollback request");
    }

    #[tokio::test]
    async fn update_profile_refetches_and_emits_profiles_updated() {
        let mut harness = harness(BASE, ReloadScript::new(usize::MAX, 0)).await;
        let url = format!("{}/sub", harness.server.uri());
        add_profile(&harness.paths, "one", &url, SUB_TWO);
        Mock::given(method("GET"))
            .and(path("/sub"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SUB_ONE))
            .mount(&harness.server)
            .await;

        harness
            .commands
            .send(Command::UpdateProfile("s1".into()))
            .unwrap();

        let profiles = next_profiles(&mut harness).await;
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].nodes, 1);
        assert!(profiles[0].updated_ms > 1_000, "timestamp refreshed");
        let snapshot = Store::read_snapshot(&harness.paths, "s1").unwrap();
        assert!(snapshot.contains("sub-node"));
        assert!(!snapshot.contains("sub2-node"));

        // The refreshed snapshot goes through the usual diff and waits for
        // the user instead of silently applying.
        let diffs = next_diff(&mut harness).await;
        assert!(
            diffs
                .iter()
                .any(|diff| diff.block == Block::Proxies && !diff.equal)
        );
    }
}
