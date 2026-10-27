//! Application state and key handling.
//!
//! `App` owns everything the UI draws and decides, for each keypress, which
//! [`Command`] the caller should run against the API. Keeping the network out
//! of this module keeps the whole thing testable without a live mihomo.

use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::style::Color;

use crate::api::{Configs, Connection, Connections, Group, LogLine, Version};
use crate::merge::{Block, BlockDiff, Source};
use crate::store::ProfileMeta;

/// How many traffic samples the graphs keep (~30 s at 4 Hz).
pub const TRAFFIC_HISTORY: usize = 120;
/// How many log lines to hold on to.
pub const LOG_CAPACITY: usize = 500;
/// How long a toast stays on screen.
const TOAST_TTL: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Proxies,
    Connections,
    Logs,
    Subscriptions,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Overview,
        Tab::Proxies,
        Tab::Connections,
        Tab::Logs,
        Tab::Subscriptions,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Proxies => "Proxies",
            Tab::Connections => "Connections",
            Tab::Logs => "Logs",
            Tab::Subscriptions => "Subscriptions",
        }
    }

    fn offset(self, delta: isize) -> Tab {
        let len = Self::ALL.len() as isize;
        let index = Self::ALL.iter().position(|t| *t == self).unwrap_or(0) as isize;
        Self::ALL[(index + delta).rem_euclid(len) as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub kind: ToastKind,
    pub text: String,
    pub at: Instant,
}

impl Toast {
    pub fn is_expired(&self, now: Instant) -> bool {
        now.duration_since(self.at) >= TOAST_TTL
    }

    pub fn color(&self) -> Color {
        match self.kind {
            ToastKind::Info => Color::Cyan,
            ToastKind::Success => Color::Green,
            ToastKind::Error => Color::Red,
        }
    }
}

/// Something the app wants done over the network. The event loop runs these;
/// the app itself never touches a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    SetMode(String),
    SetAllowLan(bool),
    SetTun(bool),
    ApplySettings {
        mode: String,
        mixed_port: u16,
        allow_lan: bool,
        tun: bool,
    },
    Select {
        group: String,
        member: String,
    },
    TestMember {
        name: String,
        url: String,
        timeout_ms: u64,
    },
    /// Run many delay tests with bounded concurrency.
    TestMany {
        generation: u64,
        names: Vec<String>,
        url: String,
        timeout_ms: u64,
    },
    /// Cancel an in-flight `TestMany` generation.
    CancelTests(u64),
    CloseConnection(String),
    CloseAllConnections,
    Refresh,
    /// Switch to (or re-apply) the named subscription; the worker diffs it
    /// against the base config and either applies or asks for confirmation.
    SwitchProfile(String),
    /// Apply a confirmed switch with the exact per-block choices shown.
    ConfirmSwitch {
        profile_id: String,
        choices: BTreeMap<Block, Source>,
        remember: bool,
    },
    /// Download and import a new subscription.
    AddProfile {
        name: String,
        url: String,
    },
    /// Re-download a subscription's snapshot and re-apply it.
    UpdateProfile(String),
    /// Remove a subscription and its snapshot.
    DeleteProfile(String),
    /// Save custom prepend-rules and append-rules to base.yaml.
    SaveRules {
        prepend: Vec<String>,
        append: Vec<String>,
    },
}

/// Results flowing back into the app from the background tasks.
#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
    /// A mouse click. Only presses of the left button are actionable.
    Mouse(MouseEvent),
    Tick,
    /// Periodic re-read of configs, groups and connections.
    Snapshot(Box<Snapshot>),
    Traffic {
        up: u64,
        down: u64,
    },
    Log(LogLine),
    /// A single member's delay test finished.
    MemberDelay {
        name: String,
        result: Result<u64, String>,
    },
    /// One member's delay test finished (carries its generation).
    TestProgress {
        generation: u64,
        name: String,
        result: Result<u64, String>,
    },
    /// All members of a generation finished (or were cancelled).
    TestsFinished {
        generation: u64,
        ok: usize,
        total: usize,
    },
    Toast(ToastKind, String),
    /// The controller connection failed; stop hammering it.
    Fatal(String),
    /// The worker compared the base config with a subscription and needs a
    /// decision before anything is applied.
    DiffProposal {
        profile_id: String,
        diffs: Vec<BlockDiff>,
        /// The profile's saved policy (`Block::key()` → `"base" |
        /// "subscription"`), used to prefill the choices.
        policy: BTreeMap<String, String>,
    },
    /// The profile list changed: add, update, delete or a finished switch.
    ProfilesUpdated(Vec<ProfileMeta>),
    /// Custom rules in base.yaml were updated.
    RulesUpdated {
        prepend: Vec<String>,
        append: Vec<String>,
    },
    /// A config file changed on disk. Never applied automatically.
    ExternalChange {
        path: String,
    },
    /// Outcome of applying a merged config through the controller.
    ReloadResult {
        ok: bool,
        rolled_back: bool,
        error: Option<String>,
    },
}

#[derive(Debug, Default)]
pub struct Snapshot {
    pub version: Option<Version>,
    pub configs: Option<Configs>,
    pub groups: Vec<Group>,
    pub connections: Connections,
}

/// Which field of the add-subscription form has the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputField {
    Url,
    Name,
}

impl InputField {
    fn toggle(self) -> InputField {
        match self {
            InputField::Url => InputField::Name,
            InputField::Name => InputField::Url,
        }
    }
}

/// The add-subscription form floating over the page.
pub struct InputOverlay {
    pub url: String,
    pub name: String,
    pub field: InputField,
}

/// The switch confirmation page: one row per config block, each with a
/// base-or-subscription choice.
pub struct PendingSwitch {
    pub profile_id: String,
    pub diffs: Vec<BlockDiff>,
    /// Per-block source; blocks left out default to the base side.
    pub choices: BTreeMap<Block, Source>,
    pub cursor: usize,
    /// Save the choices as the profile's policy on confirm.
    pub remember: bool,
}

/// The quick settings / options overlay floating over the page.
pub struct SettingsOverlay {
    pub mode: String,
    pub mixed_port: String,
    pub allow_lan: bool,
    pub tun: bool,
    pub cursor: usize,
}

/// The custom prepend/append rules management overlay.
#[derive(Debug, Clone)]
pub struct RulesOverlay {
    /// true: Prepend Rules (置顶/优先), false: Append Rules (追加/兜底)
    pub is_prepend: bool,
    pub cursor: usize,
    pub adding: bool,
    pub input_text: String,
}

pub struct App {
    pub tab: Tab,
    pub should_quit: bool,
    pub version: Option<Version>,
    pub configs: Option<Configs>,
    pub groups: Vec<Group>,
    pub group_idx: usize,
    pub member_idx: usize,
    pub traffic: Traffic,
    pub connections: Connections,
    pub conn_idx: Option<usize>,
    pub log: VecDeque<LogLine>,
    pub toast: Option<Toast>,
    pub endpoint: String,
    pub delay_url: String,
    pub timeout_ms: u64,
    /// Set when the controller is unreachable, so the UI can say so.
    pub offline: Option<String>,
    /// True while a group-wide delay test is running.
    pub testing: bool,
    /// Generation counter for the concurrent test pool; stale results are
    /// dropped by comparing against this.
    pub testing_gen: u64,
    pub testing_total: usize,
    pub testing_done: usize,
    pub testing_ok: usize,
    /// View-only reordering of the member list by measured delay.
    pub sort_by_delay: bool,
    /// Displayed member row -> original member index, captured by the
    /// renderer so mouse clicks land on the right node when sorted.
    pub members_display_order: std::cell::RefCell<Vec<usize>>,
    /// Imported subscriptions, in display order.
    pub profiles: Vec<ProfileMeta>,
    /// Highlighted row on the Subscriptions page; also the profile that
    /// `[`/`]`/`v` treat as current.
    pub sub_idx: usize,
    /// Open switch confirmation page.
    pub pending: Option<PendingSwitch>,
    /// Open add-subscription form.
    pub input: Option<InputOverlay>,
    /// Open quick settings / options form.
    pub settings: Option<SettingsOverlay>,
    /// Open custom rules form.
    pub rules: Option<RulesOverlay>,
    /// Custom prepend rules loaded from base.yaml.
    pub prepend_rules: Vec<String>,
    /// Custom append rules loaded from base.yaml.
    pub append_rules: Vec<String>,
    /// Path of the config file changed externally, awaiting re-apply.
    pub external_dirty: Option<String>,
    /// Profile id waiting for the second `d` of the two-stage delete.
    pub delete_armed: Option<String>,
    /// Width of the rendered tab strip. Written by the header and read by
    /// mouse hit-testing so both use exactly the same geometry.
    pub tab_width: Cell<u16>,
    /// Visible subscriptions table and its first visible row, captured after
    /// a draw. A click maps back through these to a profile index.
    pub subs_area: Cell<Rect>,
    pub subs_top: Cell<usize>,
    /// Same for the proxy member table.
    pub members_area: Cell<Rect>,
    pub members_top: Cell<usize>,
}

/// Rolling traffic counters for the overview graphs.
#[derive(Default)]
pub struct Traffic {
    pub up: u64,
    pub down: u64,
    pub up_history: VecDeque<u64>,
    pub down_history: VecDeque<u64>,
}

impl Traffic {
    fn push(&mut self, up: u64, down: u64) {
        self.up = up;
        self.down = down;
        self.up_history.push_back(up);
        self.down_history.push_back(down);
        while self.up_history.len() > TRAFFIC_HISTORY {
            self.up_history.pop_front();
        }
        while self.down_history.len() > TRAFFIC_HISTORY {
            self.down_history.pop_front();
        }
    }
}

impl App {
    pub fn new(endpoint: String, delay_url: String, timeout_ms: u64) -> Self {
        Self {
            tab: Tab::Overview,
            should_quit: false,
            version: None,
            configs: None,
            groups: Vec::new(),
            group_idx: 0,
            member_idx: 0,
            traffic: Traffic::default(),
            connections: Connections::default(),
            conn_idx: None,
            log: VecDeque::new(),
            toast: None,
            endpoint,
            delay_url,
            timeout_ms,
            offline: None,
            testing: false,
            testing_gen: 0,
            testing_total: 0,
            testing_done: 0,
            testing_ok: 0,
            sort_by_delay: false,
            members_display_order: std::cell::RefCell::new(Vec::new()),
            profiles: Vec::new(),
            sub_idx: 0,
            pending: None,
            input: None,
            settings: None,
            rules: None,
            prepend_rules: Vec::new(),
            append_rules: Vec::new(),
            external_dirty: None,
            delete_armed: None,
            tab_width: Cell::new(0),
            subs_area: Cell::new(Rect::default()),
            subs_top: Cell::new(0),
            members_area: Cell::new(Rect::default()),
            members_top: Cell::new(0),
        }
    }

    pub fn open_rules(&mut self) {
        self.rules = Some(RulesOverlay {
            is_prepend: true,
            cursor: 0,
            adding: false,
            input_text: String::new(),
        });
    }

    pub fn mode(&self) -> &str {
        self.configs
            .as_ref()
            .map(|c| c.mode.as_str())
            .unwrap_or("-")
    }

    pub fn version_line(&self) -> String {
        match &self.version {
            Some(v) if v.meta => format!("{} (meta)", v.version),
            Some(v) => v.version.clone(),
            None => "-".to_string(),
        }
    }

    pub fn current_group(&self) -> Option<&Group> {
        self.groups.get(self.group_idx)
    }

    pub fn set_toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toast = Some(Toast {
            kind,
            text: text.into(),
            at: Instant::now(),
        });
    }

    pub fn active_toast(&self, now: Instant) -> Option<&Toast> {
        self.toast.as_ref().filter(|t| !t.is_expired(now))
    }

    /// Put freshly fetched state in place, preserving the user's cursor.
    pub fn apply_snapshot(&mut self, snapshot: Snapshot) {
        if let Some(version) = snapshot.version {
            self.version = Some(version);
        }
        if let Some(configs) = snapshot.configs {
            self.configs = Some(configs);
        }
        self.groups = snapshot.groups;
        self.connections = snapshot.connections;
        self.offline = None;

        self.clamp_cursors();
    }

    /// Keep the group/member/connection cursors inside their new bounds.
    /// Groups come back in a stable order, but a reload can still drop one.
    fn clamp_cursors(&mut self) {
        if self.groups.is_empty() {
            self.group_idx = 0;
            self.member_idx = 0;
        } else {
            self.group_idx = self.group_idx.min(self.groups.len() - 1);
            let len = self.groups[self.group_idx].members.len();
            self.member_idx = if len == 0 {
                0
            } else {
                self.member_idx.min(len - 1)
            };
        }

        self.conn_idx = match self.conn_idx {
            Some(i) if i < self.connections.connections.len() => Some(i),
            Some(_) if self.connections.connections.is_empty() => None,
            Some(_) => Some(self.connections.connections.len() - 1),
            None => None,
        };
    }

    pub fn push_log(&mut self, line: LogLine) {
        if self.log.len() >= LOG_CAPACITY {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    pub fn apply_member_delay(&mut self, name: &str, result: &Result<u64, String>) {
        // The controller reports 0 for members that failed to test.
        let delay = result.as_ref().ok().copied().filter(|value| *value > 0);
        for group in &mut self.groups {
            for member in &mut group.members {
                if member.name == name {
                    member.delay = delay;
                }
            }
        }
    }

    pub fn selected_connection(&self) -> Option<&Connection> {
        self.conn_idx
            .and_then(|i| self.connections.connections.get(i))
    }

    /// Translate a keypress into an action. `None` means the key was consumed
    /// as pure UI navigation.
    ///
    /// Priority: add form > confirm page > global keys > per-tab keys. An
    /// open form or confirm page swallows everything — including `q` and the
    /// tab shortcuts — until Esc (or Enter) closes it.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Command> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Ctrl+C always quits, even with a modal layer open.
        if ctrl && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return None;
        }

        if self.rules.is_some() {
            return self.on_key_rules(key);
        }
        if self.settings.is_some() {
            return self.on_key_settings(key);
        }
        if self.input.is_some() {
            return self.on_key_input(key);
        }
        if self.pending.is_some() {
            return self.on_key_pending(key);
        }

        // Quitting and tab switching work from anywhere.
        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
                return None;
            }
            KeyCode::Char('o') => {
                let configs = self.configs.as_ref();
                let port = configs.map(|c| c.mixed_port).unwrap_or(7890);
                let port_str = if port == 0 {
                    "7890".to_string()
                } else {
                    port.to_string()
                };
                self.settings = Some(SettingsOverlay {
                    mode: self.mode().to_string(),
                    mixed_port: port_str,
                    allow_lan: configs.map(|c| c.allow_lan).unwrap_or(false),
                    tun: configs.map(|c| c.tun.enable).unwrap_or(false),
                    cursor: 0,
                });
                return None;
            }
            KeyCode::Tab => {
                self.tab = self.tab.offset(1);
                self.delete_armed = None;
                return None;
            }
            KeyCode::BackTab => {
                self.tab = self.tab.offset(-1);
                self.delete_armed = None;
                return None;
            }
            KeyCode::Char(c @ '1'..='5') => {
                self.tab = Tab::ALL[c as usize - '1' as usize];
                self.delete_armed = None;
                return None;
            }
            KeyCode::Esc => {
                if self.testing {
                    self.testing = false;
                    return Some(Command::CancelTests(self.testing_gen));
                }
                // With no modal open, Esc only disarms the delete prompt.
                self.delete_armed = None;
                return None;
            }
            KeyCode::Char('r') => return Some(Command::Refresh),
            // Cycle the highlighted subscription from anywhere.
            KeyCode::Char('[') => {
                return self.cycled_profile(-1).map(Command::SwitchProfile);
            }
            KeyCode::Char(']') => {
                return self.cycled_profile(1).map(Command::SwitchProfile);
            }
            // Re-apply the current subscription, e.g. after an external edit.
            KeyCode::Char('v') => return self.reapply_current(),
            _ => {}
        }

        match self.tab {
            Tab::Overview => self.on_key_overview(key),
            Tab::Proxies => self.on_key_proxies(key),
            Tab::Connections => self.on_key_connections(key),
            Tab::Logs => self.on_key_logs(key),
            Tab::Subscriptions => self.on_key_subscriptions(key),
        }
    }

    /// The profile id one step from the highlighted one, wrapping around.
    fn cycled_profile(&self, delta: isize) -> Option<String> {
        let len = self.profiles.len();
        if len == 0 {
            return None;
        }
        let current = self.sub_idx.min(len - 1) as isize;
        let next = (current + delta).rem_euclid(len as isize) as usize;
        Some(self.profiles[next].id.clone())
    }

    /// Every member name across every group, in first-seen order and
    /// deduplicated — the `A` tier of concurrent testing.
    fn all_member_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for group in &self.groups {
            for member in &group.members {
                if !names.iter().any(|existing| existing == &member.name) {
                    names.push(member.name.clone());
                }
            }
        }
        names
    }

    /// The highlighted profile, if any.
    fn current_profile(&self) -> Option<&ProfileMeta> {
        self.profiles.get(self.sub_idx)
    }

    /// `v`: rebuild and reload the current subscription in place, picking up
    /// external edits.
    fn reapply_current(&mut self) -> Option<Command> {
        let id = self.current_profile()?.id.clone();
        self.external_dirty = None;
        Some(Command::SwitchProfile(id))
    }

    fn on_key_overview(&mut self, key: KeyEvent) -> Option<Command> {
        match key.code {
            KeyCode::Char('m') => {
                let mode = self.next_mode().to_string();
                self.set_mode_local(&mode);
                Some(Command::SetMode(mode))
            }
            KeyCode::Char('M') => {
                let mode = self.prev_mode().to_string();
                self.set_mode_local(&mode);
                Some(Command::SetMode(mode))
            }
            KeyCode::Char('l') | KeyCode::Char('L') => {
                let next = !self.configs.as_ref().map(|c| c.allow_lan).unwrap_or(false);
                if let Some(configs) = self.configs.as_mut() {
                    configs.allow_lan = next;
                }
                Some(Command::SetAllowLan(next))
            }
            KeyCode::Char('u') | KeyCode::Char('U') => {
                let next = !self.configs.as_ref().map(|c| c.tun.enable).unwrap_or(false);
                if let Some(configs) = self.configs.as_mut() {
                    configs.tun.enable = next;
                }
                Some(Command::SetTun(next))
            }
            KeyCode::Char('R') => {
                self.open_rules();
                None
            }
            _ => None,
        }
    }

    /// Reflect a mode change immediately rather than waiting for the next
    /// refresh, so the badge responds to the key and rapid presses cycle
    /// from the mode the user can actually see on screen.
    fn set_mode_local(&mut self, mode: &str) {
        if let Some(configs) = self.configs.as_mut() {
            configs.mode = mode.to_string();
        }
    }

    fn next_mode(&self) -> &'static str {
        let current = self.mode();
        let index = crate::api::MODES
            .iter()
            .position(|m| *m == current)
            .unwrap_or(0);
        crate::api::MODES[(index + 1) % crate::api::MODES.len()]
    }

    fn prev_mode(&self) -> &'static str {
        let current = self.mode();
        let index = crate::api::MODES
            .iter()
            .position(|m| *m == current)
            .unwrap_or(0);
        crate::api::MODES[(index + crate::api::MODES.len() - 1) % crate::api::MODES.len()]
    }

    fn on_key_proxies(&mut self, key: KeyEvent) -> Option<Command> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.group_idx = self.group_idx.saturating_sub(1);
                self.member_idx = 0;
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.groups.is_empty() {
                    self.group_idx = (self.group_idx + 1).min(self.groups.len() - 1);
                    self.member_idx = 0;
                }
                None
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.member_idx = self.member_idx.saturating_sub(1);
                None
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(group) = self.current_group()
                    && !group.members.is_empty()
                {
                    self.member_idx = (self.member_idx + 1).min(group.members.len() - 1);
                }
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let group = self.current_group()?;
                let member = group.members.get(self.member_idx)?;
                Some(Command::Select {
                    group: group.name.clone(),
                    member: member.name.clone(),
                })
            }
            KeyCode::Char('t') => {
                let group = self.current_group()?;
                let member = group.members.get(self.member_idx)?;
                Some(Command::TestMember {
                    name: member.name.clone(),
                    url: group
                        .test_url
                        .clone()
                        .unwrap_or_else(|| self.delay_url.clone()),
                    timeout_ms: self.timeout_ms,
                })
            }
            KeyCode::Char('T') => {
                let group = self.current_group()?;
                let names: Vec<String> = {
                    let mut out = Vec::new();
                    for member in &group.members {
                        if !out.iter().any(|existing| existing == &member.name) {
                            out.push(member.name.clone());
                        }
                    }
                    out
                };
                if names.is_empty() {
                    return None;
                }
                let url = group
                    .test_url
                    .clone()
                    .unwrap_or_else(|| self.delay_url.clone());
                let timeout_ms = self.timeout_ms;
                self.testing = true;
                self.testing_gen = self.testing_gen.wrapping_add(1);
                self.testing_total = names.len();
                self.testing_done = 0;
                self.testing_ok = 0;
                Some(Command::TestMany {
                    generation: self.testing_gen,
                    names,
                    url,
                    timeout_ms,
                })
            }
            KeyCode::Char('A') => {
                let names = self.all_member_names();
                if names.is_empty() {
                    return None;
                }
                let url = self
                    .current_group()?
                    .test_url
                    .clone()
                    .unwrap_or_else(|| self.delay_url.clone());
                let timeout_ms = self.timeout_ms;
                self.testing = true;
                self.testing_gen = self.testing_gen.wrapping_add(1);
                self.testing_total = names.len();
                self.testing_done = 0;
                self.testing_ok = 0;
                Some(Command::TestMany {
                    generation: self.testing_gen,
                    names,
                    url,
                    timeout_ms,
                })
            }
            KeyCode::Char('s') => {
                self.sort_by_delay = !self.sort_by_delay;
                None
            }
            _ => None,
        }
    }

    fn on_key_connections(&mut self, key: KeyEvent) -> Option<Command> {
        let count = self.connections.connections.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.conn_idx = Some(match self.conn_idx {
                    Some(i) => i.saturating_sub(1),
                    None if count > 0 => count - 1,
                    None => 0,
                });
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if count > 0 {
                    self.conn_idx = Some(match self.conn_idx {
                        Some(i) => (i + 1).min(count - 1),
                        None => 0,
                    });
                }
                None
            }
            KeyCode::Char('x') => {
                let id = self.selected_connection()?.id.clone();
                Some(Command::CloseConnection(id))
            }
            KeyCode::Char('X') => Some(Command::CloseAllConnections),
            _ => None,
        }
    }

    fn on_key_logs(&mut self, key: KeyEvent) -> Option<Command> {
        match key.code {
            KeyCode::Char('c') => {
                self.log.clear();
                None
            }
            _ => None,
        }
    }

    fn on_key_subscriptions(&mut self, key: KeyEvent) -> Option<Command> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.sub_idx = self.sub_idx.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.profiles.is_empty() {
                    self.sub_idx = (self.sub_idx + 1).min(self.profiles.len() - 1);
                }
                None
            }
            KeyCode::Enter => {
                let id = self.current_profile()?.id.clone();
                Some(Command::SwitchProfile(id))
            }
            KeyCode::Char('a') => {
                self.input = Some(InputOverlay {
                    url: String::new(),
                    name: String::new(),
                    field: InputField::Url,
                });
                None
            }
            KeyCode::Char('u') => {
                let id = self.current_profile()?.id.clone();
                Some(Command::UpdateProfile(id))
            }
            KeyCode::Char('d') => {
                let id = self.current_profile()?.id.clone();
                if self.delete_armed.as_deref() == Some(id.as_str()) {
                    self.delete_armed = None;
                    Some(Command::DeleteProfile(id))
                } else {
                    self.delete_armed = Some(id);
                    None
                }
            }
            _ => None,
        }
    }

    /// Keys on the switch confirmation page. Every key is consumed here.
    fn on_key_pending(&mut self, key: KeyEvent) -> Option<Command> {
        match key.code {
            KeyCode::Char('b') | KeyCode::Char('s') | KeyCode::Left | KeyCode::Right => {
                if let Some(pending) = self.pending.as_mut()
                    && let Some(diff) = pending.diffs.get(pending.cursor)
                {
                    let block = diff.block;
                    let current = pending.choices.get(&block).copied().unwrap_or(Source::Base);
                    pending.choices.insert(block, current.toggle());
                }
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(pending) = self.pending.as_mut() {
                    pending.cursor = pending.cursor.saturating_sub(1);
                }
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(pending) = self.pending.as_mut() {
                    pending.cursor =
                        (pending.cursor + 1).min(pending.diffs.len().saturating_sub(1));
                }
                None
            }
            KeyCode::Char('r') => {
                if let Some(pending) = self.pending.as_mut() {
                    pending.remember = !pending.remember;
                }
                None
            }
            KeyCode::Enter => match self.pending.take() {
                Some(pending) => Some(Command::ConfirmSwitch {
                    profile_id: pending.profile_id,
                    choices: pending.choices,
                    remember: pending.remember,
                }),
                None => None,
            },
            KeyCode::Esc => {
                self.pending = None;
                None
            }
            _ => None,
        }
    }

    /// Keys on the add-subscription form. Every key is consumed here.
    fn on_key_input(&mut self, key: KeyEvent) -> Option<Command> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.input = None;
                None
            }
            KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right => {
                if let Some(overlay) = self.input.as_mut() {
                    overlay.field = overlay.field.toggle();
                }
                None
            }
            KeyCode::Backspace => {
                if let Some(overlay) = self.input.as_mut() {
                    match overlay.field {
                        InputField::Url => {
                            overlay.url.pop();
                        }
                        InputField::Name => {
                            overlay.name.pop();
                        }
                    }
                }
                None
            }
            KeyCode::Enter => match self.input.take() {
                Some(overlay) if !overlay.url.trim().is_empty() => Some(Command::AddProfile {
                    name: overlay.name,
                    url: overlay.url,
                }),
                // No url yet: put the form back so nothing typed is lost.
                overlay => {
                    self.input = overlay;
                    None
                }
            },
            KeyCode::Char(c) if !ctrl => {
                if let Some(overlay) = self.input.as_mut() {
                    match overlay.field {
                        InputField::Url => overlay.url.push(c),
                        InputField::Name => overlay.name.push(c),
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Keys on the settings / options form. Every key is consumed here.
    fn on_key_settings(&mut self, key: KeyEvent) -> Option<Command> {
        let settings = self.settings.as_mut()?;
        match key.code {
            KeyCode::Esc => {
                self.settings = None;
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if settings.cursor > 0 {
                    settings.cursor -= 1;
                }
                None
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                if settings.cursor < 4 {
                    settings.cursor += 1;
                } else if key.code == KeyCode::Tab {
                    settings.cursor = 0;
                }
                None
            }
            KeyCode::BackTab => {
                if settings.cursor > 0 {
                    settings.cursor -= 1;
                } else {
                    settings.cursor = 4;
                }
                None
            }
            KeyCode::Char(' ')
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Char('h')
            | KeyCode::Char('l') => {
                match settings.cursor {
                    0 => {
                        let current = settings.mode.as_str();
                        let next = match current {
                            "rule" => "global",
                            "global" => "direct",
                            _ => "rule",
                        };
                        settings.mode = next.to_string();
                    }
                    2 => {
                        settings.allow_lan = !settings.allow_lan;
                    }
                    3 => {
                        settings.tun = !settings.tun;
                    }
                    4 => {
                        self.settings = None;
                        self.open_rules();
                        return None;
                    }
                    _ => {}
                }
                None
            }
            KeyCode::Backspace => {
                if settings.cursor == 1 {
                    settings.mixed_port.pop();
                }
                None
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if settings.cursor == 1 && settings.mixed_port.len() < 5 {
                    settings.mixed_port.push(c);
                }
                None
            }
            KeyCode::Enter => {
                let settings = self.settings.take().unwrap();
                if settings.cursor == 4 {
                    self.open_rules();
                    return None;
                }
                let port: u16 = settings.mixed_port.parse().unwrap_or(7890);
                if let Some(configs) = self.configs.as_mut() {
                    configs.mixed_port = port;
                    configs.allow_lan = settings.allow_lan;
                    configs.tun.enable = settings.tun;
                    configs.mode = settings.mode.clone();
                }
                Some(Command::ApplySettings {
                    mode: settings.mode,
                    mixed_port: port,
                    allow_lan: settings.allow_lan,
                    tun: settings.tun,
                })
            }
            _ => None,
        }
    }

    /// Keys on the custom rules management form.
    fn on_key_rules(&mut self, key: KeyEvent) -> Option<Command> {
        let rules = self.rules.as_mut()?;
        if rules.adding {
            match key.code {
                KeyCode::Esc => {
                    rules.adding = false;
                    rules.input_text.clear();
                    None
                }
                KeyCode::Backspace => {
                    rules.input_text.pop();
                    None
                }
                KeyCode::Char(c) => {
                    rules.input_text.push(c);
                    None
                }
                KeyCode::Enter => {
                    let text = rules.input_text.trim().to_string();
                    rules.adding = false;
                    rules.input_text.clear();
                    if !text.is_empty() {
                        if rules.is_prepend {
                            self.prepend_rules.push(text);
                            rules.cursor = self.prepend_rules.len().saturating_sub(1);
                        } else {
                            self.append_rules.push(text);
                            rules.cursor = self.append_rules.len().saturating_sub(1);
                        }
                        self.toast = Some(Toast {
                            kind: ToastKind::Success,
                            text: "Custom rule added".to_string(),
                            at: Instant::now(),
                        });
                        Some(Command::SaveRules {
                            prepend: self.prepend_rules.clone(),
                            append: self.append_rules.clone(),
                        })
                    } else {
                        None
                    }
                }
                _ => None,
            }
        } else {
            let list_len = if rules.is_prepend {
                self.prepend_rules.len()
            } else {
                self.append_rules.len()
            };

            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.rules = None;
                    None
                }
                KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Char('h')
                | KeyCode::Char('l') => {
                    rules.is_prepend = !rules.is_prepend;
                    rules.cursor = 0;
                    None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if rules.cursor > 0 {
                        rules.cursor -= 1;
                    }
                    None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if list_len > 0 && rules.cursor + 1 < list_len {
                        rules.cursor += 1;
                    }
                    None
                }
                KeyCode::Char('a') => {
                    rules.adding = true;
                    rules.input_text.clear();
                    None
                }
                KeyCode::Char('d') => {
                    if list_len > 0 && rules.cursor < list_len {
                        if rules.is_prepend {
                            self.prepend_rules.remove(rules.cursor);
                            if rules.cursor >= self.prepend_rules.len() && rules.cursor > 0 {
                                rules.cursor -= 1;
                            }
                        } else {
                            self.append_rules.remove(rules.cursor);
                            if rules.cursor >= self.append_rules.len() && rules.cursor > 0 {
                                rules.cursor -= 1;
                            }
                        }
                        self.toast = Some(Toast {
                            kind: ToastKind::Info,
                            text: "Rule deleted".to_string(),
                            at: Instant::now(),
                        });
                        Some(Command::SaveRules {
                            prepend: self.prepend_rules.clone(),
                            append: self.append_rules.clone(),
                        })
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
    }

    /// Handle a click. Modals are deliberately mouse-transparent for v1:
    /// the keyboard remains the only way to drive the confirm page and the
    /// add form, so a stray click cannot apply a config.
    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }

        if mouse.row == 0 {
            let width = match self.tab_width.get() {
                0 => u16::MAX,
                width => width,
            };
            let hit = crate::ui::tab_hitboxes(width)
                .into_iter()
                .find(|(range, _)| range.contains(&mouse.column))
                .map(|(_, tab)| tab);
            if let Some(tab) = hit {
                self.tab = tab;
                self.delete_armed = None;
            }
            return;
        }

        match self.tab {
            Tab::Subscriptions => self.click_subscription(mouse.column, mouse.row),
            Tab::Proxies => self.click_member(mouse.column, mouse.row),
            _ => {}
        }
    }

    fn click_subscription(&mut self, x: u16, y: u16) {
        let area = self.subs_area.get();
        if !area.contains(Position::new(x, y)) {
            return;
        }
        // The table pins a header row at the top of its area; clicks there
        // are not rows.
        let Some(row) = (y - area.y).checked_sub(1) else {
            return;
        };
        let index = self.subs_top.get() + row as usize;
        if index < self.profiles.len() {
            self.sub_idx = index;
            self.delete_armed = None;
        }
    }

    fn click_member(&mut self, x: u16, y: u16) {
        let area = self.members_area.get();
        if !area.contains(Position::new(x, y)) {
            return;
        }
        let members = self
            .current_group()
            .map(|group| group.members.len())
            .unwrap_or(0);
        let displayed = self.members_top.get() + (y - area.y) as usize;
        if displayed < members {
            let order = self.members_display_order.borrow();
            let original = order.get(displayed).copied().unwrap_or(displayed);
            self.member_idx = original;
        }
    }

    /// Apply an event, returning a command when the event implies follow-up work.
    pub fn on_event(&mut self, event: Event) -> Option<Command> {
        match event {
            Event::Key(key) => self.on_key(key),
            Event::Mouse(mouse) => {
                self.on_mouse(mouse);
                None
            }
            Event::Tick => None,
            Event::Snapshot(snapshot) => {
                self.apply_snapshot(*snapshot);
                None
            }
            Event::Traffic { up, down } => {
                self.traffic.push(up, down);
                None
            }
            Event::Log(line) => {
                self.push_log(line);
                None
            }
            Event::MemberDelay { name, result } => {
                self.testing = false;
                match &result {
                    Ok(delay) => {
                        self.set_toast(ToastKind::Success, format!("{name}: {delay} ms"));
                    }
                    Err(err) => {
                        self.set_toast(ToastKind::Error, format!("{name}: {err}"));
                    }
                }
                self.apply_member_delay(&name, &result);
                None
            }
            Event::TestProgress {
                generation,
                name,
                result,
            } => {
                if generation != self.testing_gen || !self.testing {
                    return None;
                }
                self.testing_done = self.testing_done.saturating_add(1).min(self.testing_total);
                if result.is_ok() {
                    self.testing_ok = self.testing_ok.saturating_add(1);
                }
                self.apply_member_delay(&name, &result);
                None
            }
            Event::TestsFinished {
                generation,
                ok,
                total,
            } => {
                if generation != self.testing_gen {
                    return None;
                }
                let was_testing = self.testing;
                self.testing = false;
                if was_testing && total > 0 {
                    self.set_toast(ToastKind::Info, format!("tested {ok}/{total} OK"));
                }
                None
            }
            Event::Toast(kind, text) => {
                self.set_toast(kind, text);
                None
            }
            Event::Fatal(err) => {
                self.offline = Some(err);
                None
            }
            Event::DiffProposal {
                profile_id,
                diffs,
                policy,
            } => {
                // Unknown block keys and unknown labels are skipped rather
                // than guessed: a stale policy must not silently pick a side.
                let choices = policy
                    .iter()
                    .filter_map(|(key, label)| {
                        Some((Block::from_key(key)?, Source::from_label(label)?))
                    })
                    .collect();
                self.pending = Some(PendingSwitch {
                    profile_id,
                    diffs,
                    choices,
                    cursor: 0,
                    remember: false,
                });
                None
            }
            Event::ProfilesUpdated(profiles) => {
                self.profiles = profiles;
                self.sub_idx = self.sub_idx.min(self.profiles.len().saturating_sub(1));
                None
            }
            Event::RulesUpdated { prepend, append } => {
                self.prepend_rules = prepend;
                self.append_rules = append;
                None
            }
            Event::ExternalChange { path } => {
                self.external_dirty = Some(path);
                None
            }
            Event::ReloadResult {
                ok,
                rolled_back,
                error,
            } => {
                let (kind, text) = if ok {
                    (ToastKind::Success, "switched".to_string())
                } else if rolled_back {
                    let detail = error.map(|e| format!(": {e}")).unwrap_or_default();
                    (
                        ToastKind::Error,
                        format!("switch failed, rolled back{detail}"),
                    )
                } else {
                    (
                        ToastKind::Error,
                        error.unwrap_or_else(|| "switch failed".into()),
                    )
                };
                self.set_toast(kind, text);
                None
            }
        }
    }
}

/// Render a byte count with a binary unit.
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

/// A transfer rate, for the live traffic readout.
pub fn format_rate(n: u64) -> String {
    format!("{}/s", format_bytes(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Member;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn group(name: &str, members: &[&str], test_url: Option<&str>) -> Group {
        Group {
            name: name.to_string(),
            kind: "Selector".to_string(),
            now: members.first().copied().unwrap_or_default().to_string(),
            test_url: test_url.map(str::to_string),
            members: members
                .iter()
                .map(|m| Member {
                    name: m.to_string(),
                    kind: "Shadowsocks".to_string(),
                    delay: None,
                })
                .collect(),
        }
    }

    fn app_with_groups() -> App {
        let mut app = App::new("http://127.0.0.1:9090".into(), "http://x/204".into(), 5000);
        app.groups = vec![
            group("GLOBAL", &["DIRECT", "auto"], None),
            group("auto", &["node-a", "node-b"], Some("http://gstatic/204")),
        ];
        app
    }

    #[test]
    fn q_and_ctrl_c_quit_but_other_keys_do_not() {
        let mut app = app_with_groups();
        app.on_key(key(KeyCode::Char('j')));
        assert!(!app.should_quit);

        app.on_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);

        let mut app = app_with_groups();
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn number_keys_jump_to_tabs_and_tab_cycles_wrapping() {
        let mut app = app_with_groups();
        app.on_key(key(KeyCode::Char('3')));
        assert_eq!(app.tab, Tab::Connections);
        app.on_key(key(KeyCode::Char('5')));
        assert_eq!(app.tab, Tab::Subscriptions);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Overview, "wraps around");
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::Subscriptions, "wraps backwards");
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::Logs);
    }

    #[test]
    fn group_cursor_is_clamped_at_the_ends() {
        let mut app = app_with_groups();
        app.tab = Tab::Proxies;

        app.on_key(key(KeyCode::Up));
        assert_eq!(app.group_idx, 0, "cannot scroll above the first group");

        for _ in 0..10 {
            app.on_key(key(KeyCode::Down));
        }
        assert_eq!(app.group_idx, 1, "cannot scroll past the last group");
    }

    #[test]
    fn changing_group_resets_the_member_cursor() {
        let mut app = app_with_groups();
        app.tab = Tab::Proxies;
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.member_idx, 1);

        app.on_key(key(KeyCode::Down));
        assert_eq!(app.group_idx, 1);
        assert_eq!(
            app.member_idx, 0,
            "member index belongs to the previous group"
        );
    }

    #[test]
    fn member_cursor_tracks_the_selected_group_length() {
        let mut app = app_with_groups();
        app.tab = Tab::Proxies;
        app.on_key(key(KeyCode::Down)); // group "auto", 2 members
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.member_idx, 1, "clamped to the 2-member group");

        app.on_key(key(KeyCode::Up)); // group "GLOBAL", also 2 members
        assert_eq!(app.member_idx, 0);
    }

    #[test]
    fn enter_emits_a_select_command_for_the_highlighted_member() {
        let mut app = app_with_groups();
        app.tab = Tab::Proxies;
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Right));

        let command = app.on_key(key(KeyCode::Enter));
        assert_eq!(
            command,
            Some(Command::Select {
                group: "auto".into(),
                member: "node-b".into()
            })
        );
    }

    #[test]
    fn t_tests_one_member_and_shift_t_tests_the_group() {
        let mut app = app_with_groups();
        app.tab = Tab::Proxies;
        app.on_key(key(KeyCode::Down));

        let one = app.on_key(key(KeyCode::Char('t')));
        assert_eq!(
            one,
            Some(Command::TestMember {
                name: "node-a".into(),
                url: "http://gstatic/204".into(),
                timeout_ms: 5000,
            }),
            "uses the group's own test url"
        );

        let all = app.on_key(key(KeyCode::Char('T')));
        assert_eq!(
            all,
            Some(Command::TestMany {
                generation: 1,
                names: vec!["node-a".into(), "node-b".into()],
                url: "http://gstatic/204".into(),
                timeout_ms: 5000,
            })
        );
        assert!(app.testing, "flag is raised so the UI can show progress");
    }

    #[test]
    fn a_group_without_a_test_url_falls_back_to_the_configured_one() {
        let mut app = app_with_groups();
        app.tab = Tab::Proxies; // GLOBAL has no test url
        let command = app.on_key(key(KeyCode::Char('t')));
        assert_eq!(
            command,
            Some(Command::TestMember {
                name: "DIRECT".into(),
                url: "http://x/204".into(),
                timeout_ms: 5000,
            })
        );
    }

    #[test]
    fn mode_keys_cycle_through_all_three_modes() {
        let mut app = app_with_groups();
        app.configs = Some(Configs {
            mode: "rule".into(),
            ..Default::default()
        });

        assert_eq!(
            app.on_key(key(KeyCode::Char('m'))),
            Some(Command::SetMode("global".into()))
        );
        assert_eq!(
            app.on_key(key(KeyCode::Char('m'))),
            Some(Command::SetMode("direct".into()))
        );
        assert_eq!(
            app.on_key(key(KeyCode::Char('m'))),
            Some(Command::SetMode("rule".into()))
        );
        assert_eq!(
            app.on_key(key(KeyCode::Char('M'))),
            Some(Command::SetMode("direct".into()))
        );
    }

    #[test]
    fn overview_keys_toggle_tun_and_lan() {
        let mut app = app_with_groups();
        app.tab = Tab::Overview;
        app.configs = Some(Configs {
            allow_lan: false,
            tun: crate::api::Tun { enable: false },
            ..Default::default()
        });

        assert_eq!(
            app.on_key(key(KeyCode::Char('l'))),
            Some(Command::SetAllowLan(true))
        );
        assert!(app.configs.as_ref().unwrap().allow_lan);

        assert_eq!(
            app.on_key(key(KeyCode::Char('l'))),
            Some(Command::SetAllowLan(false))
        );
        assert!(!app.configs.as_ref().unwrap().allow_lan);

        assert_eq!(
            app.on_key(key(KeyCode::Char('u'))),
            Some(Command::SetTun(true))
        );
        assert!(app.configs.as_ref().unwrap().tun.enable);

        assert_eq!(
            app.on_key(key(KeyCode::Char('u'))),
            Some(Command::SetTun(false))
        );
        assert!(!app.configs.as_ref().unwrap().tun.enable);
    }

    #[test]
    fn key_handling_survives_an_empty_group_list() {
        let mut app = App::new("http://127.0.0.1:9090".into(), "http://x/204".into(), 5000);
        app.tab = Tab::Proxies;
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Enter,
            KeyCode::Char('t'),
            KeyCode::Char('T'),
        ] {
            assert_eq!(
                app.on_key(key(code)),
                None,
                "no group means no command, and no panic"
            );
        }
    }

    #[test]
    fn connection_cursor_clamps_and_actions_need_a_selection() {
        let mut app = app_with_groups();
        app.tab = Tab::Connections;
        app.connections = serde_json::from_str(
            r#"{"connections":[{"id":"a","metadata":{}},{"id":"b","metadata":{}}]}"#,
        )
        .unwrap();

        assert_eq!(
            app.on_key(key(KeyCode::Char('x'))),
            None,
            "nothing highlighted yet"
        );

        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.conn_idx, Some(1), "clamped to the last connection");
        assert_eq!(
            app.on_key(key(KeyCode::Char('x'))),
            Some(Command::CloseConnection("b".into()))
        );

        assert_eq!(
            app.on_key(key(KeyCode::Char('X'))),
            Some(Command::CloseAllConnections)
        );
    }

    #[test]
    fn a_snapshot_preserves_the_cursor_when_the_group_list_shrinks() {
        let mut app = app_with_groups();
        app.tab = Tab::Proxies;
        app.group_idx = 1;
        app.member_idx = 1;

        app.apply_snapshot(Snapshot {
            groups: vec![group("GLOBAL", &["DIRECT"], None)],
            ..Default::default()
        });

        assert_eq!(app.group_idx, 0, "group index clamped to the new length");
        assert_eq!(app.member_idx, 0, "member index clamped to the new group");
    }

    #[test]
    fn snapshots_clear_the_offline_flag() {
        let mut app = app_with_groups();
        app.offline = Some("connection refused".into());
        app.apply_snapshot(Snapshot::default());
        assert!(app.offline.is_none());
    }

    #[test]
    fn progress_events_update_members_and_retire_the_testing_flag() {
        let mut app = app_with_groups();
        app.testing = true;
        app.testing_gen = 1;
        app.testing_total = 2;
        app.tab = Tab::Proxies;

        app.on_event(Event::TestProgress {
            generation: 1,
            name: "node-a".into(),
            result: Ok(42),
        });
        app.on_event(Event::TestProgress {
            generation: 1,
            name: "node-b".into(),
            result: Ok(0),
        });

        assert_eq!(app.groups[1].members[0].delay, Some(42));
        assert_eq!(
            app.groups[1].members[1].delay, None,
            "zero delay is not a real measurement"
        );
        assert_eq!(app.testing_done, 2);

        app.on_event(Event::TestsFinished {
            generation: 1,
            ok: 1,
            total: 2,
        });
        assert!(!app.testing);
        assert!(app.active_toast(Instant::now()).is_some());
    }

    #[test]
    fn a_failed_member_test_clears_its_delay_and_toasts() {
        let mut app = app_with_groups();
        app.groups[1].members[0].delay = Some(99);

        app.on_event(Event::MemberDelay {
            name: "node-a".into(),
            result: Err("timeout".into()),
        });

        assert_eq!(app.groups[1].members[0].delay, None);
        let toast = app.active_toast(Instant::now()).unwrap();
        assert_eq!(toast.kind, ToastKind::Error);
        assert!(toast.text.contains("timeout"));
    }

    #[test]
    fn toasts_expire() {
        let mut app = app_with_groups();
        app.set_toast(ToastKind::Info, "mode set to global");
        assert!(app.active_toast(Instant::now()).is_some());
        assert!(
            app.active_toast(Instant::now() + TOAST_TTL).is_none(),
            "the toast is gone once its TTL has passed"
        );
    }

    #[test]
    fn traffic_history_is_capped_and_keeps_the_newest_sample() {
        let mut app = app_with_groups();
        for i in 1..=(TRAFFIC_HISTORY as u64 + 50) {
            app.on_event(Event::Traffic { up: i, down: i * 2 });
        }

        assert_eq!(app.traffic.up_history.len(), TRAFFIC_HISTORY);
        assert_eq!(
            app.traffic.up_history.back(),
            Some(&(TRAFFIC_HISTORY as u64 + 50))
        );
        assert_eq!(app.traffic.up, TRAFFIC_HISTORY as u64 + 50);
        assert_eq!(app.traffic.down, (TRAFFIC_HISTORY as u64 + 50) * 2);
    }

    #[test]
    fn log_buffer_drops_the_oldest_lines() {
        let mut app = app_with_groups();
        for i in 0..(LOG_CAPACITY + 20) {
            app.push_log(LogLine {
                level: "info".into(),
                payload: format!("line {i}"),
            });
        }

        assert_eq!(app.log.len(), LOG_CAPACITY);
        assert_eq!(app.log.front().unwrap().payload, format!("line {}", 20));
    }

    #[test]
    fn format_bytes_uses_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1536), "1.50 KiB");
        assert_eq!(format_bytes(1024 * 1024 * 3), "3.00 MiB");
        assert_eq!(
            format_bytes(u64::MAX),
            "16777216.00 TiB",
            "scaling stops at the largest unit rather than running off the end"
        );
    }

    #[test]
    fn format_rate_appends_per_second() {
        assert_eq!(format_rate(2048), "2.00 KiB/s");
    }

    // ----- subscription management: profiles, confirm page, input form -----

    fn profile(id: &str) -> ProfileMeta {
        ProfileMeta {
            id: id.to_string(),
            name: format!("sub {id}"),
            url: format!("https://example/{id}"),
            created_ms: 0,
            updated_ms: 0,
            nodes: 3,
            protocols: "ss(3)".into(),
            policy: BTreeMap::new(),
        }
    }

    fn app_with_profiles(ids: &[&str]) -> App {
        let mut app = app_with_groups();
        app.tab = Tab::Subscriptions;
        app.profiles = ids.iter().map(|id| profile(id)).collect();
        app
    }

    fn diff(block: Block) -> BlockDiff {
        BlockDiff {
            block,
            base: Some("base value".into()),
            sub: Some("sub value".into()),
            equal: false,
        }
    }

    /// Open the confirm page as the worker would: one proposal for `s1`
    /// with two differing blocks.
    fn open_pending(app: &mut App, policy: BTreeMap<String, String>) {
        app.on_event(Event::DiffProposal {
            profile_id: "s1".into(),
            diffs: vec![diff(Block::Rules), diff(Block::Dns)],
            policy,
        });
    }

    #[test]
    fn diff_proposal_opens_pending_with_policy_prefilled() {
        let mut app = app_with_groups();
        let mut policy = BTreeMap::new();
        policy.insert("rules".to_string(), "subscription".to_string());
        policy.insert("ghost".to_string(), "subscription".to_string());
        policy.insert("tun".to_string(), "neither".to_string());

        open_pending(&mut app, policy);

        let pending = app.pending.as_ref().unwrap();
        assert_eq!(pending.profile_id, "s1");
        assert_eq!(pending.cursor, 0);
        assert!(!pending.remember, "remember starts off");
        assert_eq!(pending.diffs.len(), 2);
        assert_eq!(
            pending.choices.get(&Block::Rules),
            Some(&Source::Subscription),
            "the saved policy prefills the choice"
        );
        assert_eq!(
            pending.choices.len(),
            1,
            "unknown block keys and labels are skipped, not guessed"
        );
    }

    #[test]
    fn confirm_page_keys_work() {
        let mut app = app_with_groups();
        let mut policy = BTreeMap::new();
        policy.insert("rules".to_string(), "subscription".to_string());
        open_pending(&mut app, policy); // cursor 0 → Rules

        // b / s / Left / Right flip the highlighted block's source.
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(
            app.pending.as_ref().unwrap().choices.get(&Block::Rules),
            Some(&Source::Base)
        );
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(
            app.pending.as_ref().unwrap().choices.get(&Block::Rules),
            Some(&Source::Subscription)
        );
        app.on_key(key(KeyCode::Left));
        assert_eq!(
            app.pending.as_ref().unwrap().choices.get(&Block::Rules),
            Some(&Source::Base)
        );
        app.on_key(key(KeyCode::Right));
        assert_eq!(
            app.pending.as_ref().unwrap().choices.get(&Block::Rules),
            Some(&Source::Subscription)
        );

        // Up / Down (and j/k) move the cursor, clamped at both ends.
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.pending.as_ref().unwrap().cursor, 1);
        app.on_key(key(KeyCode::Down));
        assert_eq!(
            app.pending.as_ref().unwrap().cursor,
            1,
            "clamped at the last row"
        );
        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(app.pending.as_ref().unwrap().cursor, 0);
        app.on_key(key(KeyCode::Up));
        assert_eq!(
            app.pending.as_ref().unwrap().cursor,
            0,
            "clamped at the first row"
        );

        // r toggles remember.
        app.on_key(key(KeyCode::Char('r')));
        assert!(app.pending.as_ref().unwrap().remember);
        app.on_key(key(KeyCode::Char('r')));
        assert!(!app.pending.as_ref().unwrap().remember);

        // Enter confirms with the live choices and closes the page.
        app.on_key(key(KeyCode::Char('r'))); // remember back on
        let mut choices = BTreeMap::new();
        choices.insert(Block::Rules, Source::Subscription);
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Some(Command::ConfirmSwitch {
                profile_id: "s1".into(),
                choices,
                remember: true,
            })
        );
        assert!(app.pending.is_none(), "confirming closes the page");

        // Esc cancels without emitting anything.
        open_pending(&mut app, BTreeMap::new());
        assert_eq!(app.on_key(key(KeyCode::Esc)), None);
        assert!(app.pending.is_none());
    }

    #[test]
    fn bracket_keys_cycle_profiles() {
        // Shuffled on purpose: s3 sits at index 0, so both keys have to wrap
        // around the current id rather than step along the list.
        let mut app = app_with_profiles(&["s3", "s1", "s2"]);
        assert_eq!(
            app.on_key(key(KeyCode::Char('['))),
            Some(Command::SwitchProfile("s2".into())),
            "[ wraps backwards past the first entry"
        );
        assert_eq!(
            app.on_key(key(KeyCode::Char(']'))),
            Some(Command::SwitchProfile("s1".into())),
            "] steps forward from the current id"
        );

        let mut app = app_with_groups();
        assert_eq!(
            app.on_key(key(KeyCode::Char('['))),
            None,
            "no profiles, no cycle"
        );
        assert_eq!(app.on_key(key(KeyCode::Char(']'))), None);
    }

    #[test]
    fn input_overlay_edits_two_fields() {
        let mut app = app_with_profiles(&["s1"]);
        assert_eq!(
            app.on_key(key(KeyCode::Char('a'))),
            None,
            "opening the form is pure UI"
        );
        let overlay = app.input.as_ref().unwrap();
        assert_eq!(overlay.field, InputField::Url);
        assert!(overlay.url.is_empty());

        for c in "https://x/sub".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        let overlay = app.input.as_ref().unwrap();
        assert_eq!(overlay.url, "https://x/sub");

        // Tab moves to the name field; Backspace edits it.
        app.on_key(key(KeyCode::Tab));
        for c in "my sud".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.input.as_ref().unwrap().name, "my sub");

        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Some(Command::AddProfile {
                name: "my sub".into(),
                url: "https://x/sub".into(),
            })
        );
        assert!(app.input.is_none(), "submitting closes the form");

        // No url, no command — and the form stays open for corrections.
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(app.on_key(key(KeyCode::Enter)), None);
        assert!(app.input.is_some());
        assert_eq!(app.on_key(key(KeyCode::Esc)), None);
        assert!(app.input.is_none(), "esc discards the form");
    }

    #[test]
    fn settings_overlay_edits_fields_and_applies() {
        let mut app = app_with_groups();
        app.configs = Some(Configs {
            mixed_port: 7890,
            allow_lan: false,
            tun: crate::api::Tun { enable: false },
            mode: "rule".into(),
            ..Default::default()
        });

        // 'o' opens the settings overlay.
        app.on_key(key(KeyCode::Char('o')));
        assert!(app.settings.is_some());
        let s = app.settings.as_ref().unwrap();
        assert_eq!(s.cursor, 0);
        assert_eq!(s.mode, "rule");
        assert_eq!(s.mixed_port, "7890");
        assert!(!s.allow_lan);
        assert!(!s.tun);

        // Toggle mode with Space.
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(app.settings.as_ref().unwrap().mode, "global");

        // Move to mixed-port.
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.settings.as_ref().unwrap().cursor, 1);
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Char('1')));
        assert_eq!(app.settings.as_ref().unwrap().mixed_port, "7891");

        // Move to allow-lan and toggle with Space.
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.settings.as_ref().unwrap().cursor, 2);
        app.on_key(key(KeyCode::Char(' ')));
        assert!(app.settings.as_ref().unwrap().allow_lan);

        // Move to tun and toggle with Space.
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.settings.as_ref().unwrap().cursor, 3);
        app.on_key(key(KeyCode::Char(' ')));
        assert!(app.settings.as_ref().unwrap().tun);

        // Enter applies.
        let cmd = app.on_key(key(KeyCode::Enter));
        assert_eq!(
            cmd,
            Some(Command::ApplySettings {
                mode: "global".into(),
                mixed_port: 7891,
                allow_lan: true,
                tun: true,
            })
        );
        assert!(app.settings.is_none(), "Enter closes settings overlay");

        // Verify local configs reflect the changes.
        let cfg = app.configs.as_ref().unwrap();
        assert_eq!(cfg.mixed_port, 7891);
        assert!(cfg.allow_lan);
        assert!(cfg.tun.enable);
        assert_eq!(cfg.mode, "global");
    }

    #[test]
    fn overlays_shield_global_keys_like_q() {
        // Typing a url that contains `q` must not quit the app.
        let mut app = app_with_profiles(&["s1"]);
        app.on_key(key(KeyCode::Char('a')));
        app.on_key(key(KeyCode::Char('q')));
        assert!(!app.should_quit);
        assert_eq!(app.input.as_ref().unwrap().url, "q");

        // Tab while the form is open switches fields, not tabs.
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.input.as_ref().unwrap().field, InputField::Name);
        assert_eq!(app.tab, Tab::Subscriptions, "the form swallows the tab key");

        app.on_key(key(KeyCode::Esc));

        // The confirm page shields the same keys.
        open_pending(&mut app, BTreeMap::new());
        app.on_key(key(KeyCode::Char('q')));
        assert!(!app.should_quit);
        assert!(app.pending.is_some(), "q is not an escape hatch here");
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Subscriptions);
        assert_eq!(app.on_key(key(KeyCode::Esc)), None);
        assert!(app.pending.is_none());
    }

    #[test]
    fn external_dirty_flag_and_v_key() {
        let mut app = app_with_profiles(&["s1"]);
        app.on_event(Event::ExternalChange {
            path: "/cfg/base.yaml".into(),
        });
        assert_eq!(app.external_dirty.as_deref(), Some("/cfg/base.yaml"));

        assert_eq!(
            app.on_key(key(KeyCode::Char('v'))),
            Some(Command::SwitchProfile("s1".into()))
        );
        assert!(app.external_dirty.is_none(), "re-applying clears the flag");

        // Without a profile `v` is a full no-op — the flag stays put.
        let mut app = app_with_groups();
        app.on_event(Event::ExternalChange { path: "p".into() });
        assert_eq!(app.on_key(key(KeyCode::Char('v'))), None);
        assert!(app.external_dirty.is_some());
    }

    #[test]
    fn reload_result_becomes_a_toast() {
        let mut app = app_with_groups();

        app.on_event(Event::ReloadResult {
            ok: true,
            rolled_back: false,
            error: None,
        });
        let toast = app.active_toast(Instant::now()).unwrap();
        assert_eq!(toast.kind, ToastKind::Success);
        assert!(toast.text.contains("switched"));

        app.on_event(Event::ReloadResult {
            ok: false,
            rolled_back: true,
            error: Some("503".into()),
        });
        let toast = app.active_toast(Instant::now()).unwrap();
        assert_eq!(toast.kind, ToastKind::Error);
        assert!(toast.text.contains("rolled back"), "`{}`", toast.text);

        app.on_event(Event::ReloadResult {
            ok: false,
            rolled_back: false,
            error: Some("dns broke".into()),
        });
        let toast = app.active_toast(Instant::now()).unwrap();
        assert_eq!(toast.kind, ToastKind::Error);
        assert!(
            toast.text.contains("dns broke"),
            "the reason must surface: `{}`",
            toast.text
        );
    }

    #[test]
    fn delete_needs_a_second_d_and_esc_disarms() {
        let mut app = app_with_profiles(&["s1", "s2"]);

        assert_eq!(
            app.on_key(key(KeyCode::Char('d'))),
            None,
            "the first d only arms"
        );
        assert_eq!(app.delete_armed.as_deref(), Some("s1"));

        assert_eq!(app.on_key(key(KeyCode::Esc)), None);
        assert!(app.delete_armed.is_none(), "esc disarms");

        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(
            app.on_key(key(KeyCode::Char('d'))),
            Some(Command::DeleteProfile("s1".into()))
        );
        assert!(app.delete_armed.is_none(), "the delete consumed the arm");

        // Switching tabs disarms too.
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Tab));
        assert!(app.delete_armed.is_none());

        // Nothing highlighted: d does nothing at all.
        let mut app = app_with_groups();
        app.tab = Tab::Subscriptions;
        assert_eq!(app.on_key(key(KeyCode::Char('d'))), None);
    }

    #[test]
    fn profiles_updated_replaces_and_clamps() {
        let mut app = app_with_profiles(&["s1", "s2", "s3"]);
        app.sub_idx = 2;

        app.on_event(Event::ProfilesUpdated(vec![profile("s1"), profile("s2")]));
        assert_eq!(app.profiles.len(), 2);
        assert_eq!(app.sub_idx, 1, "clamped to the new length");

        app.on_event(Event::ProfilesUpdated(Vec::new()));
        assert!(app.profiles.is_empty());
        assert_eq!(app.sub_idx, 0);
    }

    #[test]
    fn subscriptions_tab_keys_target_the_current_profile() {
        let mut app = app_with_profiles(&["s1", "s2"]);

        app.on_key(key(KeyCode::Down));
        assert_eq!(app.sub_idx, 1);
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.sub_idx, 1, "clamped at the last profile");

        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Some(Command::SwitchProfile("s2".into()))
        );
        assert_eq!(
            app.on_key(key(KeyCode::Char('u'))),
            Some(Command::UpdateProfile("s2".into()))
        );

        // Without profiles the keys are inert but never panic.
        let mut app = app_with_groups();
        app.tab = Tab::Subscriptions;
        assert_eq!(app.on_key(key(KeyCode::Enter)), None);
        assert_eq!(app.on_key(key(KeyCode::Char('u'))), None);
        app.on_key(key(KeyCode::Down));
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn tab_range(tab: Tab) -> std::ops::Range<u16> {
        crate::ui::tab_hitboxes(100)
            .into_iter()
            .find(|(_, candidate)| *candidate == tab)
            .map(|(range, _)| range)
            .expect("every tab has a hitbox")
    }

    #[test]
    fn clicking_a_tab_switches_pages_using_the_rendered_geometry() {
        let mut app = app_with_groups();
        app.tab_width.set(100);

        let subscriptions = tab_range(Tab::Subscriptions);
        app.on_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            subscriptions.start + 1,
            0,
        ));
        assert_eq!(app.tab, Tab::Subscriptions);

        let overview = tab_range(Tab::Overview);
        app.on_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            overview.start + 1,
            0,
        ));
        assert_eq!(app.tab, Tab::Overview);

        // The right side of row 0 is the status line, not a tab.
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 99, 0));
        assert_eq!(app.tab, Tab::Overview);
    }

    #[test]
    fn clicks_select_subscription_rows_through_the_scroll_offset() {
        let mut app = app_with_profiles(&["s1", "s2", "s3"]);
        app.sub_idx = 0;
        app.subs_area.set(Rect::new(0, 1, 60, 5));
        app.subs_top.set(1);

        // Area row 0 is the pinned header; row 2 is the second data row,
        // which is profile index 1 + 1 = 2.
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 3));
        assert_eq!(app.sub_idx, 2);

        // The header and the space below the table are inert.
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 1));
        assert_eq!(app.sub_idx, 2);
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 20));
        assert_eq!(app.sub_idx, 2);
    }

    #[test]
    fn clicks_select_proxy_members() {
        let mut app = app_with_groups();
        app.tab = Tab::Proxies;
        app.group_idx = 1;
        app.member_idx = 0;
        app.members_area.set(Rect::new(50, 2, 40, 3));
        app.members_top.set(0);

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 60, 3));
        assert_eq!(app.member_idx, 1);

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 60, 2));
        assert_eq!(app.member_idx, 0);

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 60, 8));
        assert_eq!(app.member_idx, 0, "outside the table is ignored");
    }

    #[test]
    fn only_left_button_presses_are_actionable() {
        let mut app = app_with_groups();
        app.tab_width.set(100);
        let subscriptions = tab_range(Tab::Subscriptions).start + 1;

        app.on_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            subscriptions,
            0,
        ));
        assert_eq!(app.tab, Tab::Overview);
        app.on_mouse(mouse(
            MouseEventKind::Down(MouseButton::Right),
            subscriptions,
            0,
        ));
        assert_eq!(app.tab, Tab::Overview);
        app.on_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            subscriptions,
            0,
        ));
        assert_eq!(app.tab, Tab::Overview);
    }
    #[test]
    fn a_key_starts_test_many_with_deduped_names_and_generation() {
        let mut app = App::new("http://127.0.0.1:9090".into(), "http://x/204".into(), 5000);
        app.groups = vec![
            group("GLOBAL", &["shared", "auto"], None),
            group("auto", &["node-a", "shared"], Some("http://gstatic/204")),
        ];
        app.tab = Tab::Proxies;
        app.group_idx = 1;

        let command = app.on_key(key(KeyCode::Char('A')));
        assert_eq!(
            command,
            Some(Command::TestMany {
                generation: 1,
                names: vec!["shared".into(), "auto".into(), "node-a".into()],
                url: "http://gstatic/204".into(),
                timeout_ms: 5000,
            })
        );
        assert!(app.testing);
        assert_eq!(app.testing_total, 3);
    }

    #[test]
    fn progress_events_update_counters_and_stale_gen_is_ignored() {
        let mut app = app_with_groups();
        app.testing = true;
        app.testing_gen = 1;
        app.testing_total = 2;
        app.testing_done = 0;
        app.testing_ok = 0;
        app.tab = Tab::Proxies;

        app.on_event(Event::TestProgress {
            generation: 1,
            name: "node-a".into(),
            result: Ok(42),
        });
        assert_eq!(app.testing_done, 1);
        assert_eq!(app.testing_ok, 1);
        assert_eq!(app.groups[1].members[0].delay, Some(42));

        app.on_event(Event::TestProgress {
            generation: 0,
            name: "node-b".into(),
            result: Ok(7),
        });
        assert_eq!(app.testing_done, 1);
        assert_eq!(app.groups[1].members[1].delay, None);

        app.on_event(Event::TestsFinished {
            generation: 1,
            ok: 1,
            total: 1,
        });
        assert!(!app.testing);
    }

    #[test]
    fn esc_cancels_and_s_toggles_sort() {
        let mut app = app_with_groups();
        app.tab = Tab::Proxies;
        app.testing = true;
        app.testing_gen = 3;

        assert_eq!(app.on_key(key(KeyCode::Esc)), Some(Command::CancelTests(3)));
        assert!(!app.testing);

        assert_eq!(app.on_key(key(KeyCode::Char('s'))), None);
        assert!(app.sort_by_delay);
        assert_eq!(app.on_key(key(KeyCode::Char('s'))), None);
        assert!(!app.sort_by_delay);
    }

    #[test]
    fn rules_overlay_navigation_add_and_delete() {
        let mut app = app_with_groups();
        app.tab = Tab::Overview;

        // 1. Shift+R opens rules overlay
        app.on_key(key(KeyCode::Char('R')));
        assert!(app.rules.is_some());
        let rules = app.rules.as_ref().unwrap();
        assert!(rules.is_prepend);
        assert_eq!(rules.cursor, 0);
        assert!(!rules.adding);

        // 2. Tab toggles to append rules
        app.on_key(key(KeyCode::Tab));
        assert!(!app.rules.as_ref().unwrap().is_prepend);

        // 3. Tab toggles back to prepend rules
        app.on_key(key(KeyCode::Tab));
        assert!(app.rules.as_ref().unwrap().is_prepend);

        // 4. 'a' enters adding mode
        app.on_key(key(KeyCode::Char('a')));
        assert!(app.rules.as_ref().unwrap().adding);

        // 5. Type rule "MATCH,DIRECT" and Enter
        for c in "MATCH,DIRECT".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        let save_cmd = app.on_key(key(KeyCode::Enter));
        assert_eq!(
            save_cmd,
            Some(Command::SaveRules {
                prepend: vec!["MATCH,DIRECT".to_string()],
                append: vec![],
            })
        );
        assert_eq!(app.prepend_rules, vec!["MATCH,DIRECT".to_string()]);
        assert!(!app.rules.as_ref().unwrap().adding);

        // 6. 'd' deletes the rule
        let delete_cmd = app.on_key(key(KeyCode::Char('d')));
        assert_eq!(
            delete_cmd,
            Some(Command::SaveRules {
                prepend: vec![],
                append: vec![],
            })
        );
        assert!(app.prepend_rules.is_empty());

        // 7. Esc closes the overlay
        app.on_key(key(KeyCode::Esc));
        assert!(app.rules.is_none());
    }

    #[test]
    fn settings_overlay_opens_rules_on_item_4() {
        let mut app = app_with_groups();
        app.tab = Tab::Overview;

        // 'o' opens settings
        app.on_key(key(KeyCode::Char('o')));
        assert!(app.settings.is_some());

        // Navigate down to item 4 (Custom Rules)
        for _ in 0..4 {
            app.on_key(key(KeyCode::Down));
        }
        assert_eq!(app.settings.as_ref().unwrap().cursor, 4);

        // Enter opens rules overlay and closes settings
        app.on_key(key(KeyCode::Enter));
        assert!(app.settings.is_none());
        assert!(app.rules.is_some());
    }
}
