//! Rendering for the deploy TUI (ratatui).
//!
//! Layout while a script runs:
//! - left column: the curated step-progress feed (styled `_lib.sh` events),
//! - right column: top = raw live command output, bottom = infra-state snapshot.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use ratatui::Frame;

use crate::app::{App, Focus, ProgressItem, Screen};

// Oxide cloud-shell inspired palette: green-on-dark.
/// Near-black navy, the main canvas background.
const BG: Color = Color::Rgb(11, 14, 20);
/// Slightly lifted panel background for header/footer/title strips.
const BG_PANEL: Color = Color::Rgb(18, 22, 30);
/// Green-tinted selection row background.
const BG_SEL: Color = Color::Rgb(28, 40, 34);
/// Spring-green accent (replaces the old cyan everywhere).
const ACCENT: Color = Color::Rgb(63, 207, 142);
/// Brighter green for progress-bar fill and active accents.
const ACCENT_BRIGHT: Color = Color::Rgb(74, 222, 128);
/// Primary foreground text.
const TEXT: Color = Color::Rgb(228, 231, 236);
/// Muted secondary text.
const DIM: Color = Color::Rgb(110, 118, 132);
/// Muted foreground for streamed command output and infra-state tables — the
/// neutral base the coordinated accents sit on top of.
const MUTED: Color = Color::Rgb(176, 182, 194);
/// Sky blue for informational accents (matches the curated "info" lines).
const INFO: Color = Color::Rgb(96, 165, 250);
/// Amber for warnings / transient states.
const WARN: Color = Color::Rgb(250, 204, 21);
/// Red for errors / failed states.
const ERR: Color = Color::Rgb(248, 113, 113);

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Top-level render entry point.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Paint the whole canvas so unused cells use our dark navy, not the
    // terminal default background.
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(5),    // body
            Constraint::Length(1), // footer
        ])
        .split(area);

    render_header(frame, app, rows[0]);
    match app.screen {
        Screen::Picker => render_picker(frame, app, rows[1]),
        Screen::Running | Screen::Finished => render_run(frame, app, rows[1]),
    }
    render_footer(frame, app, rows[2]);
}

// ---------------------------------------------------------------------------
// Header / footer
// ---------------------------------------------------------------------------

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let (status, status_style) = match app.screen {
        Screen::Picker => (
            "select a script".to_string(),
            Style::default().fg(DIM),
        ),
        Screen::Running => {
            let frame_i = SPINNER[app.spinner_tick % SPINNER.len()];
            (
                format!("{frame_i} running  {}", app.elapsed()),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )
        }
        Screen::Finished => match app.exit_code {
            Some(0) => (
                format!("✓ done  {}", app.elapsed()),
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            Some(code) => (
                format!("✗ failed (exit {code})  {}", app.elapsed()),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            None => (
                format!("✗ cancelled  {}", app.elapsed()),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        },
    };

    let script = app.running_script.as_deref().unwrap_or("");

    // Left label: "AURA SWARM DEPLOY". Right side shows the active script as a
    // green underlined "tab" (echoing the WEB1 tab in the reference) plus
    // run status.
    let label_len = "AURA SWARM  DEPLOY ".len();
    let tab = if script.is_empty() {
        String::new()
    } else {
        format!(" {script} ")
    };
    let status_len = status.chars().count();
    let pad = (area.width as usize)
        .saturating_sub(label_len + tab.chars().count() + status_len + 2);

    let mut spans = vec![
        Span::styled(
            " AURA SWARM",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  DEPLOY ",
            Style::default().fg(DIM).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(pad)),
    ];
    if !tab.is_empty() {
        spans.push(Span::styled(
            tab,
            Style::default()
                .fg(ACCENT_BRIGHT)
                .bg(BG_SEL)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ));
        spans.push(Span::raw("  "));
    }
    spans.push(Span::styled(status, status_style));
    spans.push(Span::raw(" "));

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG_PANEL).fg(TEXT)),
        area,
    );
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let hints = match app.screen {
        Screen::Picker => {
            if app.editing_args {
                "type args   Enter: confirm   Esc: cancel"
            } else {
                "↑/↓: select   Enter: run   e: edit args   q: quit"
            }
        }
        Screen::Running => "Tab: focus pane   ↑/↓ j/k: scroll   PgUp/PgDn   End: tail   Esc: cancel   q: quit",
        Screen::Finished => "Enter/Esc: back to scripts   ↑/↓: scroll   q: quit",
    };
    frame.render_widget(
        Paragraph::new(Line::from(hint_spans(hints))).style(Style::default().bg(BG_PANEL)),
        area,
    );
}

/// Build footer hint spans, highlighting the key (text before each `:`) in the
/// accent color for a shell-prompt feel: `Enter: run   q: quit`.
fn hint_spans(hints: &str) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw(" ")];
    for (i, segment) in hints.split("   ").enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", Style::default().fg(DIM)));
        }
        match segment.split_once(':') {
            Some((key, rest)) => {
                spans.push(Span::styled(
                    key.to_string(),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    format!(":{rest}"),
                    Style::default().fg(DIM),
                ));
            }
            None => spans.push(Span::styled(
                segment.to_string(),
                Style::default().fg(DIM),
            )),
        }
    }
    spans
}

// ---------------------------------------------------------------------------
// Picker
// ---------------------------------------------------------------------------

fn render_picker(frame: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    let items: Vec<ListItem> = app
        .scripts
        .iter()
        .map(|s| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<26}", s.file_name),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(s.title.clone(), Style::default().fg(DIM)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .title(Span::styled(
                    " Deploy scripts ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .padding(Padding::horizontal(1)),
        )
        .highlight_style(
            Style::default()
                .bg(BG_SEL)
                .fg(ACCENT_BRIGHT)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("❯ ");

    let mut state = ListState::default();
    if !app.scripts.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, cols[0], &mut state);

    let args_title = if app.editing_args {
        " Arguments (editing) "
    } else {
        " Arguments "
    };
    let args_text = if app.editing_args {
        format!("{}\u{2588}", app.args_input)
    } else if app.args_input.is_empty() {
        "(none — press e to add, e.g. --ref master)".to_string()
    } else {
        app.args_input.clone()
    };
    let args_style = if app.args_input.is_empty() && !app.editing_args {
        Style::default().fg(DIM)
    } else {
        Style::default().fg(TEXT)
    };
    let prompt_style = Style::default()
        .fg(if app.editing_args { ACCENT_BRIGHT } else { DIM })
        .add_modifier(Modifier::BOLD);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("❯ ", prompt_style),
            Span::styled(args_text, args_style),
        ]))
        .block(
            Block::default()
                .title(Span::styled(
                    args_title,
                    Style::default().fg(if app.editing_args { ACCENT } else { DIM }),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if app.editing_args { ACCENT } else { DIM })),
        ),
        cols[1],
    );
}

// ---------------------------------------------------------------------------
// Running / finished two-column view
// ---------------------------------------------------------------------------

fn render_run(frame: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(area);

    render_progress(frame, app, cols[0]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(cols[1]);

    render_raw(frame, app, right[0]);
    render_snapshot(frame, app, right[1]);
}

fn render_progress(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Progress;
    let running = app.screen == Screen::Running;

    // Separate each new phase/section with a blank line so steps breathe.
    let mut lines: Vec<Line> = Vec::with_capacity(app.progress.len());
    for item in &app.progress {
        if matches!(item.level.as_str(), "banner" | "section") && !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(style_progress(item));
    }
    if running {
        let spin = SPINNER[app.spinner_tick % SPINNER.len()];
        let bar_len = 18usize;
        // Indeterminate fill that cycles, echoing the reference progress bar.
        let filled = (app.spinner_tick % (bar_len + 1)).min(bar_len);
        lines.push(Line::from(vec![
            Span::styled(format!("{spin} working "), Style::default().fg(ACCENT)),
            Span::styled("█".repeat(filled), Style::default().fg(ACCENT_BRIGHT)),
            Span::styled(
                "░".repeat(bar_len.saturating_sub(filled)),
                Style::default().fg(DIM),
            ),
        ]));
    }

    let title = if running {
        let spin = SPINNER[app.spinner_tick % SPINNER.len()];
        format!(" {spin} Progress ")
    } else {
        " Progress ".to_string()
    };

    let height = area.height.saturating_sub(2);
    let scroll = scroll_offset(lines.len(), height, app.progress_scroll);

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(pane_block(&title, focused))
            .scroll((scroll, 0)),
        area,
    );
}

fn render_raw(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Raw;
    let lines: Vec<Line> = app.raw.iter().map(|l| style_raw_line(l)).collect();

    let height = area.height.saturating_sub(2);
    let scroll = scroll_offset(lines.len(), height, app.raw_scroll);

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(pane_block(" Live output ", focused))
            .scroll((scroll, 0)),
        area,
    );
}

fn render_snapshot(frame: &mut Frame, app: &App, area: Rect) {
    let when = app
        .snapshot_at
        .map_or_else(|| "polling…".to_string(), |t| t.format("%H:%M:%S").to_string());
    let title = format!(" Infra state · {when} ");

    let body: Text = if app.snapshot.is_empty() {
        Text::from(Line::from(Span::styled(
            "Waiting for first snapshot…",
            Style::default().fg(DIM),
        )))
    } else {
        Text::from(app.snapshot.lines().map(style_snapshot_line).collect::<Vec<_>>())
    };

    frame.render_widget(
        Paragraph::new(body)
            .block(pane_block(&title, false))
            .wrap(Wrap { trim: false }),
        area,
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let border = if focused { ACCENT } else { DIM };
    Block::default()
        .title(Span::styled(
            title.to_string(),
            Style::default()
                .fg(if focused { ACCENT } else { DIM })
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .padding(Padding::horizontal(1))
}

/// First visible line index so that scrolling counts up from the bottom
/// (`scroll_from_bottom == 0` follows the tail).
fn scroll_offset(total: usize, height: u16, scroll_from_bottom: usize) -> u16 {
    let height = height as usize;
    let max_top = total.saturating_sub(height);
    let top = max_top.saturating_sub(scroll_from_bottom);
    u16::try_from(top).unwrap_or(u16::MAX)
}

/// Style one curated progress item into a renderable line.
fn style_progress(item: &ProgressItem) -> Line<'static> {
    let bold = Modifier::BOLD;
    match item.level.as_str() {
        "banner" => Line::from(vec![
            Span::styled(
                format!("▌ Step {} ", item.step),
                Style::default().fg(ACCENT).add_modifier(bold),
            ),
            Span::styled(
                item.text.clone(),
                Style::default().fg(TEXT).add_modifier(bold),
            ),
        ]),
        "section" => Line::from(vec![
            Span::styled("▌ ", Style::default().fg(ACCENT).add_modifier(bold)),
            Span::styled(
                item.text.clone(),
                Style::default().fg(ACCENT).add_modifier(bold),
            ),
        ]),
        "ok" => status_line("✓", ACCENT, &item.text, false),
        "step_ok" => status_line("✓", ACCENT_BRIGHT, &item.text, true),
        "info" => status_line("ℹ", INFO, &item.text, false),
        "warn" => status_line("⚠", WARN, &item.text, false),
        "err" => status_line("✗", ERR, &item.text, false),
        "step_fail" => status_line("✗", ERR, &item.text, true),
        "cmd" => Line::from(vec![
            Span::styled("  ├ ", Style::default().fg(DIM)),
            Span::styled("$ ", Style::default().fg(ACCENT)),
            Span::styled(item.text.clone(), Style::default().fg(DIM)),
        ]),
        // detail / progress / kv and anything else: dim tree continuation.
        _ => Line::from(vec![
            Span::styled("  ├ ", Style::default().fg(DIM)),
            Span::styled(item.text.clone(), Style::default().fg(DIM)),
        ]),
    }
}

fn status_line(icon: &str, color: Color, text: &str, bold: bool) -> Line<'static> {
    let mut icon_style = Style::default().fg(color);
    let mut text_style = Style::default().fg(TEXT);
    if bold {
        icon_style = icon_style.add_modifier(Modifier::BOLD);
        text_style = text_style.add_modifier(Modifier::BOLD);
    }
    Line::from(vec![
        Span::styled(format!("{icon} "), icon_style),
        Span::styled(text.to_string(), text_style),
    ])
}

/// Style one raw "Live output" line so it speaks the same colour language as the
/// curated progress pane: leading curated icons (`✓ ⚠ ✗ ℹ ◆ ▌ $`) keep their
/// semantic colour, otherwise lines are tinted by severity keyword, falling back
/// to a neutral muted grey.
fn style_raw_line(raw: &str) -> Line<'static> {
    let trimmed = raw.trim_start();
    let indent = &raw[..raw.len() - trimmed.len()];

    if let Some((color, bold)) = leading_icon_color(trimmed) {
        let icon = trimmed.chars().next().unwrap_or(' ');
        let rest = &trimmed[icon.len_utf8()..];
        let mut icon_style = Style::default().fg(color);
        if bold {
            icon_style = icon_style.add_modifier(Modifier::BOLD);
        }
        let rest_style = Style::default().fg(if bold { TEXT } else { MUTED });
        return Line::from(vec![
            Span::raw(indent.to_string()),
            Span::styled(icon.to_string(), icon_style),
            Span::styled(rest.to_string(), rest_style),
        ]);
    }

    let color = severity_color(trimmed).unwrap_or(MUTED);
    Line::from(Span::styled(raw.to_string(), Style::default().fg(color)))
}

/// Map a line's leading curated icon to `(colour, bold)`, if present.
fn leading_icon_color(s: &str) -> Option<(Color, bool)> {
    match s.chars().next()? {
        '✓' | '$' => Some((ACCENT, false)),
        '✗' => Some((ERR, true)),
        '⚠' => Some((WARN, false)),
        'ℹ' => Some((INFO, false)),
        '◆' | '▌' | '●' | '►' => Some((ACCENT, true)),
        _ => None,
    }
}

/// Tint a plain (icon-less) output line by severity keyword.
fn severity_color(s: &str) -> Option<Color> {
    const FAIL: [&str; 7] = [
        "error", "failed", "failure", "fatal", "denied", "panic", "exception",
    ];
    const CAUTION: [&str; 3] = ["warning", "warn:", "deprecated"];
    let lower = s.to_ascii_lowercase();
    if FAIL.iter().any(|k| lower.contains(k)) {
        Some(ERR)
    } else if CAUTION.iter().any(|k| lower.contains(k)) {
        Some(WARN)
    } else {
        None
    }
}

/// Style one infra-state line: kubectl column headers in accent, the
/// "API not reachable" explainer in amber, and status tokens (`Running`,
/// `Pending`, `CrashLoopBackOff`, ready ratios like `1/1`) coloured in place
/// while preserving the table's column spacing.
fn style_snapshot_line(line: &str) -> Line<'static> {
    let trimmed = line.trim_start();

    if trimmed.starts_with("NAME") || trimmed.starts_with("NAMESPACE") {
        return Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    }
    if trimmed.starts_with("Kubernetes API not reachable") {
        return Line::from(Span::styled(line.to_string(), Style::default().fg(WARN)));
    }
    if trimmed.starts_with('(') || trimmed.starts_with("Infra snapshots") || trimmed.starts_with("Override") {
        return Line::from(Span::styled(line.to_string(), Style::default().fg(DIM)));
    }

    let spans = chunks_keep_ws(line)
        .into_iter()
        .map(|chunk| {
            if chunk.trim().is_empty() {
                Span::raw(chunk.to_string())
            } else {
                let style = token_color(chunk)
                    .map_or_else(|| Style::default().fg(MUTED), |c| Style::default().fg(c));
                Span::styled(chunk.to_string(), style)
            }
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

/// Colour a single table token by Kubernetes status semantics, or `None` to
/// leave it neutral. Ready ratios (`x/y`) are green when full, red when zero,
/// amber otherwise.
fn token_color(tok: &str) -> Option<Color> {
    if let Some((a, b)) = tok.split_once('/') {
        if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
            return Some(if b > 0 && a == b {
                ACCENT
            } else if a == 0 {
                ERR
            } else {
                WARN
            });
        }
    }
    match tok {
        "Running" | "Ready" | "Active" | "Bound" | "True" | "Completed" | "Succeeded"
        | "Healthy" => Some(ACCENT),
        "Pending" | "ContainerCreating" | "PodInitializing" | "Terminating" | "Init"
        | "Unknown" | "SchedulingDisabled" => Some(WARN),
        "NotReady" | "Error" | "CrashLoopBackOff" | "ImagePullBackOff" | "ErrImagePull"
        | "Failed" | "Evicted" | "OOMKilled" | "BackOff" | "InvalidImageName" | "False" => {
            Some(ERR)
        }
        _ => None,
    }
}

/// Split a string into alternating runs of whitespace and non-whitespace,
/// preserving every character so column alignment is kept intact.
fn chunks_keep_ws(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev_ws: Option<bool> = None;
    for (i, c) in s.char_indices() {
        let ws = c.is_whitespace();
        if prev_ws.is_some_and(|p| p != ws) {
            out.push(&s[start..i]);
            start = i;
        }
        prev_ws = Some(ws);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}
