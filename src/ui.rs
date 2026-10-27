//! Rendering. Every function here takes `&App` and draws; nothing mutates
//! application state or touches the network.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, BorderType, Cell, Chart, Clear, Dataset, GraphType, List, ListItem, ListState,
    Paragraph, Row, Table, TableState, Tabs,
};

use crate::api::Connection;
use crate::app::{
    App, InputField, InputOverlay, PendingSwitch, RulesOverlay, SettingsOverlay, Tab, format_bytes,
    format_rate,
};
use crate::merge::Source;
use crate::store::ProfileMeta;

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
        Constraint::Length(1), // tabs + hints
        Constraint::Min(3),    // body
    ])
    .split(area);

    draw_header(frame, app, areas[0], now);

    frame.render_widget(Clear, areas[1]);

    match app.tab {
        Tab::Overview => draw_overview(frame, app, areas[1]),
        Tab::Proxies => draw_proxies(frame, app, areas[1]),
        Tab::Connections => draw_connections(frame, app, areas[1]),
        Tab::Logs => draw_logs(frame, app, areas[1]),
        Tab::Subscriptions => draw_subscriptions(frame, app, areas[1]),
    }

    // Modals are drawn last so they cover the page and its footer. Keyboard
    // priority in `App` keeps them mutually exclusive.
    if let Some(pending) = &app.pending {
        draw_confirm_overlay(frame, app, pending, area);
    }
    if let Some(input) = &app.input {
        draw_input_overlay(frame, input, area);
    }
    if let Some(rules) = &app.rules {
        draw_rules_overlay(frame, app, rules, area);
    }
    if let Some(settings) = &app.settings {
        draw_settings_overlay(frame, app, settings, area);
    }
}

/// The exact string drawn for one tab. The header and the hitboxes share it,
/// so a click can never disagree with the pixels.
fn tab_title(index: usize, tab: Tab) -> String {
    format!(" {} {} ", index + 1, tab.title())
}

/// Click rectangles for the tab strip. `width` is the strip's rendered width
/// (not the terminal width), so clicks in the right-aligned status area never
/// match a tab that was clipped away.
pub fn tab_hitboxes(width: u16) -> Vec<(std::ops::Range<u16>, Tab)> {
    let mut start = 0u16;
    Tab::ALL
        .iter()
        .enumerate()
        .filter_map(|(index, tab)| {
            let end = start + tab_title(index, *tab).chars().count() as u16;
            let range = start..end.min(width);
            start = end;
            (range.start < width).then_some((range, *tab))
        })
        .collect()
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect, now: Instant) {
    let titles: Vec<Line> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, tab)| Line::from(tab_title(i, *tab)))
        .collect();

    let selected = Tab::ALL.iter().position(|t| *t == app.tab);
    let tabs = Tabs::new(titles)
        .select(selected)
        .highlight_style(Style::default().fg(Color::Black).bg(ACCENT).bold())
        .divider("")
        .padding("", "");

    let tabs_len: u16 = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, tab)| tab_title(i, *tab).chars().count() as u16)
        .sum();

    if area.width <= tabs_len {
        app.tab_width.set(area.width);
        frame.render_widget(tabs, area);
        return;
    }

    let header_areas =
        Layout::horizontal([Constraint::Length(tabs_len), Constraint::Min(0)]).split(area);
    app.tab_width.set(header_areas[0].width);
    frame.render_widget(tabs, header_areas[0]);

    // Right-hand side of tab bar: active toast or operation hints.
    if let Some(toast) = app.active_toast(now) {
        let text = Line::from(format!(" {} ", toast.text))
            .style(Style::default().fg(toast.color()).bold());
        frame.render_widget(
            Paragraph::new(text).alignment(Alignment::Right),
            header_areas[1],
        );
        return;
    }

    let keys = action_hints(app);
    let right_width = header_areas[1].width as usize;

    let mut spans = Vec::new();
    let mut used_len = 1;
    for (key, label) in keys {
        let item_len = key.chars().count() + 1 + label.chars().count() + 2;
        if spans.is_empty() || used_len + item_len <= right_width {
            spans.push(Span::styled(key, Style::default().fg(ACCENT)));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(label, Style::default().fg(MUTED)));
            spans.push(Span::raw("  "));
            used_len += item_len;
        } else {
            break;
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Right),
        header_areas[1],
    );
}

fn action_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let mut keys: Vec<(&'static str, &'static str)> = match app.tab {
        Tab::Overview => vec![
            ("m/M", "mode"),
            ("u", "tun"),
            ("l", "lan"),
            ("o", "options"),
            ("R", "rules"),
            ("r", "refresh"),
            ("tab", "switch"),
            ("q", "quit"),
        ],
        Tab::Proxies => vec![
            ("j/k", "group"),
            ("h/l", "member"),
            ("enter", "select"),
            ("t/T/A", "test"),
            ("s", "sort"),
            ("r", "refresh"),
            ("q", "quit"),
        ],
        Tab::Connections => vec![
            ("j/k", "move"),
            ("x", "close"),
            ("X", "close all"),
            ("r", "refresh"),
            ("q", "quit"),
        ],
        Tab::Logs => vec![("c", "clear"), ("tab", "switch"), ("q", "quit")],
        Tab::Subscriptions => vec![
            ("a", "add"),
            ("u", "update"),
            ("d", "delete"),
            ("enter", "switch"),
            ("[/]", "cycle"),
            ("q", "quit"),
        ],
    };

    // The two-stage delete has no modal; the hint area is where the second
    // press is advertised.
    if app.delete_armed.is_some()
        && let Some(entry) = keys.iter_mut().find(|(key, _)| *key == "d")
    {
        *entry = ("d", "again = delete");
    }
    if app.external_dirty.is_some() {
        keys.push(("v", "review changes"));
    }

    keys
}

fn draw_overview(frame: &mut Frame, app: &App, area: Rect) {
    let status_height = if app.offline.is_some() { 9 } else { 8 };
    if area.height >= 20 {
        let traffic_height = 10.min(area.height.saturating_sub(status_height + 4));
        let areas = Layout::vertical([
            Constraint::Length(status_height),
            Constraint::Length(traffic_height),
            Constraint::Min(4),
        ])
        .split(area);
        draw_status(frame, app, areas[0]);
        draw_traffic(frame, app, areas[1]);
        draw_custom_rules_panel(frame, app, areas[2]);
    } else {
        let areas =
            Layout::vertical([Constraint::Length(status_height), Constraint::Min(0)]).split(area);
        draw_status(frame, app, areas[0]);
        draw_traffic(frame, app, areas[1]);
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel("Status");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width.saturating_sub(2);
    let (mode, mode_color) = mode_badge(app.mode());
    let configs = app.configs.as_ref();

    let (status_text, status_color) = if app.offline.is_some() {
        ("offline", Color::Red)
    } else {
        ("connected", Color::Green)
    };
    let host = app.endpoint.trim_end_matches('/');

    let rules_val = if app.prepend_rules.is_empty() && app.append_rules.is_empty() {
        Line::from(vec![
            Span::styled("none", Style::default().fg(MUTED)),
            Span::raw("  "),
            Span::styled("(press R to configure)", Style::default().fg(MUTED)),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                format!(
                    "{} prepend, {} append",
                    app.prepend_rules.len(),
                    app.append_rules.len()
                ),
                Style::default().fg(Color::White),
            ),
            Span::raw("  "),
            Span::styled("press R to manage", Style::default().fg(MUTED)),
        ])
    };

    let rows: [(Line, Line); 6] = [
        (
            label_line("Endpoint"),
            Line::from(vec![
                Span::styled(host.to_string(), Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled(
                    format!("● {status_text}"),
                    Style::default().fg(status_color),
                ),
            ]),
        ),
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
        (label_line("Rules"), rules_val),
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
    let block = panel("Traffic (Realtime Curve)");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let total_up = format_bytes(app.connections.upload_total);
    let total_down = format_bytes(app.connections.download_total);
    let conns = app.connections.connections.len();

    let peak = app
        .traffic
        .up_history
        .iter()
        .chain(app.traffic.down_history.iter())
        .copied()
        .max()
        .unwrap_or(0)
        .max(1024);

    let summary = Line::from(vec![
        Span::styled("↑ ", Style::default().fg(UP).bold()),
        Span::styled(
            format!("{:<8}", format_rate(app.traffic.up)),
            Style::default().fg(UP).bold(),
        ),
        Span::styled(format!("({total_up})"), Style::default().fg(MUTED)),
        Span::raw("   "),
        Span::styled("↓ ", Style::default().fg(DOWN).bold()),
        Span::styled(
            format!("{:<8}", format_rate(app.traffic.down)),
            Style::default().fg(DOWN).bold(),
        ),
        Span::styled(format!("({total_down})"), Style::default().fg(MUTED)),
        Span::raw("   "),
        Span::styled("peak ", Style::default().fg(MUTED)),
        Span::styled(format_rate(peak), Style::default().fg(Color::White)),
        Span::raw("   "),
        Span::styled(format!("{conns} conns"), Style::default().fg(MUTED)),
    ]);

    if inner.height <= 3 {
        frame.render_widget(Paragraph::new(summary), inner);
        return;
    }

    let areas = Layout::vertical([
        Constraint::Length(1), // summary line
        Constraint::Min(2),    // braille chart
    ])
    .split(inner);

    frame.render_widget(Paragraph::new(summary), areas[0]);

    let up_history = &app.traffic.up_history;
    let down_history = &app.traffic.down_history;

    let up_points: Vec<(f64, f64)> = if up_history.is_empty() {
        vec![(0.0, 0.0)]
    } else {
        up_history
            .iter()
            .enumerate()
            .map(|(i, &rate)| (i as f64, rate as f64))
            .collect()
    };

    let down_points: Vec<(f64, f64)> = if down_history.is_empty() {
        vec![(0.0, 0.0)]
    } else {
        down_history
            .iter()
            .enumerate()
            .map(|(i, &rate)| (i as f64, rate as f64))
            .collect()
    };

    let max_x = (up_history.len().max(down_history.len()).max(1) as f64).max(30.0);

    let dataset_up = Dataset::default()
        .name("↑ Up")
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(UP))
        .data(&up_points);

    let dataset_down = Dataset::default()
        .name("↓ Down")
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(DOWN))
        .data(&down_points);

    let x_axis = Axis::default()
        .style(Style::default().fg(MUTED))
        .bounds([0.0, max_x])
        .labels(vec![
            Span::styled("30s ago", Style::default().fg(MUTED)),
            Span::styled("15s", Style::default().fg(MUTED)),
            Span::styled("now", Style::default().fg(MUTED)),
        ]);

    let y_axis = Axis::default()
        .style(Style::default().fg(MUTED))
        .bounds([0.0, peak as f64])
        .labels(vec![
            Span::styled("0", Style::default().fg(MUTED)),
            Span::styled(format_rate(peak / 2), Style::default().fg(MUTED)),
            Span::styled(format_rate(peak), Style::default().fg(Color::White)),
        ]);

    let chart = Chart::new(vec![dataset_up, dataset_down])
        .x_axis(x_axis)
        .y_axis(y_axis);

    frame.render_widget(chart, areas[1]);
}

fn draw_custom_rules_panel(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel("Custom Rules (base.yaml)");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.prepend_rules.is_empty() && app.append_rules.is_empty() {
        let msg = vec![
            Line::from(vec![Span::styled(
                "No custom rules configured in base.yaml",
                Style::default().fg(MUTED),
            )]),
            Line::from(vec![
                Span::styled("Press ", Style::default().fg(MUTED)),
                Span::styled("R", Style::default().fg(ACCENT).bold()),
                Span::styled(
                    " to add Prepend (置顶优先) or Append (追加兜底) rules.",
                    Style::default().fg(MUTED),
                ),
            ]),
        ];
        frame.render_widget(Paragraph::new(msg), inner);
        return;
    }

    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner);

    let max_display = cols[0].height.saturating_sub(1) as usize;

    // Left: Prepend Rules
    let mut prepend_lines = vec![Line::from(vec![
        Span::styled(
            "● Prepend Rules (置顶优先) ",
            Style::default().fg(Color::Yellow).bold(),
        ),
        Span::styled(
            format!("({})", app.prepend_rules.len()),
            Style::default().fg(MUTED),
        ),
    ])];
    if app.prepend_rules.is_empty() {
        prepend_lines.push(Line::from("  (none)").fg(MUTED));
    } else {
        for (i, rule) in app.prepend_rules.iter().take(max_display).enumerate() {
            prepend_lines.push(Line::from(vec![
                Span::styled(format!("  {}. ", i + 1), Style::default().fg(MUTED)),
                Span::styled(rule.clone(), Style::default().fg(Color::White)),
            ]));
        }
        if app.prepend_rules.len() > max_display {
            prepend_lines.push(
                Line::from(format!(
                    "  … +{} more",
                    app.prepend_rules.len() - max_display
                ))
                .fg(MUTED),
            );
        }
    }
    frame.render_widget(Paragraph::new(prepend_lines), cols[0]);

    // Right: Append Rules
    let mut append_lines = vec![Line::from(vec![
        Span::styled(
            "○ Append Rules (追加兜底) ",
            Style::default().fg(Color::Cyan).bold(),
        ),
        Span::styled(
            format!("({})", app.append_rules.len()),
            Style::default().fg(MUTED),
        ),
    ])];
    if app.append_rules.is_empty() {
        append_lines.push(Line::from("  (none)").fg(MUTED));
    } else {
        for (i, rule) in app.append_rules.iter().take(max_display).enumerate() {
            append_lines.push(Line::from(vec![
                Span::styled(format!("  {}. ", i + 1), Style::default().fg(MUTED)),
                Span::styled(rule.clone(), Style::default().fg(Color::White)),
            ]));
        }
        if app.append_rules.len() > max_display {
            append_lines.push(
                Line::from(format!(
                    "  … +{} more",
                    app.append_rules.len() - max_display
                ))
                .fg(MUTED),
            );
        }
    }
    frame.render_widget(Paragraph::new(append_lines), cols[1]);
}

fn draw_proxies(frame: &mut Frame, app: &App, area: Rect) {
    if app.groups.is_empty() {
        let block = panel("Groups");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(empty_state("no proxy groups"), inner);
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
    app.members_area.set(Rect::default());
    app.members_top.set(0);
    let group = match app.current_group() {
        Some(group) => group,
        None => return,
    };

    let busy = if app.testing {
        format!(" · testing ({}/{})", app.testing_done, app.testing_total)
    } else {
        String::new()
    };
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

    // `sort_by_delay` is view-only: the underlying member order never
    // changes, so proxy selection and delay application stay stable.
    let mut order: Vec<usize> = (0..group.members.len()).collect();
    if app.sort_by_delay {
        order.sort_by_key(|&index| {
            let delay = group.members[index].delay;
            (delay.is_none(), delay.unwrap_or(u64::MAX), index)
        });
    }

    let rows: Vec<Row> = order
        .iter()
        .map(|&index| {
            let member = &group.members[index];
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
    let selected_position = order
        .iter()
        .position(|&index| index == app.member_idx)
        .unwrap_or(0);
    state.select(Some(selected_position));
    frame.render_stateful_widget(
        Table::new(rows, widths)
            .row_highlight_style(Style::default().bg(Color::Indexed(236)))
            .highlight_symbol("")
            .column_spacing(1),
        areas[1],
        &mut state,
    );
    app.members_area.set(areas[1]);
    app.members_top.set(state.offset());
    *app.members_display_order.borrow_mut() = order;

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
        let block = panel("Connections");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(empty_state("no active connections"), inner);
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

#[allow(dead_code)]
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
    // Reset before every early return so a click can never be interpreted
    // against the previous frame's geometry.
    app.subs_area.set(Rect::default());
    app.subs_top.set(0);

    let block = panel(&format!("Subscriptions ({})", app.profiles.len()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.profiles.is_empty() {
        frame.render_widget(
            empty_state("no subscriptions yet — press a to add one"),
            inner,
        );
        return;
    }

    let rows: Vec<Row> = app
        .profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| {
            let selected = index == app.sub_idx;
            let marker = if selected { "▶ " } else { "  " };
            let name_style = if selected {
                Style::default().fg(ACCENT).bold()
            } else {
                Style::default().fg(Color::White)
            };
            Row::new(vec![
                Cell::from(format!("{marker}{}", profile.name)).style(name_style),
                Cell::from(profile.nodes.to_string()),
                Cell::from(profile.protocols.clone()).style(Style::default().fg(MUTED)),
                Cell::from(updated_label(profile.updated_ms)).style(Style::default().fg(MUTED)),
                Cell::from(policy_label(profile)),
            ])
        })
        .collect();

    let header = Row::new(vec![
        Cell::from("name"),
        Cell::from("nodes"),
        Cell::from("protocols"),
        Cell::from("updated"),
        Cell::from("policy"),
    ])
    .style(Style::default().fg(MUTED).bold());

    let widths = [
        Constraint::Min(16),
        Constraint::Length(5),
        Constraint::Length(22),
        Constraint::Length(11),
        Constraint::Length(9),
    ];

    let mut state = TableState::default();
    state.select(Some(app.sub_idx));
    frame.render_stateful_widget(
        Table::new(rows, widths)
            .header(header)
            .row_highlight_style(Style::default().bg(Color::Indexed(236)))
            .highlight_symbol("")
            .column_spacing(1),
        inner,
        &mut state,
    );

    // `inner` includes the pinned header row; the click mapping subtracts it.
    app.subs_area.set(inner);
    app.subs_top.set(state.offset());
}

/// "just now" / "3m ago": subscriptions are refreshed by hand, so a coarse
/// relative age is more useful than a timestamp.
fn updated_label(updated_ms: u64) -> String {
    if updated_ms == 0 {
        return "-".to_string();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(updated_ms);
    let age_secs = now.saturating_sub(updated_ms) / 1000;
    match age_secs {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", age_secs / 60),
        3_600..=86_399 => format!("{}h ago", age_secs / 3_600),
        _ => format!("{}d ago", age_secs / 86_400),
    }
}

/// Compact policy marker for the subscriptions table: the remembered side
/// when it is uniform, `mixed` when the profile remembers both.
fn policy_label(profile: &ProfileMeta) -> String {
    let mut base = false;
    let mut subscription = false;
    for value in profile.policy.values() {
        match value.as_str() {
            "base" => base = true,
            "subscription" => subscription = true,
            _ => {}
        }
    }
    match (base, subscription) {
        (true, false) => "[base]".to_string(),
        (false, true) => "[sub]".to_string(),
        (true, true) => "[mixed]".to_string(),
        (false, false) => "default".to_string(),
    }
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn draw_confirm_overlay(frame: &mut Frame, app: &App, pending: &PendingSwitch, area: Rect) {
    let width = area.width.saturating_sub(4).min(94);
    let height = (pending.diffs.len() as u16 + 4).min(area.height.saturating_sub(2));
    let popup = centered_rect(area, width, height);
    frame.render_widget(Clear, popup);

    let name = app
        .profiles
        .iter()
        .find(|profile| profile.id == pending.profile_id)
        .map(|profile| profile.name.clone())
        .unwrap_or_else(|| pending.profile_id.clone());
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(
            Line::from(format!(" Confirm switch · {name} "))
                .style(Style::default().fg(ACCENT).bold()),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height == 0 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("  {:<14}", "block"),
                Style::default().fg(MUTED).bold(),
            ),
            Span::styled(format!("{:<18}", "base"), Style::default().fg(MUTED).bold()),
            Span::styled("subscription", Style::default().fg(MUTED).bold()),
        ])),
        rows[0],
    );

    let visible = rows[1].height as usize;
    let offset = pending.cursor.saturating_sub(visible.saturating_sub(1));
    let lines: Vec<Line> = pending
        .diffs
        .iter()
        .enumerate()
        .map(|(index, diff)| {
            let chosen = pending
                .choices
                .get(&diff.block)
                .copied()
                .unwrap_or(Source::Base);
            let cursor = index == pending.cursor;
            let title_style = if cursor {
                Style::default().fg(ACCENT).bold()
            } else if diff.equal {
                Style::default().fg(MUTED)
            } else {
                Style::default().fg(Color::White)
            };
            let base_style = if chosen == Source::Base {
                Style::default().fg(ACCENT).bold()
            } else {
                Style::default().fg(MUTED)
            };
            let sub_style = if chosen == Source::Subscription {
                Style::default().fg(ACCENT).bold()
            } else {
                Style::default().fg(MUTED)
            };
            let base_marker = if chosen == Source::Base { "●" } else { "○" };
            let sub_marker = if chosen == Source::Subscription {
                "●"
            } else {
                "○"
            };
            let base_text = diff.base.as_deref().unwrap_or("-");
            let sub_text = diff.sub.as_deref().unwrap_or("-");

            Line::from(vec![
                Span::styled(if cursor { "▶ " } else { "  " }, title_style),
                Span::styled(format!("{:<14}", diff.block.title()), title_style),
                Span::styled(format!("{base_marker} {base_text:<16}"), base_style),
                Span::styled(" ▸ ", Style::default().fg(MUTED)),
                Span::styled(format!("{sub_marker} {sub_text}"), sub_style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), rows[1]);

    let hint = format!(
        "↑↓ move   b/s or ←→ switch   r remember: {}   enter apply   Esc cancel",
        if pending.remember { "on" } else { "off" }
    );
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(MUTED)),
        rows[2],
    );
}

fn draw_input_overlay(frame: &mut Frame, overlay: &InputOverlay, area: Rect) {
    let popup = centered_rect(
        area,
        area.width.saturating_sub(4).min(78),
        6.min(area.height),
    );
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Line::from(" Add subscription ").style(Style::default().fg(ACCENT).bold()));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let field = |label: &str, value: &str, focused: bool| {
        let label_style = if focused {
            Style::default().fg(ACCENT).bold()
        } else {
            Style::default().fg(MUTED)
        };
        let value_style = if focused {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(MUTED)
        };
        let mut spans = vec![
            Span::styled(format!("{label:<6}"), label_style),
            Span::styled(value.to_string(), value_style),
        ];
        if focused {
            spans.push(Span::styled("▌", Style::default().fg(ACCENT)));
        }
        Line::from(spans)
    };

    let lines = vec![
        field("URL", &overlay.url, overlay.field == InputField::Url),
        field("Name", &overlay.name, overlay.field == InputField::Name),
        Line::from("Tab switch field   enter add   Esc cancel").style(Style::default().fg(MUTED)),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_settings_overlay(frame: &mut Frame, app: &App, settings: &SettingsOverlay, area: Rect) {
    let popup = centered_rect(
        area,
        area.width.saturating_sub(4).min(56),
        10.min(area.height),
    );
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Line::from(" Settings / Options ").style(Style::default().fg(ACCENT).bold()));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let row = |index: usize, label: &str, value: &str| {
        let focused = index == settings.cursor;
        let prefix = if focused { "▶ " } else { "  " };
        let title_style = if focused {
            Style::default().fg(ACCENT).bold()
        } else {
            Style::default().fg(Color::White)
        };
        let value_style = if focused {
            Style::default().fg(Color::Yellow).bold()
        } else {
            Style::default().fg(MUTED)
        };
        Line::from(vec![
            Span::styled(prefix, title_style),
            Span::styled(format!("{label:<14}"), title_style),
            Span::styled(value.to_string(), value_style),
        ])
    };

    let mode_val = format!("◄ {} ►", settings.mode);
    let port_val = if settings.cursor == 1 {
        format!("{}▌", settings.mixed_port)
    } else {
        settings.mixed_port.clone()
    };
    let lan_val = if settings.allow_lan {
        "[● ON ]"
    } else {
        "[○ OFF]"
    };
    let tun_val = if settings.tun {
        "[● ON ]"
    } else {
        "[○ OFF]"
    };
    let rules_val = format!(
        "{} prepend, {} append  [enter to open]",
        app.prepend_rules.len(),
        app.append_rules.len()
    );

    let lines = vec![
        row(0, "Mode", &mode_val),
        row(1, "Mixed Port", &port_val),
        row(2, "Allow LAN", lan_val),
        row(3, "TUN Mode", tun_val),
        row(4, "Custom Rules", &rules_val),
        Line::from(""),
        Line::from("↑↓ move  Space toggle  enter apply/open  Esc cancel")
            .style(Style::default().fg(MUTED)),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_rules_overlay(frame: &mut Frame, app: &App, rules: &RulesOverlay, area: Rect) {
    let popup = centered_rect(
        area,
        area.width.saturating_sub(4).min(72),
        16.min(area.height.saturating_sub(2)),
    );
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(
            Line::from(" Custom Rules Management (base.yaml) ")
                .style(Style::default().fg(ACCENT).bold()),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let bottom_height = if rules.adding { 3 } else { 1 };
    let areas = Layout::vertical([
        Constraint::Length(1),             // tabs: [Prepend Rules]  [Append Rules]
        Constraint::Length(1),             // separator
        Constraint::Min(3),                // list of rules
        Constraint::Length(bottom_height), // input prompt or key hints
    ])
    .split(inner);

    // Tab switcher
    let prepend_style = if rules.is_prepend {
        Style::default().fg(Color::Black).bg(Color::Yellow).bold()
    } else {
        Style::default().fg(MUTED)
    };
    let append_style = if !rules.is_prepend {
        Style::default().fg(Color::Black).bg(Color::Cyan).bold()
    } else {
        Style::default().fg(MUTED)
    };

    let tabs_line = Line::from(vec![
        Span::styled(
            format!(" Prepend ({}) [置顶优先] ", app.prepend_rules.len()),
            prepend_style,
        ),
        Span::raw("   "),
        Span::styled(
            format!(" Append ({}) [追加兜底] ", app.append_rules.len()),
            append_style,
        ),
        Span::raw("   "),
        Span::styled("(Tab to switch category)", Style::default().fg(MUTED)),
    ]);
    frame.render_widget(Paragraph::new(tabs_line), areas[0]);

    frame.render_widget(
        Paragraph::new(Line::from("─".repeat(areas[1].width as usize)).fg(MUTED)),
        areas[1],
    );

    // List of rules in currently active category
    let current_list = if rules.is_prepend {
        &app.prepend_rules
    } else {
        &app.append_rules
    };

    if current_list.is_empty() {
        let empty_msg = Paragraph::new(vec![
            Line::from(""),
            Line::from("  No rules in this category. Press 'a' to add a rule.").fg(MUTED),
            Line::from("  Example: DOMAIN-SUFFIX,google.com,PROXY or GEOIP,CN,DIRECT").fg(MUTED),
        ]);
        frame.render_widget(empty_msg, areas[2]);
    } else {
        let max_visible = areas[2].height as usize;
        let scroll_offset = if rules.cursor >= max_visible {
            rules.cursor - max_visible + 1
        } else {
            0
        };

        let mut lines = Vec::new();
        for (i, rule) in current_list
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(max_visible)
        {
            let selected = i == rules.cursor && !rules.adding;
            let prefix = if selected { " ▶ " } else { "   " };
            let style = if selected {
                Style::default().fg(ACCENT).bold()
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(format!("{:<3} ", i + 1), Style::default().fg(MUTED)),
                Span::styled(rule.clone(), style),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), areas[2]);
    }

    // Input prompt or instructions
    if rules.adding {
        let input_block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Yellow))
            .title(
                Line::from(" Add Rule (e.g. DOMAIN-SUFFIX,openai.com,PROXY) ")
                    .style(Style::default().fg(Color::Yellow)),
            );
        let input_inner = input_block.inner(areas[3]);
        frame.render_widget(input_block, areas[3]);

        let prompt_line = Line::from(vec![
            Span::raw("> "),
            Span::styled(&rules.input_text, Style::default().fg(Color::White).bold()),
            Span::styled("▌", Style::default().fg(Color::Yellow)),
            Span::raw("    "),
            Span::styled("enter confirm   Esc cancel", Style::default().fg(MUTED)),
        ]);
        frame.render_widget(Paragraph::new(prompt_line), input_inner);
    } else {
        let hints = Line::from(vec![
            Span::styled("Tab", Style::default().fg(ACCENT)),
            Span::styled(" switch category   ", Style::default().fg(MUTED)),
            Span::styled("j/k", Style::default().fg(ACCENT)),
            Span::styled(" move   ", Style::default().fg(MUTED)),
            Span::styled("a", Style::default().fg(ACCENT)),
            Span::styled(" add rule   ", Style::default().fg(MUTED)),
            Span::styled("d", Style::default().fg(ACCENT)),
            Span::styled(" delete   ", Style::default().fg(MUTED)),
            Span::styled("Esc", Style::default().fg(ACCENT)),
            Span::styled(" close", Style::default().fg(MUTED)),
        ]);
        frame.render_widget(Paragraph::new(hints), areas[3]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use std::collections::BTreeMap;

    use crate::api::{Configs, Connections, Group, Member, Version};
    use crate::app::{App, InputField, InputOverlay, PendingSwitch, SettingsOverlay};
    use crate::merge::{Block, BlockDiff, Source};
    use crate::store::ProfileMeta;

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
        assert!(
            screen.contains("http://127.0.0.1:9090"),
            "controller endpoint is shown: {screen}"
        );
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
    fn a_toast_is_rendered_in_the_header() {
        let mut app = app();
        app.set_toast(crate::app::ToastKind::Success, "mode set to global");
        let screen = render(&app, 120, 30);
        assert!(screen.contains("mode set to global"), "got: {screen}");
    }

    #[test]
    fn an_expired_toast_is_not_rendered() {
        let app = app();
        // No toast set at all: the header shows keybindings.
        let screen = render(&app, 140, 30);
        assert!(screen.contains("quit"));
        assert!(screen.contains("mode"));
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

    fn profile(id: &str, name: &str, policy: &str) -> ProfileMeta {
        ProfileMeta {
            id: id.to_string(),
            name: name.to_string(),
            url: "https://example/sub".to_string(),
            created_ms: 1,
            updated_ms: 0,
            nodes: 3,
            protocols: "vless(2), trojan(1)".to_string(),
            policy: BTreeMap::from([("proxies".to_string(), policy.to_string())]),
        }
    }

    #[test]
    fn subscriptions_tab_lists_profiles_and_policy() {
        let mut app = app();
        app.tab = Tab::Subscriptions;
        app.profiles = vec![
            profile("s1", "alpha", "base"),
            profile("s2", "beta", "base"),
            profile("s3", "gamma", "subscription"),
        ];
        app.sub_idx = 1;

        let screen = render(&app, 100, 30);
        assert!(screen.contains("Subscriptions (3)"), "{screen}");
        assert!(screen.contains("alpha"));
        assert!(screen.contains("gamma"));
        assert!(
            screen.contains("▶ beta"),
            "selected row is marked: {screen}"
        );
        assert!(screen.contains("vless(2), trojan(1)"), "protocols show");
        assert!(screen.contains("[base]"), "remembered base policy shows");
        assert!(screen.contains("[sub]"), "remembered sub policy shows");
        assert!(screen.contains("nodes"), "column header shows");
    }

    #[test]
    fn confirm_overlay_renders_blocks_choices_and_remember_hint() {
        let mut app = app();
        app.profiles = vec![profile("s1", "alpha", "base")];
        app.pending = Some(PendingSwitch {
            profile_id: "s1".to_string(),
            diffs: vec![
                BlockDiff {
                    block: Block::Rules,
                    base: Some("1 rule".to_string()),
                    sub: Some("2 rules".to_string()),
                    equal: false,
                },
                BlockDiff {
                    block: Block::Dns,
                    base: None,
                    sub: Some("present".to_string()),
                    equal: false,
                },
            ],
            choices: BTreeMap::from([(Block::Rules, Source::Subscription)]),
            cursor: 0,
            remember: false,
        });

        let screen = render(&app, 100, 30);
        assert!(screen.contains("Confirm switch · alpha"), "{screen}");
        // The chosen side is marked on both axes: text and colour.
        assert!(screen.contains("○ 1 rule"), "{screen}");
        assert!(screen.contains("● 2 rules"), "{screen}");
        assert!(screen.contains("base"), "base column header");
        assert!(screen.contains("subscription"), "sub column header");
        assert!(screen.contains("r remember: off"), "{screen}");
        assert!(screen.contains("Esc cancel"), "{screen}");
    }

    #[test]
    fn input_overlay_renders_focused_field() {
        let mut app = app();
        app.input = Some(InputOverlay {
            url: "https://example/sub".to_string(),
            name: "my sub".to_string(),
            field: InputField::Url,
        });

        let screen = render(&app, 100, 30);
        let url_line = screen
            .lines()
            .find(|line| line.contains("https://example/sub"))
            .expect("url renders");
        let name_line = screen
            .lines()
            .find(|line| line.contains("my sub"))
            .expect("name renders");
        assert!(
            url_line.contains('▌'),
            "url line has the cursor: {url_line}"
        );
        assert!(
            !name_line.contains('▌'),
            "unfocused field has no cursor: {name_line}"
        );
        assert!(screen.contains("Tab switch field"));

        app.input.as_mut().unwrap().field = InputField::Name;
        let screen = render(&app, 100, 30);
        let url_line = screen
            .lines()
            .find(|line| line.contains("https://example/sub"))
            .unwrap();
        let name_line = screen.lines().find(|line| line.contains("my sub")).unwrap();
        assert!(!url_line.contains('▌'));
        assert!(name_line.contains('▌'), "focus moved: {name_line}");
    }

    #[test]
    fn tab_hitboxes_match_drawn_titles() {
        let app = app();
        let screen = render(&app, 100, 30);
        let first_line = screen.lines().next().expect("a header row");
        let hitboxes = tab_hitboxes(100);
        assert_eq!(hitboxes.len(), Tab::ALL.len());

        for (index, (range, tab)) in hitboxes.into_iter().enumerate() {
            let drawn: String = first_line
                .chars()
                .skip(range.start as usize)
                .take((range.end - range.start) as usize)
                .collect();
            assert_eq!(
                drawn,
                format!(" {} {} ", index + 1, tab.title()),
                "hitbox {range:?} for {tab:?}"
            );
        }
    }

    #[test]
    fn add_and_update_and_delete_hints_render() {
        let mut app = app();
        app.tab = Tab::Subscriptions;
        app.profiles = vec![profile("s1", "alpha", "base")];

        let screen = render(&app, 120, 30);
        for hint in ["a add", "u update", "d delete", "enter switch", "[/] cycle"] {
            assert!(screen.contains(hint), "missing {hint}: {screen}");
        }

        app.delete_armed = Some("s1".to_string());
        let screen = render(&app, 120, 30);
        assert!(
            screen.contains("again = delete"),
            "the second press is advertised: {screen}"
        );
    }

    #[test]
    fn settings_overlay_renders() {
        let mut app = app();
        app.settings = Some(SettingsOverlay {
            mode: "rule".into(),
            mixed_port: "7890".into(),
            allow_lan: true,
            tun: false,
            cursor: 1,
        });

        let screen = render(&app, 100, 30);
        assert!(screen.contains("Settings / Options"));
        assert!(screen.contains("Mixed Port"));
        assert!(screen.contains("7890"));
        assert!(screen.contains("[● ON ]"));
    }

    #[test]
    fn switching_tabs_clears_the_body_area() {
        let mut app = app();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

        app.tab = Tab::Overview;
        terminal
            .draw(|frame| draw(frame, &app, Instant::now()))
            .unwrap();

        app.tab = Tab::Subscriptions;
        terminal
            .draw(|frame| draw(frame, &app, Instant::now()))
            .unwrap();

        let screen = (0..terminal.backend().buffer().area.height)
            .map(|y| {
                (0..terminal.backend().buffer().area.width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(screen.contains("Subscriptions (0)"));
        assert!(screen.contains("no subscriptions yet"));
        assert!(!screen.contains("Version"));
        assert!(!screen.contains("Traffic"));
    }

    #[test]
    fn rules_overlay_renders() {
        let mut app = app();
        app.prepend_rules = vec!["DOMAIN-SUFFIX,google.com,PROXY".to_string()];
        app.append_rules = vec!["MATCH,DIRECT".to_string()];
        app.rules = Some(RulesOverlay {
            is_prepend: true,
            cursor: 0,
            adding: false,
            input_text: String::new(),
        });

        let screen = render(&app, 100, 30);
        assert!(
            screen.contains("Custom Rules Management (base.yaml)"),
            "{screen}"
        );
        assert!(screen.contains("Prepend (1)"), "{screen}");
        assert!(screen.contains("Append (1)"), "{screen}");
        assert!(
            screen.contains("DOMAIN-SUFFIX,google.com,PROXY"),
            "{screen}"
        );
        assert!(screen.contains("Tab switch category"), "{screen}");
    }

    #[test]
    fn overview_renders_curve_traffic_and_custom_rules_panel() {
        let mut app = app();
        app.prepend_rules = vec!["GEOIP,CN,DIRECT".to_string()];
        app.append_rules = vec!["MATCH,PROXY".to_string()];

        let screen = render(&app, 100, 30);
        assert!(screen.contains("Status"), "{screen}");
        assert!(screen.contains("Traffic (Realtime Curve)"), "{screen}");
        assert!(screen.contains("Custom Rules (base.yaml)"), "{screen}");
        assert!(screen.contains("Prepend Rules"), "{screen}");
        assert!(screen.contains("GEOIP,CN,DIRECT"), "{screen}");
        assert!(screen.contains("Append Rules"), "{screen}");
        assert!(screen.contains("MATCH,PROXY"), "{screen}");
    }
}
