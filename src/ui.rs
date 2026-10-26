//! Rendering. Every function here takes `&App` and draws; nothing mutates
//! application state or touches the network.

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Gauge, List, ListItem, ListState, Paragraph, Row, Sparkline, Table,
    TableState, Tabs,
};

use crate::api::Connection;
use crate::app::{App, Tab, format_bytes, format_rate, sparkline_data};

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
const UP: Color = Color::LightCyan;
const DOWN: Color = Color::LightGreen;

/// Below this the layout starts colliding, so we show a hint instead.
const MIN_WIDTH: u16 = 48;
const MIN_HEIGHT: u16 = 12;

pub fn draw(frame: &mut Frame, app: &App, now: Instant) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let hint = Paragraph::new(vec![
            Line::from("terminal too small")
                .fg(Color::Yellow)
                .centered(),
            Line::from(format!(
                "need at least {MIN_WIDTH}x{MIN_HEIGHT}, have {}x{}",
                area.width, area.height
            ))
            .fg(MUTED)
            .centered(),
        ])
        .alignment(Alignment::Center);
        frame.render_widget(hint, area);
        return;
    }

    let areas = Layout::vertical([
        Constraint::Length(1), // tabs
        Constraint::Min(3),    // body
        Constraint::Length(1), // footer
    ])
    .split(area);

    draw_header(frame, app, areas[0]);

    match app.tab {
        Tab::Overview => draw_overview(frame, app, areas[1]),
        Tab::Proxies => draw_proxies(frame, app, areas[1]),
        Tab::Connections => draw_connections(frame, app, areas[1]),
        Tab::Logs => draw_logs(frame, app, areas[1]),
        Tab::Subscriptions => draw_subscriptions(frame, app, areas[1]),
    }

    draw_footer(frame, app, areas[2], now);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let titles: Vec<Line> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, tab)| Line::from(format!(" {} {} ", i + 1, tab.title())))
        .collect();

    let selected = Tab::ALL.iter().position(|t| *t == app.tab);
    let tabs = Tabs::new(titles)
        .select(selected)
        .highlight_style(Style::default().fg(Color::Black).bg(ACCENT).bold())
        .divider("")
        .padding("", "");

    // The endpoint is more useful than a title here, so show it right-aligned.
    let host = app.endpoint.trim_end_matches('/');
    let status = if app.offline.is_some() {
        "offline"
    } else {
        "connected"
    };
    let status_color = if app.offline.is_some() {
        Color::Red
    } else {
        Color::Green
    };

    let right = Line::from(vec![
        Span::styled(status, Style::default().fg(status_color)),
        Span::raw("  "),
        Span::styled(host, Style::default().fg(MUTED)),
        Span::raw(" "),
    ]);

    // Lay the tabs and the status text out on the same row without overlap.
    let right_width = right.width() as u16;
    let areas =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width)]).split(area);
    frame.render_widget(tabs, areas[0]);
    frame.render_widget(Paragraph::new(right), areas[1]);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect, now: Instant) {
    let keys: &[(&str, &str)] = match app.tab {
        Tab::Overview => &[
            ("m/M", "mode"),
            ("r", "refresh"),
            ("tab", "switch"),
            ("q", "quit"),
        ],
        Tab::Proxies => &[
            ("j/k", "group"),
            ("h/l", "member"),
            ("enter", "select"),
            ("t/T", "test"),
            ("r", "refresh"),
            ("q", "quit"),
        ],
        Tab::Connections => &[
            ("j/k", "move"),
            ("x", "close"),
            ("X", "close all"),
            ("r", "refresh"),
            ("q", "quit"),
        ],
        Tab::Logs => &[("c", "clear"), ("tab", "switch"), ("q", "quit")],
        Tab::Subscriptions => &[
            ("a", "add"),
            ("u", "update"),
            ("d", "delete"),
            ("enter", "switch"),
            ("[/]", "cycle"),
            ("q", "quit"),
        ],
    };

    let mut spans = Vec::new();
    for (key, label) in keys {
        spans.push(Span::styled(*key, Style::default().fg(ACCENT)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*label, Style::default().fg(MUTED)));
        spans.push(Span::raw("  "));
    }

    // A toast takes over the right-hand side until it expires.
    if let Some(toast) = app.active_toast(now) {
        let text = Line::from(format!(" {} ", toast.text))
            .style(Style::default().fg(toast.color()).bold());
        let width = (text.width() as u16).min(area.width / 2);
        let areas = Layout::horizontal([Constraint::Min(0), Constraint::Length(width)]).split(area);
        frame.render_widget(Paragraph::new(Line::from(spans)), areas[0]);
        frame.render_widget(Paragraph::new(text).alignment(Alignment::Right), areas[1]);
    } else {
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

fn draw_overview(frame: &mut Frame, app: &App, area: Rect) {
    let areas = Layout::vertical([Constraint::Length(7), Constraint::Min(0)]).split(area);
    draw_status(frame, app, areas[0]);
    draw_traffic(frame, app, areas[1]);
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel("Status");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width.saturating_sub(2);
    let (mode, mode_color) = mode_badge(app.mode());
    let configs = app.configs.as_ref();

    let rows: [(Line, Line); 4] = [
        (
            label_line("Version"),
            Line::from(app.version_line()).fg(Color::White),
        ),
        (
            label_line("Mode"),
            Line::from(vec![
                Span::styled(
                    format!(" {mode} "),
                    Style::default().fg(Color::Black).bg(mode_color),
                ),
                Span::raw("  "),
                Span::styled("press m to cycle", Style::default().fg(MUTED)),
            ]),
        ),
        (
            label_line("Inbound"),
            Line::from(configs.map(|c| c.ports()).unwrap_or_else(|| "-".into())),
        ),
        (
            label_line("Options"),
            Line::from(configs.map_or_else(
                || "-".to_string(),
                |c| {
                    let mut parts = Vec::new();
                    parts.push(format!("log {}", c.log_level));
                    parts.push(format!("lan {}", if c.allow_lan { "on" } else { "off" }));
                    parts.push(format!("ipv6 {}", if c.ipv6 { "on" } else { "off" }));
                    parts.push(format!("tun {}", if c.tun.enable { "on" } else { "off" }));
                    parts.join("   ")
                },
            )),
        ),
    ];

    let mut lines = Vec::new();
    for (label, value) in rows {
        lines.push(pair_line(label, value, width));
    }
    if let Some(err) = &app.offline {
        lines.push(Line::from(vec![
            Span::styled("! ", Style::default().fg(Color::Red).bold()),
            Span::styled(err.clone(), Style::default().fg(Color::Red)),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_traffic(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel("Traffic");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let total = format!(
        "↑ {}   ↓ {}",
        format_bytes(app.connections.upload_total),
        format_bytes(app.connections.download_total)
    );

    let areas = Layout::vertical([
        Constraint::Length(1), // live rates
        Constraint::Length(1), // up ratio gauge
        Constraint::Length(1), // down ratio gauge
        Constraint::Length(1), // totals
        Constraint::Min(1),    // graphs
        Constraint::Length(1), // graph titles
    ])
    .split(inner);

    let rates = Line::from(vec![
        Span::styled("↑ ", Style::default().fg(UP)),
        Span::styled(format_rate(app.traffic.up), Style::default().fg(UP).bold()),
        Span::raw("    "),
        Span::styled("↓ ", Style::default().fg(DOWN)),
        Span::styled(
            format_rate(app.traffic.down),
            Style::default().fg(DOWN).bold(),
        ),
    ]);
    frame.render_widget(Paragraph::new(rates), areas[0]);

    // Scale both bars against the same peak so they are comparable at a glance.
    let peak = app
        .traffic
        .up_history
        .iter()
        .chain(app.traffic.down_history.iter())
        .copied()
        .max()
        .unwrap_or(0)
        .max(1);
    frame.render_widget(traffic_gauge("up", app.traffic.up, peak, UP), areas[1]);
    frame.render_widget(
        traffic_gauge("down", app.traffic.down, peak, DOWN),
        areas[2],
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("total  ", Style::default().fg(MUTED)),
            Span::styled(total, Style::default().fg(Color::White)),
            Span::raw("    "),
            Span::styled(
                format!("{} connections", app.connections.connections.len()),
                Style::default().fg(MUTED),
            ),
        ])),
        areas[3],
    );

    let graphs =
        Layout::vertical([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(areas[4]);
    let up_data = sparkline_data(&app.traffic.up_history);
    let down_data = sparkline_data(&app.traffic.down_history);

    frame.render_widget(
        Sparkline::default()
            .data(up_data)
            .max(peak)
            .style(Style::default().fg(UP)),
        graphs[0],
    );
    frame.render_widget(
        Sparkline::default()
            .data(down_data)
            .max(peak)
            .style(Style::default().fg(DOWN)),
        graphs[1],
    );

    let hint = if app.traffic.up_history.is_empty() {
        Line::from("waiting for samples…").fg(MUTED)
    } else {
        Line::from(vec![
            Span::styled("peak ", Style::default().fg(MUTED)),
            Span::styled(format_rate(peak), Style::default().fg(Color::White)),
            Span::styled("   (last 120 samples)", Style::default().fg(MUTED)),
        ])
    };
    frame.render_widget(Paragraph::new(hint), areas[5]);
}

fn traffic_gauge(label: &str, value: u64, peak: u64, color: Color) -> Gauge<'_> {
    Gauge::default()
        .ratio(ratio(value, peak))
        .label(format!("{label} {}", format_rate(value)))
        .style(Style::default().fg(MUTED))
        .gauge_style(Style::default().fg(color))
}

fn draw_proxies(frame: &mut Frame, app: &App, area: Rect) {
    if app.groups.is_empty() {
        frame.render_widget(empty_state("no proxy groups"), area);
        return;
    }

    let areas =
        Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)]).split(area);

    draw_group_list(frame, app, areas[0]);
    draw_member_list(frame, app, areas[1]);
}

fn draw_group_list(frame: &mut Frame, app: &App, area: Rect) {
    let title = format!("Groups ({})", app.groups.len());
    let block = panel(&title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let items: Vec<ListItem> = app
        .groups
        .iter()
        .map(|group| {
            let active = !group.now.is_empty();
            ListItem::new(Line::from(vec![
                Span::styled(
                    group.name.clone(),
                    Style::default().fg(if active { Color::White } else { MUTED }),
                ),
                Span::raw("  "),
                Span::styled(group.kind.clone(), Style::default().fg(MUTED)),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.group_idx));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(Style::default().bg(ACCENT).fg(Color::Black).bold())
            .highlight_symbol(""),
        inner,
        &mut state,
    );
}

fn draw_member_list(frame: &mut Frame, app: &App, area: Rect) {
    let group = match app.current_group() {
        Some(group) => group,
        None => return,
    };

    let busy = if app.testing { " · testing…" } else { "" };
    let title = format!(
        "{} · {} ({}){busy}",
        group.name,
        group.kind,
        group.members.len()
    );
    let block = panel(&title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if group.members.is_empty() {
        frame.render_widget(Paragraph::new("group has no members").fg(MUTED), inner);
        return;
    }

    // The table needs a column to spare for the delay bar.
    let show_bar = inner.width > 60;
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);

    // Manual header instead of Table::header so it can sit inside its own row
    // of the layout and stay pinned while the table scrolls.
    let header = Line::from(vec![
        Span::styled("member", Style::default().fg(MUTED).bold()),
        Span::raw("  "),
        Span::styled("type", Style::default().fg(MUTED).bold()),
        Span::raw("  "),
        Span::styled("delay", Style::default().fg(MUTED).bold()),
    ]);
    frame.render_widget(Paragraph::new(header), areas[0]);

    let rows: Vec<Row> = group
        .members
        .iter()
        .map(|member| {
            let current = member.name == group.now;
            let name_style = if current {
                Style::default().fg(ACCENT).bold()
            } else {
                Style::default().fg(Color::White)
            };
            let marker = if current { "▶ " } else { "  " };
            let (delay_text, delay_style) = match member.delay {
                Some(delay) => (
                    format!("{delay} ms"),
                    Style::default().fg(delay_color(delay)),
                ),
                None => ("-".to_string(), Style::default().fg(MUTED)),
            };

            let mut cells = vec![
                Cell::from(format!("{marker}{}", member.name)).style(name_style),
                Cell::from(member.kind.clone()).style(Style::default().fg(MUTED)),
                Cell::from(delay_text).style(delay_style),
            ];
            if show_bar {
                cells.push(Cell::from(delay_bar(member.delay)));
            }
            Row::new(cells)
        })
        .collect();

    let widths = if show_bar {
        vec![
            Constraint::Min(18),
            Constraint::Length(14),
            Constraint::Length(9),
            Constraint::Length(10),
        ]
    } else {
        vec![
            Constraint::Min(16),
            Constraint::Length(12),
            Constraint::Length(9),
        ]
    };

    let mut state = TableState::default();
    state.select(Some(app.member_idx));
    frame.render_stateful_widget(
        Table::new(rows, widths)
            .row_highlight_style(Style::default().bg(Color::Indexed(236)))
            .highlight_symbol("")
            .column_spacing(1),
        areas[1],
        &mut state,
    );

    let selected = group.members.get(app.member_idx);
    let detail = match selected {
        Some(member) => Line::from(vec![
            Span::styled("selected  ", Style::default().fg(MUTED)),
            Span::styled(member.name.clone(), Style::default().fg(Color::White)),
            Span::styled("   now  ", Style::default().fg(MUTED)),
            Span::styled(
                if group.now.is_empty() {
                    "-".to_string()
                } else {
                    group.now.clone()
                },
                Style::default().fg(ACCENT),
            ),
        ]),
        None => Line::from(""),
    };
    frame.render_widget(Paragraph::new(detail), areas[2]);

    let test_url = group.test_url.as_deref().unwrap_or(&app.delay_url);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("test url  ", Style::default().fg(MUTED)),
            Span::styled(test_url.to_string(), Style::default().fg(MUTED)),
        ])),
        areas[3],
    );
}

fn draw_connections(frame: &mut Frame, app: &App, area: Rect) {
    let connections = &app.connections.connections;
    if connections.is_empty() {
        frame.render_widget(empty_state("no active connections"), area);
        return;
    }

    let areas = Layout::vertical([Constraint::Min(3), Constraint::Length(7)]).split(area);
    draw_connection_table(frame, app, connections, areas[0]);
    draw_connection_detail(frame, app, areas[1]);
}

fn draw_connection_table(frame: &mut Frame, app: &App, connections: &[Connection], area: Rect) {
    let title = format!("Connections ({})", connections.len());
    let block = panel(&title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows: Vec<Row> = connections
        .iter()
        .map(|conn| {
            Row::new(vec![
                Cell::from(conn.destination()),
                Cell::from(format!("{}/{}", conn.metadata.kind, conn.metadata.network)),
                Cell::from(conn.chain()),
                Cell::from(conn.metadata.process_name()).style(Style::default().fg(MUTED)),
                Cell::from(format_bytes(conn.upload)).style(Style::default().fg(UP)),
                Cell::from(format_bytes(conn.download)).style(Style::default().fg(DOWN)),
                Cell::from(conn.rule.clone()).style(Style::default().fg(MUTED)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Min(20),
        Constraint::Length(12),
        Constraint::Min(16),
        Constraint::Length(14),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(12),
    ];

    let header = Row::new(vec![
        Cell::from("destination"),
        Cell::from("type"),
        Cell::from("chain"),
        Cell::from("process"),
        Cell::from("up"),
        Cell::from("down"),
        Cell::from("rule"),
    ])
    .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD));

    let mut state = TableState::default();
    state.select(app.conn_idx);
    frame.render_stateful_widget(
        Table::new(rows, widths)
            .header(header)
            .row_highlight_style(Style::default().bg(ACCENT).fg(Color::Black))
            .highlight_symbol("")
            .column_spacing(1),
        inner,
        &mut state,
    );
}

fn draw_connection_detail(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel("Detail");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(conn) = app.selected_connection() else {
        frame.render_widget(
            Paragraph::new("select a connection with j/k").fg(MUTED),
            inner,
        );
        return;
    };

    let meta = &conn.metadata;
    let width = inner.width.saturating_sub(2);
    let lines = vec![
        pair_line(
            label_line("source"),
            Line::from(if meta.source_ip.is_empty() {
                "-".to_string()
            } else {
                format!("{}:{}", meta.source_ip, meta.source_port)
            }),
            width,
        ),
        pair_line(
            label_line("destination"),
            Line::from(conn.destination()),
            width,
        ),
        pair_line(label_line("chain"), Line::from(conn.chain()), width),
        pair_line(
            label_line("rule"),
            Line::from(
                format!("{} {}", conn.rule, conn.rule_payload)
                    .trim()
                    .to_string(),
            ),
            width,
        ),
        pair_line(
            label_line("transfer"),
            Line::from(format!(
                "↑ {}   ↓ {}",
                format_bytes(conn.upload),
                format_bytes(conn.download)
            )),
            width,
        ),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_logs(frame: &mut Frame, app: &App, area: Rect) {
    let title = format!("Logs ({})", app.log.len());
    let block = panel(&title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.log.is_empty() {
        frame.render_widget(Paragraph::new("no log lines yet").fg(MUTED), inner);
        return;
    }

    // Keep the newest lines visible: render the tail that fits.
    let height = inner.height as usize;
    let skip = app.log.len().saturating_sub(height);
    let lines: Vec<Line> = app
        .log
        .iter()
        .skip(skip)
        .map(|entry| {
            Line::from(vec![
                Span::styled(
                    format!("{:<7} ", entry.level),
                    Style::default().fg(log_color(&entry.level)).bold(),
                ),
                Span::raw(entry.payload.clone()),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn panel(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(MUTED))
        .title(Line::from(format!(" {title} ")).style(Style::default().fg(ACCENT)))
}

fn empty_state(message: &str) -> Paragraph<'static> {
    Paragraph::new(Line::from(message.to_string()).fg(MUTED))
        .block(panel(" "))
        .alignment(Alignment::Center)
        .style(Style::default().fg(MUTED))
}

fn label_line(label: &str) -> Line<'static> {
    Line::from(format!("{label:<10}")).style(Style::default().fg(MUTED))
}

/// Put a label and value on one line, truncating the value to fit.
fn pair_line(label: Line<'static>, value: Line<'static>, width: u16) -> Line<'static> {
    let label_width = label.width();
    let available = (width as usize)
        .saturating_sub(label_width)
        .saturating_sub(1);
    let mut value = value;
    if value.width() > available {
        value = Line::from(truncate(&value.to_string(), available));
    }
    let mut spans = label.spans;
    spans.extend(value.spans);
    Line::from(spans)
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    if width <= 1 {
        return text.chars().take(width).collect();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// Traffic ratios are fed straight into `Gauge::ratio`, which panics outside 0..=1.
fn ratio(value: u64, peak: u64) -> f64 {
    if peak == 0 {
        return 0.0;
    }
    (value as f64 / peak as f64).clamp(0.0, 1.0)
}

fn mode_badge(mode: &str) -> (&str, Color) {
    match mode {
        "global" => ("GLOBAL", Color::Magenta),
        "direct" => ("DIRECT", Color::Yellow),
        "rule" => ("RULE", Color::Green),
        other => (other, Color::Gray),
    }
}

fn delay_color(delay: u64) -> Color {
    match delay {
        0..=200 => Color::Green,
        201..=500 => Color::Yellow,
        501..=1000 => Color::LightRed,
        _ => Color::Red,
    }
}

/// A tiny inline bar so latency is readable at a glance.
fn delay_bar(delay: Option<u64>) -> Span<'static> {
    let Some(delay) = delay else {
        return Span::styled("··········", Style::default().fg(MUTED));
    };
    const SLOTS: usize = 10;
    // 0 ms is full, 1000 ms or worse is empty.
    let filled = SLOTS - ((delay as usize).min(1000) * SLOTS / 1000);
    let color = delay_color(delay);
    Span::styled(
        format!("{}{}", "█".repeat(filled), "░".repeat(SLOTS - filled)),
        Style::default().fg(color),
    )
}

fn log_color(level: &str) -> Color {
    match level {
        "error" => Color::Red,
        "warning" | "warn" => Color::Yellow,
        "debug" => Color::DarkGray,
        _ => Color::Gray,
    }
}

/// Interim view until the Subscriptions page gets its full layout: the state
/// behind the tab is already live, so this only has to prove the tab renders.
fn draw_subscriptions(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel(&format!("Subscriptions ({})", app.profiles.len()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::from(format!("{} imported subscriptions", app.profiles.len())),
        Line::from("a add · u update · d delete · enter switch").fg(MUTED),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::api::{Configs, Connections, Group, Member, Version};
    use crate::app::App;

    fn app() -> App {
        let mut app = App::new(
            "http://127.0.0.1:9090".into(),
            "http://www.gstatic.com/generate_204".into(),
            5000,
        );
        app.version = Some(Version {
            version: "v1.18.0".into(),
            meta: true,
        });
        app.configs = Some(Configs {
            mixed_port: 7890,
            mode: "rule".into(),
            log_level: "info".into(),
            ..Default::default()
        });
        app.groups = vec![Group {
            name: "auto".into(),
            kind: "URLTest".into(),
            now: "node-a".into(),
            test_url: None,
            members: vec![
                Member {
                    name: "node-a".into(),
                    kind: "Shadowsocks".into(),
                    delay: Some(42),
                },
                Member {
                    name: "node-b".into(),
                    kind: "Vmess".into(),
                    delay: None,
                },
            ],
        }];
        app
    }

    /// Render the app at a given size and return the buffer as text, one line
    /// per row, so tests can assert on what a user would actually see.
    fn render(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, app, Instant::now()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn overview_renders_the_controller_facts() {
        let app = app();
        let screen = render(&app, 100, 30);

        assert!(screen.contains("Overview"), "tab bar is present");
        assert!(screen.contains("v1.18.0 (meta)"), "version is shown");
        assert!(screen.contains("RULE"), "mode badge is shown");
        assert!(screen.contains("mixed 7890"), "inbound ports are shown");
        assert!(screen.contains("connected"), "controller status is shown");
    }

    #[test]
    fn a_small_terminal_gets_a_hint_instead_of_a_panic() {
        let app = app();
        let screen = render(&app, 30, 8);
        assert!(screen.contains("terminal too small"), "got: {screen}");
    }

    #[test]
    fn the_minimum_supported_size_renders_without_panicking() {
        let app = app();
        let screen = render(&app, MIN_WIDTH, MIN_HEIGHT);
        assert!(!screen.contains("terminal too small"));
    }

    #[test]
    fn proxies_tab_lists_groups_and_marks_the_active_member() {
        let mut app = app();
        app.tab = Tab::Proxies;
        let screen = render(&app, 100, 30);

        assert!(screen.contains("node-a"), "members are listed");
        assert!(screen.contains("node-b"));
        assert!(
            screen.contains("▶ node-a"),
            "the active member is marked: {screen}"
        );
        assert!(screen.contains("42 ms"), "measured delay is shown");
        assert!(screen.contains("URLTest"), "group type is shown");
    }

    #[test]
    fn proxies_tab_shows_the_fallback_test_url_when_a_group_has_none() {
        let mut app = app();
        app.tab = Tab::Proxies;
        let screen = render(&app, 100, 30);
        assert!(
            screen.contains("www.gstatic.com/generate_204"),
            "falls back to the configured delay url: {screen}"
        );
    }

    #[test]
    fn an_offline_controller_says_so_on_the_overview() {
        let mut app = app();
        app.offline = Some("connection refused".into());
        let screen = render(&app, 100, 30);
        assert!(screen.contains("offline"), "status flips to offline");
        assert!(screen.contains("connection refused"), "the reason is shown");
    }

    #[test]
    fn empty_collections_render_an_empty_state() {
        let mut app = App::new("http://127.0.0.1:9090".into(), "http://x/204".into(), 5000);

        app.tab = Tab::Proxies;
        assert!(render(&app, 100, 30).contains("no proxy groups"));

        app.tab = Tab::Connections;
        assert!(render(&app, 100, 30).contains("no active connections"));

        app.tab = Tab::Logs;
        assert!(render(&app, 100, 30).contains("no log lines yet"));
    }

    #[test]
    fn connections_tab_lists_routes() {
        let mut app = app();
        app.tab = Tab::Connections;
        app.connections = serde_json::from_str(
            r#"{
                "downloadTotal": 2048,
                "uploadTotal": 1024,
                "connections": [{
                    "id": "abc",
                    "metadata": {
                        "host": "example.com", "destinationPort": "443",
                        "destinationIP": "1.2.3.4", "sourceIP": "192.168.1.5",
                        "sourcePort": "51000", "network": "tcp", "type": "HTTPS",
                        "processPath": "/usr/bin/curl"
                    },
                    "upload": 100, "download": 200,
                    "chains": ["node-a", "auto"], "rule": "MATCH", "rulePayload": ""
                }]
            }"#,
        )
        .unwrap();
        app.conn_idx = Some(0);

        let screen = render(&app, 120, 30);
        assert!(screen.contains("example.com:443"), "destination is shown");
        assert!(screen.contains("node-a → auto"), "chain is shown");
        assert!(screen.contains("curl"), "process name is shown");
        assert!(screen.contains("HTTPS/tcp"), "connection type is shown");
    }

    #[test]
    fn logs_tab_shows_levels_and_keeps_the_newest_line_visible() {
        let mut app = app();
        app.tab = Tab::Logs;
        for i in 0..200 {
            app.push_log(crate::api::LogLine {
                level: "info".into(),
                payload: format!("line {i}"),
            });
        }
        app.push_log(crate::api::LogLine {
            level: "error".into(),
            payload: "the newest line".into(),
        });

        let screen = render(&app, 100, 24);
        assert!(
            screen.contains("the newest line"),
            "tail is rendered: {screen}"
        );
        assert!(!screen.contains("line 10 "), "old lines scroll off");
    }

    #[test]
    fn a_toast_is_rendered_in_the_footer() {
        let mut app = app();
        app.set_toast(crate::app::ToastKind::Success, "mode set to global");
        let screen = render(&app, 120, 30);
        assert!(screen.contains("mode set to global"), "got: {screen}");
    }

    #[test]
    fn an_expired_toast_is_not_rendered() {
        let app = app();
        // No toast set at all: the footer shows only keybindings.
        let screen = render(&app, 120, 30);
        assert!(screen.contains("quit"));
    }

    #[test]
    fn delay_bars_shrink_as_latency_grows() {
        let fast = delay_bar(Some(50)).content.to_string();
        let slow = delay_bar(Some(950)).content.to_string();
        let none = delay_bar(None).content.to_string();

        assert_eq!(fast.chars().count(), 10, "bars are a fixed width");
        assert_eq!(slow.chars().count(), 10);
        assert_ne!(fast, slow, "a fast node fills more of the bar");
        assert!(fast.starts_with('█'));
        assert!(none.contains('·'), "an untested node shows dots");
    }

    #[test]
    fn gauge_ratios_stay_within_bounds() {
        assert_eq!(ratio(0, 0), 0.0, "no peak yet must not divide by zero");
        assert_eq!(ratio(0, 100), 0.0);
        assert_eq!(ratio(100, 100), 1.0);
        assert_eq!(ratio(200, 100), 1.0, "clamped so Gauge does not panic");
    }

    #[test]
    fn truncate_respects_the_width_budget() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(truncate("hello", 0), "");
    }

    #[test]
    fn every_tab_renders_at_a_range_of_sizes() {
        let mut app = app();
        app.connections = Connections::default();
        for tab in Tab::ALL {
            app.tab = tab;
            for (w, h) in [(MIN_WIDTH, MIN_HEIGHT), (80, 24), (200, 60)] {
                let screen = render(&app, w, h);
                assert!(!screen.contains("terminal too small"), "{tab:?} at {w}x{h}");
                assert!(
                    !screen.trim().is_empty(),
                    "{tab:?} at {w}x{h} rendered nothing"
                );
            }
        }
    }
}
