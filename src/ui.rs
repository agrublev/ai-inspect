//! ratatui rendering. Left: active session list. Right: detail for the
//! selected session — header, token/cost stats, tool timeline, live feed.

use chrono::Local;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use serde_json::Value;

use crate::app::{App, GroupInfo, Row, SummaryState, TracePane};
use crate::cost;
use crate::session::{Event, EventKind, Session, Status};

/// iris brand magenta (#c678dd) — the primary color of the docs/landing palette.
/// An exact RGB value so the accent matches the site regardless of terminal theme.
const PRIMARY: Color = Color::Rgb(0xc6, 0x78, 0xdd);

/// Icon, color, and short label for a session's current status.
pub fn status_glyph(s: &Session) -> (&'static str, Color, String) {
    match s.status() {
        Status::NeedsApproval => {
            let label = match &s.pending_tool {
                Some(t) => format!("NEEDS APPROVAL · {t}"),
                None => "NEEDS APPROVAL".into(),
            };
            ("⚠", Color::Red, label)
        }
        Status::Working => {
            let label = match &s.pending_tool {
                Some(t) => format!("running · {t}"),
                None => "working".into(),
            };
            ("●", Color::Green, label)
        }
        Status::Done => ("✓", Color::Blue, "done · awaiting you".into()),
        Status::Idle => ("○", Color::DarkGray, "idle".into()),
    }
}

pub fn short_model(model: Option<&str>) -> String {
    let m = model.unwrap_or("?");
    m.strip_prefix("claude-")
        .unwrap_or(m)
        .split('[')
        .next()
        .unwrap_or(m)
        .to_string()
}

pub fn human_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

pub fn draw(f: &mut Frame, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_header(f, app, root[0]);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(root[1]);

    draw_list(f, app, cols[0]);
    draw_detail(f, app, cols[1]);
    draw_footer(f, app, root[2]);

    if app.popup_open {
        draw_summary_popup(f, app);
    }
    if app.approve_open {
        draw_approval_popup(f, app);
    }
    if app.editing_key {
        draw_key_popup(f, app);
    }
    if app.install_open {
        draw_install_popup(f, app);
    }
    if app.trace_browser_open {
        draw_trace_browser(f, app);
    }
}

fn draw_install_popup(f: &mut Frame, app: &App) {
    let area = centered_rect(72, 46, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" trace capture + approvals ")
        .border_style(Style::new().fg(Color::Cyan));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    let (body, hint) = if app.hook_installed {
        (
            vec![
                Line::from(Span::styled(
                    "Trace capture hooks are installed.",
                    Style::new().fg(Color::Green).bold(),
                )),
                Line::from(""),
                Line::from(
                    "Prompts, model responses, tool calls/results, and lifecycle events are \
                     stored locally in ~/.claude/iris/traces.db (0600); nothing is uploaded. \
                     Approval gating remains off unless you arm it with A.",
                ),
                Line::from(""),
                Line::from(Span::styled(
                    "Remove Iris capture and approval hooks?",
                    Style::new().fg(Color::DarkGray),
                )),
            ],
            Line::from(vec![
                Span::styled(" x ", Style::new().fg(Color::Black).bg(Color::Red).bold()),
                Span::styled(" disable    ", Style::new().fg(Color::Red)),
                Span::styled(" Esc ", Style::new().fg(Color::Black).bg(Color::DarkGray)),
                Span::styled(" keep enabled", Style::new().fg(Color::DarkGray)),
            ]),
        )
    } else {
        (
            vec![
                Line::from(Span::styled(
                    "Install local trace capture hooks for Claude Code?",
                    Style::new().fg(Color::Reset).bold(),
                )),
                Line::from(""),
                Line::from(
                    "Iris records prompts, model output, tool inputs/results, skills, and \
                     subagent lifecycle locally in ~/.claude/iris/traces.db (0600). No data \
                     leaves this machine. The approval hook is also installed, but it only \
                     intercepts calls after you arm gating with A. Restart sessions to apply.",
                ),
                Line::from(""),
                Line::from(Span::styled(
                    "Enable local capture? (Approval gating stays opt-in.)",
                    Style::new().fg(Color::DarkGray),
                )),
            ],
            Line::from(vec![
                Span::styled(" a ", Style::new().fg(Color::Black).bg(Color::Green).bold()),
                Span::styled(" accept / enable    ", Style::new().fg(Color::Green)),
                Span::styled(" r ", Style::new().fg(Color::Black).bg(Color::DarkGray)),
                Span::styled(" refuse", Style::new().fg(Color::DarkGray)),
            ]),
        )
    };
    f.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), rows[0]);
    f.render_widget(Paragraph::new(hint), rows[1]);
}

fn draw_trace_browser(f: &mut Frame, app: &App) {
    let area = f.area();
    f.render_widget(Clear, area);
    let count = app.trace_rows.len();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " trace explorer · {count} {} ",
            if count == 1 { "trace" } else { "traces" }
        ))
        .border_style(Style::new().fg(PRIMARY));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
        .split(rows[0]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(columns[1]);

    let trace_border = if app.trace_pane == TracePane::Traces {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let trace_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " {}traces · newest first ",
            if app.trace_pane == TracePane::Traces {
                "▶ "
            } else {
                ""
            }
        ))
        .border_style(Style::new().fg(trace_border));
    if app.trace_rows.is_empty() {
        f.render_widget(
            Paragraph::new("No traces yet. Install capture hooks or run `iris traces` to import existing transcripts.")
                .wrap(Wrap { trim: false })
                .block(trace_block),
            columns[0],
        );
    } else {
        let items = app
            .trace_rows
            .iter()
            .map(|trace| {
                let timestamp = trace["started_at"].as_str().unwrap_or("unknown time");
                let status = trace["status"].as_str().unwrap_or("unknown");
                let count = trace["event_count"].as_u64().unwrap_or(0);
                let input_max = (columns[0].width as usize).saturating_sub(16).max(8);
                let input = short_trace_value(&trace["input"], input_max);
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(
                            format!("{} ", trace_state_glyph(status)),
                            Style::new().fg(trace_state_color(status)),
                        ),
                        Span::styled(
                            timestamp.chars().take(19).collect::<String>(),
                            Style::new().fg(Color::Reset),
                        ),
                    ]),
                    Line::from(vec![
                        Span::styled(
                            format!("{count:>3} events  "),
                            Style::new().fg(Color::DarkGray),
                        ),
                        Span::styled(input, Style::new().fg(Color::Reset)),
                    ]),
                ])
            })
            .collect::<Vec<_>>();
        let list = List::new(items)
            .block(trace_block)
            .highlight_symbol("▸ ")
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD));
        let mut state = ListState::default();
        state.select(Some(app.trace_selected));
        f.render_stateful_widget(list, columns[0], &mut state);
    }

    let event_border = if app.trace_pane == TracePane::Events {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let trace_id = app
        .trace_rows
        .get(app.trace_selected)
        .and_then(|trace| trace["trace_id"].as_str())
        .unwrap_or("select a trace");
    let event_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " {}timeline · {} events · {} ",
            if app.trace_pane == TracePane::Events {
                "▶ "
            } else {
                ""
            },
            app.trace_events.len(),
            short_text(trace_id, 12)
        ))
        .border_style(Style::new().fg(event_border));
    if app.trace_events.is_empty() {
        f.render_widget(
            Paragraph::new("Select a trace to inspect its event timeline.")
                .style(Style::new().fg(Color::DarkGray))
                .block(event_block),
            right[0],
        );
    } else {
        let items = app
            .trace_events
            .iter()
            .map(|event| {
                let timestamp = event["timestamp"].as_str().unwrap_or("?");
                let kind = event["event_type"].as_str().unwrap_or("event");
                let name = event["name"].as_str().unwrap_or("");
                let status = event["status"].as_str().unwrap_or("?");
                let name_max = (right[0].width as usize).saturating_sub(40).clamp(8, 24);
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{} ", clock_time(timestamp)),
                        Style::new().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        format!("{:<16}", short_text(kind, 16)),
                        Style::new().fg(Color::Cyan),
                    ),
                    Span::styled(short_text(name, name_max), Style::new().fg(Color::Reset)),
                    Span::styled(
                        format!(" [{status}]"),
                        Style::new().fg(trace_state_color(status)),
                    ),
                ]))
            })
            .collect::<Vec<_>>();
        let list = List::new(items)
            .block(event_block)
            .highlight_symbol("▸ ")
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD));
        let mut state = ListState::default();
        state.select(Some(app.trace_event_selected));
        f.render_stateful_widget(list, right[0], &mut state);
    }

    let payload_border = if app.trace_pane == TracePane::Payload {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let payload_title = app
        .selected_trace_event()
        .and_then(|event| event["event_type"].as_str())
        .unwrap_or("event payload");
    let payload = app
        .selected_trace_event()
        .and_then(|event| serde_json::to_string_pretty(event).ok())
        .unwrap_or_else(|| "No event selected.".to_string());
    let payload_width = right[1].width.saturating_sub(2).max(1);
    let payload_height = right[1].height.saturating_sub(2);
    let payload_lines = wrapped_line_count(&payload, payload_width);
    let max_scroll = payload_lines
        .saturating_sub(payload_height as usize)
        .min(u16::MAX as usize) as u16;
    app.set_trace_payload_max_scroll(max_scroll);
    f.render_widget(
        Paragraph::new(payload)
            .scroll((app.trace_payload_scroll(), 0))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(
                        " {}payload · {} ",
                        if app.trace_pane == TracePane::Payload {
                            "▶ "
                        } else {
                            ""
                        },
                        short_text(payload_title, 26)
                    ))
                    .border_style(Style::new().fg(payload_border)),
            ),
        right[1],
    );

    let hint = Line::from(vec![
        Span::styled(" Tab ", Style::new().fg(Color::Black).bg(Color::DarkGray)),
        Span::styled(" pane   ", Style::new().fg(Color::DarkGray)),
        Span::styled(" j/k ", Style::new().fg(Color::Black).bg(Color::DarkGray)),
        Span::styled(" move   ", Style::new().fg(Color::DarkGray)),
        Span::styled(" Enter ", Style::new().fg(Color::Black).bg(Color::DarkGray)),
        Span::styled(" next   ", Style::new().fg(Color::DarkGray)),
        Span::styled(" r ", Style::new().fg(Color::Black).bg(Color::DarkGray)),
        Span::styled(" refresh   ", Style::new().fg(Color::DarkGray)),
        Span::styled(" Esc/q ", Style::new().fg(Color::Black).bg(Color::DarkGray)),
        Span::styled(" close", Style::new().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(hint), rows[1]);
}

fn trace_state_glyph(status: &str) -> &'static str {
    match status {
        "running" => "●",
        "error" | "failed" => "✗",
        "ok" | "complete" => "✓",
        _ => "○",
    }
}

fn trace_state_color(status: &str) -> Color {
    match status {
        "running" => Color::Yellow,
        "error" | "failed" => Color::Red,
        "ok" | "complete" => Color::Green,
        _ => Color::DarkGray,
    }
}

fn short_trace_value(value: &Value, max: usize) -> String {
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
        .replace('\n', " ");
    short_text(&text, max)
}

fn short_text(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars()
            .take(max.saturating_sub(1))
            .chain(['…'])
            .collect()
    }
}

fn clock_time(timestamp: &str) -> &str {
    timestamp
        .split_once('T')
        .map(|(_, time)| time.get(..8).unwrap_or(time))
        .unwrap_or_else(|| timestamp.get(..8).unwrap_or(timestamp))
}

fn wrapped_line_count(text: &str, width: u16) -> usize {
    let width = usize::from(width.max(1));
    text.lines()
        .map(|line| Line::from(line).width().max(1).div_ceil(width))
        .sum()
}

fn draw_approval_popup(f: &mut Frame, app: &App) {
    let p = match app.current_pending() {
        Some(p) => p,
        None => return,
    };
    let area = centered_rect(76, 70, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" approve — {} ", p.tool_name))
        .border_style(Style::new().fg(Color::Red));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // cwd
            Constraint::Length(5), // AI risk assessment
            Constraint::Min(0),    // full input
            Constraint::Length(1), // hints
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("dir ", Style::new().fg(Color::DarkGray)),
            Span::styled(p.cwd.clone(), Style::new().fg(Color::Cyan)),
        ])),
        rows[0],
    );

    let assess = match app.current_assessment() {
        Some(SummaryState::Done(t)) => {
            Paragraph::new(t.clone()).style(Style::new().fg(Color::Reset))
        }
        Some(SummaryState::Loading) => {
            Paragraph::new("assessing risk…").style(Style::new().fg(Color::Yellow))
        }
        Some(SummaryState::Error(e)) => {
            Paragraph::new(format!("risk check failed: {e}")).style(Style::new().fg(Color::Red))
        }
        None => {
            Paragraph::new("press x for an AI risk read").style(Style::new().fg(Color::DarkGray))
        }
    };
    let assess_block = Block::default()
        .borders(Borders::ALL)
        .title(" risk ")
        .border_style(Style::new().fg(Color::DarkGray));
    f.render_widget(
        assess.block(assess_block).wrap(Wrap { trim: false }),
        rows[1],
    );

    let input_block = Block::default()
        .borders(Borders::TOP)
        .title(" tool input ")
        .border_style(Style::new().fg(Color::DarkGray));
    f.render_widget(
        Paragraph::new(p.input.clone())
            .style(Style::new().fg(Color::Reset))
            .block(input_block)
            .wrap(Wrap { trim: false }),
        rows[2],
    );

    let hint = Line::from(vec![
        Span::styled(" a ", Style::new().fg(Color::Black).bg(Color::Green).bold()),
        Span::styled(" allow   ", Style::new().fg(Color::Green)),
        Span::styled(" d ", Style::new().fg(Color::Black).bg(Color::Red).bold()),
        Span::styled(" deny   ", Style::new().fg(Color::Red)),
        Span::styled(" x ", Style::new().fg(Color::Black).bg(Color::DarkGray)),
        Span::styled(" risk   ", Style::new().fg(Color::DarkGray)),
        Span::styled(" Esc ", Style::new().fg(Color::Black).bg(Color::DarkGray)),
        Span::styled(" close", Style::new().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(hint), rows[3]);
}

fn draw_key_popup(f: &mut Frame, app: &App) {
    let area = centered_rect(64, 30, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" set ANTHROPIC API key ")
        .border_style(Style::new().fg(Color::Cyan));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Mask the key — show a dot per character plus a cursor.
    let masked: String = "•".repeat(app.key_buffer.chars().count());
    let lines = vec![
        Line::from(Span::styled(
            "paste or type your key, then Enter:",
            Style::new().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            format!("{masked}▏"),
            Style::new().fg(Color::Reset),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Enter save · Esc cancel · stored 0600 in ~/.claude/iris/api_key",
            Style::new().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_summary_popup(f: &mut Frame, app: &App) {
    let area = centered_rect(70, 60, f.area());
    f.render_widget(Clear, area);

    let title = match app.selected_session() {
        Some(s) => format!(" summary · {} ", s.project()),
        None => " summary ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::new().fg(PRIMARY));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    let body: Paragraph = match app.selected_summary() {
        Some(SummaryState::Done(text)) => Paragraph::new(summary_lines(text)),
        Some(SummaryState::Loading) => {
            let via = if app.has_api_key() {
                format!("{} API", short_model(Some(crate::anthropic::SUMMARY_MODEL)))
            } else {
                "claude CLI".to_string()
            };
            Paragraph::new(format!("generating summary via {via}…"))
                .style(Style::new().fg(Color::Yellow))
        }
        Some(SummaryState::Error(e)) => {
            Paragraph::new(format!("error: {e}")).style(Style::new().fg(Color::Red))
        }
        None => Paragraph::new("no session selected").style(Style::new().fg(Color::DarkGray)),
    };
    f.render_widget(body.wrap(Wrap { trim: false }), rows[0]);

    let hint = Line::from(vec![
        Span::styled(" g ", Style::new().fg(Color::Black).bg(Color::DarkGray)),
        Span::styled(" regenerate   ", Style::new().fg(Color::DarkGray)),
        Span::styled(" Esc ", Style::new().fg(Color::Black).bg(Color::DarkGray)),
        Span::styled(" close", Style::new().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(hint), rows[1]);
}

/// Render the model's `DOING / DONE / NEXT` briefing as styled lines. Each
/// section gets an icon + colored heading echoing the status palette (green =
/// in-progress, blue = done, yellow = where it's headed), with continuation
/// lines bulleted and indented under their heading so multi-line DONE/NEXT
/// blocks stay readable. Anything that doesn't match a section is shown plain.
fn summary_lines(text: &str) -> Vec<Line<'static>> {
    // icon, heading color, body style — keyed by the section label.
    let section = |word: &str| -> Option<(&'static str, Color, Style)> {
        match word {
            "DOING" => Some(("●", Color::Green, Style::new().fg(Color::Reset).bold())),
            "DONE" => Some(("✓", Color::Blue, Style::new().fg(Color::Reset))),
            "NEXT" => Some((
                "→",
                Color::Yellow,
                Style::new().fg(Color::Reset).add_modifier(Modifier::ITALIC),
            )),
            _ => None,
        }
    };

    let mut lines: Vec<Line> = Vec::new();
    // Body style of the section we're currently under, for continuation lines.
    let mut cur: Option<Style> = None;
    let mut first = true;

    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }

        // A "LABEL:" prefix (or bare "LABEL") starts a new section heading.
        let head = trimmed
            .split_once(':')
            .map(|(w, rest)| (w.trim(), rest.trim()))
            .or(Some((trimmed, "")))
            .and_then(|(w, rest)| section(&w.to_uppercase()).map(|s| (w, rest, s)));

        if let Some((label, rest, (icon, hcolor, body_style))) = head {
            // Blank spacer between sections (but not before the first).
            if !first {
                lines.push(Line::from(""));
            }
            first = false;
            cur = Some(body_style);

            let mut spans = vec![
                Span::styled(format!("{icon} "), Style::new().fg(hcolor).bold()),
                Span::styled(
                    label.to_uppercase(),
                    Style::new()
                        .fg(hcolor)
                        .bold()
                        .add_modifier(Modifier::UNDERLINED),
                ),
            ];
            if !rest.is_empty() {
                spans.push(Span::styled(format!("  {rest}"), body_style));
            }
            lines.push(Line::from(spans));
        } else {
            // Continuation line — bullet + indent under the active heading.
            let style = cur.unwrap_or_else(|| Style::new().fg(Color::Reset));
            lines.push(Line::from(vec![
                Span::styled("   • ", Style::new().fg(Color::DarkGray)),
                Span::styled(
                    trimmed
                        .trim_start_matches(['-', '•', '*'])
                        .trim()
                        .to_string(),
                    style,
                ),
            ]));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            text.to_string(),
            Style::new().fg(Color::Reset),
        )));
    }
    lines
}

/// A rect centered in `area`, sized as a percentage of width/height.
fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let now = Local::now().format("%H:%M:%S").to_string();
    let count = app.visible.len();
    let npending = app.pending.len();

    // Flash and warnings go right after the badge so they never clip off-screen.
    let mut spans = vec![Span::styled(
        " iris ",
        Style::new().fg(Color::Black).bg(PRIMARY).bold(),
    )];

    if let Some(status) = &app.flash {
        spans.push(Span::styled(
            format!(" {status} "),
            Style::new().fg(Color::Black).bg(Color::Green).bold(),
        ));
    }

    let ngroups = app
        .rows
        .iter()
        .filter(|r| matches!(r, crate::app::Row::Group(_)))
        .count();
    let active = if ngroups > 0 {
        format!("  {count} active · {ngroups} groups")
    } else {
        format!("  {count} active")
    };
    spans.push(Span::styled(active, Style::new().fg(Color::Reset).bold()));

    // Pending approvals — red when there are any, so it's obvious a/d can act.
    let pend_style = if npending > 0 {
        Style::new().fg(Color::Black).bg(Color::Red).bold()
    } else {
        Style::new().fg(Color::DarkGray)
    };
    spans.push(Span::styled(format!("  pending {npending} "), pend_style));

    if !app.hook_installed {
        spans.push(Span::styled(
            "  ⚑ approvals off — press i to enable",
            Style::new().fg(Color::Yellow).bold(),
        ));
    } else if app.gating {
        spans.push(Span::styled(
            "  ⚡ gating ARMED — A to disarm",
            Style::new().fg(Color::Black).bg(Color::Yellow).bold(),
        ));
    } else {
        spans.push(Span::styled(
            "  ⚐ passive — A to arm gating",
            Style::new().fg(Color::DarkGray),
        ));
    }
    if app.hook_installed && !app.has_api_key() {
        spans.push(Span::styled(
            "  · no API key — AI via claude CLI (K to set one)",
            Style::new().fg(Color::Yellow),
        ));
    }

    // Aggregate estimated cost of everything on screen — the number nobody
    // else shows you.
    let total = app.visible_cost();
    if total > 0.0 {
        spans.push(Span::styled("  ~cost ", Style::new().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!("${total:.2}"),
            Style::new().fg(Color::Green).bold(),
        ));
    }

    spans.push(Span::styled(
        format!("  · {}m · {now}", app.window.as_secs() / 60),
        Style::new().fg(Color::DarkGray),
    ));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_list(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" sessions ")
        .border_style(Style::new().fg(Color::DarkGray));

    if app.rows.is_empty() {
        let p = Paragraph::new("no active sessions in window")
            .style(Style::new().fg(Color::DarkGray))
            .block(block);
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| match row {
            Row::Group(g) => group_item(g),
            Row::Session { path, grouped } => {
                let s = match app.session_at(path) {
                    Some(s) => s,
                    None => return ListItem::new(""),
                };
                session_item(app, s, *grouped)
            }
        })
        .collect();

    // Reverse-video highlight adapts to any terminal theme (light or dark)
    // instead of a fixed dark bar that vanishes on a light background.
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD));

    let mut state = ListState::default();
    state.select(Some(app.selected));
    f.render_stateful_widget(list, area, &mut state);
}

/// A one-line project header with a fold glyph and per-status counts.
fn group_item(g: &GroupInfo) -> ListItem<'static> {
    let glyph = if g.collapsed { "▸" } else { "▾" };
    // Urgency tints the header so a folded group still signals it needs you.
    let hcolor = if g.pending > 0 || g.needs > 0 {
        Color::Red
    } else if g.working > 0 {
        Color::Green
    } else if g.done > 0 {
        Color::Blue
    } else {
        Color::DarkGray
    };

    let mut spans = vec![
        Span::styled(format!("{glyph} "), Style::new().fg(hcolor).bold()),
        Span::styled(g.key.clone(), Style::new().fg(Color::Cyan).bold()),
        Span::styled(format!(" ({})", g.count), Style::new().fg(Color::DarkGray)),
        Span::raw("  "),
    ];
    let mut counts: Vec<Span> = Vec::new();
    if g.pending > 0 {
        counts.push(Span::styled(
            format!("⚠{} ", g.pending),
            Style::new().fg(Color::Red).bold(),
        ));
    } else if g.needs > 0 {
        counts.push(Span::styled(
            format!("⚠{} ", g.needs),
            Style::new().fg(Color::Red).bold(),
        ));
    }
    if g.working > 0 {
        counts.push(Span::styled(
            format!("●{} ", g.working),
            Style::new().fg(Color::Green),
        ));
    }
    if g.done > 0 {
        counts.push(Span::styled(
            format!("✓{} ", g.done),
            Style::new().fg(Color::Blue),
        ));
    }
    if g.idle > 0 {
        counts.push(Span::styled(
            format!("○{}", g.idle),
            Style::new().fg(Color::DarkGray),
        ));
    }
    spans.extend(counts);
    if g.collapsed && !g.lead_label.is_empty() {
        spans.push(Span::styled(
            format!("  {}", g.lead_label),
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    ListItem::new(Line::from(spans))
}

/// A compact single-line session row. Indented when it sits under a group.
fn session_item(app: &App, s: &Session, grouped: bool) -> ListItem<'static> {
    // A live hook approval overrides the transcript-derived status.
    let (icon, color, state) = match app.pending.get(&s.id) {
        Some(p) => ("⚠", Color::Red, format!("APPROVE {}", p.tool_name)),
        None => status_glyph(s),
    };
    let indent = if grouped { "  " } else { "" };
    let mut spans = vec![
        Span::styled(format!("{indent}{icon} "), Style::new().fg(color).bold()),
        Span::styled(clamp(&s.label(), 30), Style::new().fg(Color::Reset)),
        Span::styled(format!("  {state}"), Style::new().fg(color).bold()),
    ];
    // For ungrouped (singleton) rows, show the project for context.
    if !grouped {
        spans.push(Span::styled(
            format!(" · {}", s.project()),
            Style::new().fg(Color::Cyan),
        ));
    }
    spans.push(Span::styled(
        format!(" · {}", short_model(s.model.as_deref())),
        Style::new().fg(PRIMARY),
    ));
    spans.push(Span::styled(
        format!(" · {}", human_tokens(s.usage.total())),
        Style::new().fg(Color::DarkGray),
    ));
    ListItem::new(Line::from(spans))
}

/// Clamp to `max` chars on one line, appending `…` when truncated.
fn clamp(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).chain(['…']).collect()
    }
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    // A cyan border + title signals the pane is "entered" for scrolling.
    let (title, border) = if app.focused {
        (" detail · SCROLLING ", Color::Cyan)
    } else {
        (" detail ", Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::new().fg(border));

    let s = match app.selected_session() {
        Some(s) => s,
        None => {
            // A group header is selected — show the group overview instead.
            if let Some(g) = app.selected_group() {
                let inner = block.inner(area);
                f.render_widget(block, area);
                draw_group_detail(f, app, g, inner);
            } else {
                f.render_widget(block, area);
            }
            return;
        }
    };

    let inner = block.inner(area);
    f.render_widget(block, area);

    let pending = app.pending.get(&s.id);
    let banner_rows = if pending.is_some() { 4 } else { 0 };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(banner_rows), // pending approval banner
            Constraint::Length(2),           // header (title + path)
            Constraint::Length(2),           // stats
            Constraint::Length(tool_rows(s)),
            Constraint::Min(0), // feed
        ])
        .split(inner);

    if let Some(p) = pending {
        let banner = vec![
            Line::from(Span::styled(
                {
                    let dir = p.cwd.rsplit('/').next().unwrap_or(&p.cwd);
                    format!("⚠ PENDING APPROVAL — {} in {dir}", p.tool_name)
                },
                Style::new().fg(Color::Red).bold(),
            )),
            Line::from(Span::styled(p.brief.clone(), Style::new().fg(Color::Reset))),
            Line::from(vec![
                Span::styled(" a ", Style::new().fg(Color::Black).bg(Color::Green).bold()),
                Span::styled(" allow    ", Style::new().fg(Color::Green)),
                Span::styled(" d ", Style::new().fg(Color::Black).bg(Color::Red).bold()),
                Span::styled(" deny", Style::new().fg(Color::Red)),
            ]),
        ];
        f.render_widget(Paragraph::new(banner).wrap(Wrap { trim: false }), rows[0]);
    }

    // header
    let branch = s
        .branch
        .as_deref()
        .map(|b| format!("  @{b}"))
        .unwrap_or_default();
    let (icon, scolor, state) = status_glyph(s);
    let head = vec![
        Line::from(vec![
            Span::styled(format!("{icon} "), Style::new().fg(scolor).bold()),
            Span::styled(s.label(), Style::new().fg(Color::Reset).bold()),
            Span::styled(format!("   [{state}]"), Style::new().fg(scolor).bold()),
        ]),
        Line::from(vec![
            Span::styled(
                s.cwd.clone().unwrap_or_else(|| "?".into()),
                Style::new().fg(Color::Cyan),
            ),
            Span::styled(branch, Style::new().fg(Color::Green)),
        ]),
    ];
    f.render_widget(Paragraph::new(head), rows[1]);

    // stats
    let u = &s.usage;
    let est = cost::estimate(u, s.model.as_deref());
    let stats = vec![
        Line::from(vec![
            Span::styled("model ", Style::new().fg(Color::DarkGray)),
            Span::styled(short_model(s.model.as_deref()), Style::new().fg(PRIMARY)),
            Span::styled("   turns ", Style::new().fg(Color::DarkGray)),
            Span::styled(s.assistant_turns.to_string(), Style::new().fg(Color::Reset)),
            Span::styled("   ~cost ", Style::new().fg(Color::DarkGray)),
            Span::styled(format!("${est:.2}"), Style::new().fg(Color::Green).bold()),
        ]),
        Line::from(vec![
            Span::styled("in ", Style::new().fg(Color::DarkGray)),
            Span::raw(human_tokens(u.input)),
            Span::styled("  out ", Style::new().fg(Color::DarkGray)),
            Span::raw(human_tokens(u.output)),
            Span::styled("  cache w/r ", Style::new().fg(Color::DarkGray)),
            Span::raw(format!(
                "{}/{}",
                human_tokens(u.cache_creation),
                human_tokens(u.cache_read)
            )),
        ]),
    ];
    f.render_widget(Paragraph::new(stats), rows[2]);

    draw_tools(f, s, rows[3]);
    draw_feed(f, app, s, rows[4]);
}

/// Detail pane for a selected (folded or open) project group: a header line
/// with aggregate counts, then one compact line per session in the group.
fn draw_group_detail(f: &mut Frame, app: &App, g: &GroupInfo, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    let fold = if g.collapsed { "folded" } else { "open" };
    let head = vec![
        Line::from(vec![
            Span::styled("group ", Style::new().fg(Color::DarkGray)),
            Span::styled(g.key.clone(), Style::new().fg(Color::Cyan).bold()),
            Span::styled(
                format!("   {} sessions · {fold}", g.count),
                Style::new().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!("⚠ {} pending   ", g.pending),
                Style::new().fg(Color::Red).bold(),
            ),
            Span::styled(
                format!("● {} working   ", g.working),
                Style::new().fg(Color::Green),
            ),
            Span::styled(
                format!("✓ {} done   ", g.done),
                Style::new().fg(Color::Blue),
            ),
            Span::styled(
                format!("○ {} idle", g.idle),
                Style::new().fg(Color::DarkGray),
            ),
        ]),
        Line::from(Span::styled(
            "⏎/␣ fold/unfold   a/d approve/deny all pending in group",
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )),
    ];
    f.render_widget(Paragraph::new(head), rows[0]);

    let cap = rows[1].height as usize;
    let lines: Vec<Line> = app
        .group_sessions(&g.key)
        .into_iter()
        .take(cap)
        .map(|s| {
            let (icon, color, state) = match app.pending.get(&s.id) {
                Some(p) => ("⚠", Color::Red, format!("APPROVE {}", p.tool_name)),
                None => status_glyph(s),
            };
            Line::from(vec![
                Span::styled(format!("{icon} "), Style::new().fg(color).bold()),
                Span::styled(clamp(&s.label(), 34), Style::new().fg(Color::Reset)),
                Span::styled(format!("  {state}"), Style::new().fg(color)),
                Span::styled(
                    format!("  · {}", human_tokens(s.usage.total())),
                    Style::new().fg(Color::DarkGray),
                ),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), rows[1]);
}

/// One row for a "tools" heading plus up to a few tool bars.
fn tool_rows(s: &Session) -> u16 {
    if s.tool_counts.is_empty() {
        0
    } else {
        1 + s.tool_counts.len().min(6) as u16
    }
}

fn draw_tools(f: &mut Frame, s: &Session, area: Rect) {
    if s.tool_counts.is_empty() || area.height == 0 {
        return;
    }
    let max = s.tool_counts.values().copied().max().unwrap_or(1).max(1);
    let mut pairs: Vec<(&String, &u64)> = s.tool_counts.iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(a.1));

    let mut lines = vec![Line::from(Span::styled(
        "tool calls",
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::UNDERLINED),
    ))];
    for (name, n) in pairs.into_iter().take(6) {
        let width = 18usize;
        let filled = ((*n as f64 / max as f64) * width as f64).round() as usize;
        let bar: String = "█".repeat(filled) + &"░".repeat(width - filled);
        lines.push(Line::from(vec![
            Span::styled(format!("{name:<14}"), Style::new().fg(Color::Yellow)),
            Span::styled(bar, Style::new().fg(Color::Yellow)),
            Span::styled(format!(" {n}"), Style::new().fg(Color::Reset)),
        ]));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_feed(f: &mut Frame, app: &App, s: &Session, area: Rect) {
    let total = s.events.len();
    let inner_h = area.height.saturating_sub(1) as usize; // minus the TOP border

    // Title doubles as a scroll read-out once the pane is entered.
    let title = if app.focused {
        let pos = if total == 0 { 0 } else { app.feed_cursor() + 1 };
        format!(" activity  {pos}/{total}  ▲▼ j/k · ^d/^u · gg/G ")
    } else if total > inner_h {
        format!(" activity  (latest {inner_h}/{total} · ⏎ to scroll) ")
    } else {
        " activity ".to_string()
    };
    let border = if app.focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let block = Block::default()
        .borders(Borders::TOP)
        .title(title)
        .border_style(Style::new().fg(border));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Stash the real viewport height so half/full-page motions size correctly.
    app.set_feed_viewport(inner.height);
    if inner.height == 0 {
        return;
    }

    let items: Vec<ListItem> = s
        .events
        .iter()
        .map(|e| ListItem::new(event_line(e)))
        .collect();
    let mut list = List::new(items);
    // A reverse-video current line gives the vim "cursor" feel while entered.
    if app.focused {
        list = list.highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    }

    if app.focused {
        // Persisted state: ratatui keeps the cursor visible and the offset
        // smooth across frames.
        let mut state = app.feed_state_mut();
        f.render_stateful_widget(list, inner, &mut state);
    } else {
        // Not entered: just show the tail (park selection on the last line).
        let mut state = ListState::default();
        state.select(total.checked_sub(1));
        f.render_stateful_widget(list, inner, &mut state);
    }
}

fn event_line(e: &Event) -> Line<'static> {
    let ts =
        e.ts.map(|t| t.format("%H:%M:%S").to_string())
            .unwrap_or_else(|| "--:--:--".into());
    // Each action gets its own icon, accent color, and — crucially — its own
    // text typography, so the feed reads at a glance: your prompts are bold,
    // Claude's thinking is dim italic, tool commands are italic, results are
    // muted (errors stay red), and Claude's replies are plain body text.
    let (icon, color, label, text): (&str, Color, String, Style) = match &e.kind {
        EventKind::Prompt => (
            "▸",
            Color::Cyan,
            "you".into(),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        EventKind::Assistant => (
            "✷",
            Color::Green,
            "claude".into(),
            Style::new().fg(Color::Reset),
        ),
        EventKind::Thinking => (
            "·",
            Color::DarkGray,
            "think".into(),
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ),
        EventKind::Tool(name) => (
            "⚒",
            Color::Yellow,
            name.clone(),
            Style::new()
                .fg(Color::Reset)
                .add_modifier(Modifier::ITALIC | Modifier::DIM),
        ),
        EventKind::ToolResult { error } => {
            if *error {
                (
                    "✗",
                    Color::Red,
                    "result".into(),
                    Style::new().fg(Color::Red),
                )
            } else {
                (
                    "←",
                    Color::Green,
                    "result".into(),
                    Style::new().fg(Color::Reset).add_modifier(Modifier::DIM),
                )
            }
        }
    };
    Line::from(vec![
        Span::styled(format!("{ts} "), Style::new().fg(Color::DarkGray)),
        Span::styled(format!("{icon} "), Style::new().fg(color)),
        Span::styled(
            format!("{label} "),
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(e.text.clone(), text),
    ])
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let chip = |k: &'static str| Span::styled(k, Style::new().fg(Color::Black).bg(Color::DarkGray));
    let txt = |t: &'static str| Span::styled(t, Style::new().fg(Color::DarkGray));

    // In scroll mode the keys are repurposed for navigating the feed.
    let line = if app.focused {
        Line::from(vec![
            chip(" j/k "),
            txt(" scroll  "),
            chip(" g/G "),
            txt(" top/bottom  "),
            chip(" PgUp/PgDn "),
            txt(" page  "),
            chip(" s "),
            txt(" summary  "),
            chip(" t "),
            txt(" traces  "),
            chip(" a/d "),
            txt(" allow/deny  "),
            chip(" Esc/h "),
            txt(" back  "),
            chip(" q "),
            txt(" quit"),
        ])
    } else {
        Line::from(vec![
            chip(" j/k "),
            txt(" move  "),
            chip(" ⏎/l "),
            txt(" enter  "),
            chip(" ␣ "),
            txt(" fold  "),
            chip(" z "),
            txt(" fold all  "),
            chip(" a/d "),
            txt(" allow/deny  "),
            chip(" D "),
            txt(" delete  "),
            chip(" A "),
            txt(" gate  "),
            chip(" s "),
            txt(" summary  "),
            chip(" i "),
            txt(" hooks  "),
            chip(" K "),
            txt(" key  "),
            chip(" t "),
            txt(" traces  "),
            chip(" q "),
            txt(" quit"),
        ])
    };
    f.render_widget(Paragraph::new(line), area);
}
