//! Application state and key handling.
//!
//! `App` owns everything the UI draws and decides, for each keypress, which
//! [`Command`] the caller should run against the API. Keeping the network out
//! of this module keeps the whole thing testable without a live mihomo.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;

use crate::api::{Configs, Connection, Connections, Group, LogLine, Version};

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
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Overview, Tab::Proxies, Tab::Connections, Tab::Logs];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Proxies => "Proxies",
            Tab::Connections => "Connections",
            Tab::Logs => "Logs",
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
    Select {
        group: String,
        member: String,
    },
    TestMember {
        name: String,
        url: String,
        timeout_ms: u64,
    },
    TestGroup {
        name: String,
        url: String,
        timeout_ms: u64,
    },
    CloseConnection(String),
    CloseAllConnections,
    Refresh,
}

/// Results flowing back into the app from the background tasks.
#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
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
    /// A whole group's delay test finished.
    GroupDelay(HashMap<String, u64>),
    Toast(ToastKind, String),
    /// The controller connection failed; stop hammering it.
    Fatal(String),
}

#[derive(Debug, Default)]
pub struct Snapshot {
    pub version: Option<Version>,
    pub configs: Option<Configs>,
    pub groups: Vec<Group>,
    pub connections: Connections,
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
        }
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

    pub fn apply_group_delays(&mut self, delays: &HashMap<String, u64>) {
        for group in &mut self.groups {
            for member in &mut group.members {
                if let Some(delay) = delays.get(&member.name) {
                    // The group endpoint reports 0 for members that failed.
                    member.delay = (*delay > 0).then_some(*delay);
                }
            }
        }
    }

    pub fn apply_member_delay(&mut self, name: &str, result: &Result<u64, String>) {
        let delay = result.as_ref().ok().copied();
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
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Command> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Quitting and tab switching work from anywhere.
        match key.code {
            KeyCode::Char('c') if ctrl => {
                self.should_quit = true;
                return None;
            }
            KeyCode::Char('q') => {
                self.should_quit = true;
                return None;
            }
            KeyCode::Tab => {
                self.tab = self.tab.offset(1);
                return None;
            }
            KeyCode::BackTab => {
                self.tab = self.tab.offset(-1);
                return None;
            }
            KeyCode::Char(c @ '1'..='4') => {
                self.tab = Tab::ALL[c as usize - '1' as usize];
                return None;
            }
            KeyCode::Char('r') => return Some(Command::Refresh),
            _ => {}
        }

        match self.tab {
            Tab::Overview => self.on_key_overview(key),
            Tab::Proxies => self.on_key_proxies(key),
            Tab::Connections => self.on_key_connections(key),
            Tab::Logs => self.on_key_logs(key),
        }
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
                let name = group.name.clone();
                let url = group
                    .test_url
                    .clone()
                    .unwrap_or_else(|| self.delay_url.clone());
                let timeout_ms = self.timeout_ms;
                self.testing = true;
                Some(Command::TestGroup {
                    name,
                    url,
                    timeout_ms,
                })
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

    /// Apply an event, returning a command when the event implies follow-up work.
    pub fn on_event(&mut self, event: Event) -> Option<Command> {
        match event {
            Event::Key(key) => self.on_key(key),
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
            Event::GroupDelay(delays) => {
                let ok = delays.values().filter(|d| **d > 0).count();
                let total = delays.len();
                self.apply_group_delays(&delays);
                self.testing = false;
                self.set_toast(ToastKind::Info, format!("tested {ok}/{total} OK"));
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

/// Traffic history as u64 for the sparkline widget.
pub fn sparkline_data(history: &VecDeque<u64>) -> Vec<u64> {
    history.iter().copied().collect()
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
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Logs);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Overview, "wraps around");
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::Logs, "wraps backwards");
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
            Some(Command::TestGroup {
                name: "auto".into(),
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
    fn group_delay_results_update_members_and_retire_the_testing_flag() {
        let mut app = app_with_groups();
        app.testing = true;
        app.tab = Tab::Proxies;

        let mut delays = HashMap::new();
        delays.insert("node-a".to_string(), 42);
        delays.insert("node-b".to_string(), 0); // 0 means the test failed

        app.on_event(Event::GroupDelay(delays));

        assert_eq!(app.groups[1].members[0].delay, Some(42));
        assert_eq!(
            app.groups[1].members[1].delay, None,
            "zero delay is not a real measurement"
        );
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
}
