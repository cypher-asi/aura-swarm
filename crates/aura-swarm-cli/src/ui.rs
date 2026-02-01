//! UI rendering with ratatui.
//!
//! This module implements the two-column TUI layout for the CLI.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use ratatui::Frame;

use crate::app::{App, Focus, InputMode};
use crate::markdown::render_markdown;
use crate::types::AgentState;

/// Render the UI.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Main layout: vertical split for header, main content, and status bar
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Header bar
            Constraint::Min(5),    // Main content (two columns)
            Constraint::Length(1), // Status bar
        ])
        .split(area);

    // Render header bar
    render_header_bar(frame, app, main_layout[0]);

    // Horizontal split for agents panel (left) and chat column (right)
    let content_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30), // Left: Agents
            Constraint::Percentage(70), // Right: Chat + Input
        ])
        .split(main_layout[1]);

    // Render panels
    render_agents_panel(frame, app, content_layout[0]);
    render_chat_column(frame, app, content_layout[1]);
    render_status_bar(frame, app, main_layout[2]);

    // Render modal if in special input mode
    if app.input_mode == InputMode::CreatingAgent {
        render_create_agent_dialog(frame, app, area);
    } else if app.input_mode == InputMode::ConfirmingDelete {
        render_confirm_delete_dialog(frame, app, area);
    }
}

/// Truncate a string in the middle with ellipsis if it exceeds max_len.
fn truncate_middle(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    if max_len < 5 {
        return s[..max_len].to_string();
    }
    let keep = (max_len - 3) / 2; // 3 chars for "..."
    let start = &s[..keep];
    let end = &s[s.len() - keep..];
    format!("{start}...{end}")
}

/// Render the header bar with project name and gateway status.
fn render_header_bar(frame: &mut Frame, app: &App, area: Rect) {
    // Build the gateway status text
    let gateway_url = app.gateway_url();
    let (status_text, status_style) = if app.refresh_error.is_some() {
        ("disconnected", Style::default().fg(Color::Red))
    } else {
        ("connected", Style::default().fg(Color::Green))
    };

    let title = "AURA SWARM";
    // Limit URL to max 50% of header width, reserving space for status indicator
    let max_url_width = (area.width as usize / 2).saturating_sub(15); // Reserve space for " [connected]"
    let display_url = truncate_middle(gateway_url, max_url_width);

    // Calculate right side text for spacing
    let right_text = format!("{} [{}]", display_url, status_text);

    // Build the line with left and right content
    let line = Line::from(vec![
        Span::styled(title, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(area.width.saturating_sub(title.len() as u16 + right_text.len() as u16) as usize)),
        Span::raw(&display_url),
        Span::raw(" ["),
        Span::styled(status_text, status_style),
        Span::raw("]"),
    ]);

    let header = Paragraph::new(line).style(Style::default().bg(Color::DarkGray));
    frame.render_widget(header, area);
}

/// Render the agents panel.
fn render_agents_panel(frame: &mut Frame, app: &App, area: Rect) {
    let is_focused = app.focus == Focus::Agents;

    let block = Block::default()
        .title(" Agents ")
        .borders(Borders::ALL)
        .border_style(if is_focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::Gray)
        });

    let items: Vec<ListItem> = app
        .agents
        .iter()
        .map(|agent| {
            // Show FAILED prominently when in Error state with error_message
            let (status_text, status_style) = if agent.status == AgentState::Error && agent.error_message.is_some() {
                ("FAILED".to_string(), Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
            } else {
                (agent.status.as_str().to_string(), Style::default().fg(agent.status.color()))
            };

            let name_span = Span::raw(&agent.name);
            let status_span = Span::styled(format!(" {}", status_text), status_style);

            ListItem::new(Line::from(vec![name_span, status_span]))
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut state = ListState::default();
    state.select(app.selected_agent);

    frame.render_stateful_widget(list, area, &mut state);

    // Render keybindings help at the bottom
    if is_focused && app.input_mode == InputMode::Normal {
        let help_area = Rect::new(
            area.x + 1,
            area.y + area.height.saturating_sub(2),
            area.width.saturating_sub(2),
            1,
        );

        if help_area.height > 0 && area.height > 4 {
            let help = Paragraph::new(Line::from(vec![
                Span::styled("[n]", Style::default().fg(Color::Yellow)),
                Span::raw("ew "),
                Span::styled("[d]", Style::default().fg(Color::Yellow)),
                Span::raw("el "),
                Span::styled("[s]", Style::default().fg(Color::Yellow)),
                Span::raw("tart "),
                Span::styled("[t]", Style::default().fg(Color::Yellow)),
                Span::raw("op "),
                Span::styled("[r]", Style::default().fg(Color::Yellow)),
                Span::raw("estart"),
            ]))
            .style(Style::default().fg(Color::DarkGray));

            frame.render_widget(help, help_area);
        }
    }
}

/// Horizontal padding for chat content.
const CHAT_PADDING: u16 = 2;

/// Render the right column containing chat and input as one unit.
fn render_chat_column(frame: &mut Frame, app: &App, area: Rect) {
    let is_focused = app.focus == Focus::Chat;

    // Build title (no streaming indicator here - it goes in the chat area)
    let title = if let Some(agent) = app.selected_agent() {
        if app.is_connected() {
            format!(" Chat: {} (connected) ", agent.name)
        } else {
            format!(" Chat: {} ", agent.name)
        }
    } else {
        " Chat ".to_string()
    };

    // Outer block for the entire right column
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(if is_focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::Gray)
        });

    let inner_area = block.inner(area);
    frame.render_widget(block, area);

    // Split inner area into chat messages and input
    let inner_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // Chat messages
            Constraint::Length(1), // Separator line
            Constraint::Length(1), // Input line
        ])
        .split(inner_area);

    let chat_area_full = inner_layout[0];
    let separator_area = inner_layout[1];
    let input_area = inner_layout[2];
    
    // Apply horizontal padding to chat area
    let chat_area = Rect::new(
        chat_area_full.x + CHAT_PADDING,
        chat_area_full.y,
        chat_area_full.width.saturating_sub(CHAT_PADDING * 2 + 1), // +1 for scrollbar
        chat_area_full.height,
    );

    // Check if selected agent is in Error state with error message
    if let Some(agent) = app.selected_agent() {
        if agent.status == AgentState::Error {
            if let Some(ref error) = agent.error_message {
                render_error_details(frame, error, chat_area);
                render_input_line(frame, app, separator_area, input_area, is_focused);
                return;
            }
        }
    }

    // Available width for markdown rendering (accounting for padding)
    let content_width = chat_area.width as usize;

    // Render chat messages
    if app.messages.is_empty() && !app.is_streaming {
        let help = if app.selected_agent().is_some() {
            if app.is_connected() {
                "Type a message and press Enter to send"
            } else {
                "Press Enter to connect to agent"
            }
        } else {
            "Select an agent to chat"
        };

        let text = Paragraph::new(help)
            .style(Style::default().fg(Color::DarkGray))
            .wrap(Wrap { trim: true });

        frame.render_widget(text, chat_area);
    } else {
        // Build chat text
        let mut lines: Vec<Line> = Vec::new();

        for msg in &app.messages {
            if msg.is_user() {
                // User messages: simple display with user style
                lines.push(Line::from(vec![
                    Span::styled("[You] ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    Span::styled(&msg.content, Style::default().fg(Color::White)),
                ]));
                lines.push(Line::from(""));
            } else {
                // Agent messages: render markdown
                lines.push(Line::from(vec![
                    Span::styled("[Agent]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                ]));
                
                // Render markdown content with syntax highlighting
                let md_lines = render_markdown(&msg.content, content_width);
                lines.extend(md_lines);
                lines.push(Line::from(""));
            }
        }

        // Add animated thinking indicator if streaming
        if app.is_streaming {
            // Only add a new thinking line if the last message isn't already showing streaming content
            let needs_thinking_line = app.messages.is_empty() || 
                app.messages.last().map_or(true, |m| m.is_user());
            
            if needs_thinking_line {
                lines.push(Line::from(vec![
                    Span::styled("[Agent]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                ]));
                lines.push(Line::from(vec![
                    Span::styled(app.spinner_char(), Style::default().fg(Color::Yellow)),
                    Span::styled(" thinking...", Style::default().fg(Color::DarkGray)),
                ]));
            }
        }

        let text = Text::from(lines);
        let visible_lines = chat_area.height as usize;

        // Calculate wrapped line count (accounts for text wrapping)
        let total_wrapped_lines = calculate_wrapped_line_count(&text, content_width);

        // Max scroll is how far we can scroll up from the bottom
        let max_scroll = total_wrapped_lines.saturating_sub(visible_lines);

        // Clamp chat_scroll to valid range
        let effective_scroll = app.chat_scroll.min(max_scroll);

        // scroll_offset for Paragraph: lines to skip from TOP
        // When chat_scroll = 0, we want to see the bottom -> skip max_scroll lines
        // When chat_scroll = max_scroll, we want to see the top -> skip 0 lines
        let scroll_offset = max_scroll.saturating_sub(effective_scroll);

        let paragraph = Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .scroll((scroll_offset as u16, 0));

        frame.render_widget(paragraph, chat_area);

        // Render scrollbar if content exceeds view (in full area, on right edge)
        if total_wrapped_lines > visible_lines {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"));

            // Scrollbar position: 0 = top, max_scroll = bottom
            // When effective_scroll = 0, we're at the bottom -> position = max_scroll
            // When effective_scroll = max_scroll, we're at the top -> position = 0
            let scrollbar_position = max_scroll.saturating_sub(effective_scroll);

            let mut scrollbar_state = ScrollbarState::new(total_wrapped_lines)
                .position(scrollbar_position)
                .viewport_content_length(visible_lines);

            frame.render_stateful_widget(
                scrollbar,
                chat_area_full,
                &mut scrollbar_state,
            );
        }
    }

    // Render input line
    render_input_line(frame, app, separator_area, input_area, is_focused);
}

/// Render the input line at the bottom of the chat column.
fn render_input_line(frame: &mut Frame, app: &App, separator_area: Rect, input_area: Rect, is_focused: bool) {
    // Draw separator line
    let separator = Paragraph::new("─".repeat(separator_area.width as usize))
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(separator, separator_area);

    // Don't show input content when in a modal dialog (input is used by the dialog)
    let in_modal = app.input_mode != InputMode::Normal;

    // Draw input prompt and text
    // Show different prompt based on insert mode
    let prompt = if !is_focused {
        "│ "
    } else if app.chat_insert_mode {
        "> "
    } else {
        ": "  // Command mode indicator
    };

    // Show input text only when not in a modal dialog
    let input_text = if in_modal { "" } else { app.input.as_str() };
    
    let input_line = Line::from(vec![
        Span::styled(prompt, Style::default().fg(if app.chat_insert_mode && is_focused { Color::Cyan } else { Color::DarkGray })),
        Span::styled(input_text, Style::default().fg(Color::White)),
    ]);
    let input_widget = Paragraph::new(input_line);
    frame.render_widget(input_widget, input_area);

    // Show cursor only if focused AND in insert mode AND not streaming AND not in modal
    // Hide cursor during streaming to prevent flickering
    if is_focused && app.input_mode == InputMode::Normal && app.chat_insert_mode && !app.is_streaming {
        frame.set_cursor_position((
            input_area.x + prompt.len() as u16 + app.cursor_position as u16,
            input_area.y,
        ));
    }
}

/// Render the status bar.
fn render_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    // Mode indicator (vim-like)
    let mode_indicator = if app.focus == Focus::Chat {
        if app.chat_insert_mode {
            Span::styled(" INSERT ", Style::default().fg(Color::Black).bg(Color::Green))
        } else {
            Span::styled(" NORMAL ", Style::default().fg(Color::Black).bg(Color::Blue))
        }
    } else {
        Span::styled(" AGENTS ", Style::default().fg(Color::Black).bg(Color::Magenta))
    };

    let status = if let Some(ref error) = app.error_message {
        Line::from(vec![
            mode_indicator,
            Span::styled(" ERROR: ", Style::default().fg(Color::Red).bold()),
            Span::styled(error, Style::default().fg(Color::Red)),
        ])
    } else if let Some(ref refresh_error) = app.refresh_error {
        // Show refresh errors in yellow/orange
        Line::from(vec![
            mode_indicator,
            Span::styled(" ⚠ ", Style::default().fg(Color::Yellow).bold()),
            Span::styled(refresh_error, Style::default().fg(Color::Yellow)),
        ])
    } else if let Some(ref status) = app.status_message {
        Line::from(vec![
            mode_indicator,
            Span::styled(format!(" {status}"), Style::default().fg(Color::Green)),
        ])
    } else if app.focus == Focus::Chat && app.chat_insert_mode {
        // Insert mode help
        Line::from(vec![
            mode_indicator,
            Span::raw(" "),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw(":send "),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::raw(":normal mode "),
            Span::styled("Tab", Style::default().fg(Color::Yellow)),
            Span::raw(":switch"),
        ])
    } else if app.focus == Focus::Chat {
        // Normal mode in chat
        Line::from(vec![
            mode_indicator,
            Span::raw(" "),
            Span::styled("i", Style::default().fg(Color::Yellow)),
            Span::raw(":insert "),
            Span::styled("q", Style::default().fg(Color::Yellow)),
            Span::raw(":quit "),
            Span::styled("j/k", Style::default().fg(Color::Yellow)),
            Span::raw(":scroll "),
            Span::styled("Tab", Style::default().fg(Color::Yellow)),
            Span::raw(":switch"),
        ])
    } else {
        // Agents panel help
        Line::from(vec![
            mode_indicator,
            Span::raw(" "),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw(":connect "),
            Span::styled("n", Style::default().fg(Color::Yellow)),
            Span::raw(":new "),
            Span::styled("d", Style::default().fg(Color::Yellow)),
            Span::raw(":delete "),
            Span::styled("q", Style::default().fg(Color::Yellow)),
            Span::raw(":quit "),
            Span::styled("Tab", Style::default().fg(Color::Yellow)),
            Span::raw(":switch"),
        ])
    };

    let status_bar = Paragraph::new(status).style(Style::default().bg(Color::DarkGray));

    frame.render_widget(status_bar, area);
}

/// Render the create agent dialog.
fn render_create_agent_dialog(frame: &mut Frame, app: &App, area: Rect) {
    // Calculate dialog size - use fixed minimum size for better consistency
    let dialog_width = 50.min(area.width.saturating_sub(4));
    let dialog_height = 9.min(area.height.saturating_sub(4)); // Need at least 9 lines for content

    let dialog_area = Rect::new(
        area.x + (area.width.saturating_sub(dialog_width)) / 2,
        area.y + (area.height.saturating_sub(dialog_height)) / 2,
        dialog_width,
        dialog_height,
    );

    // Clear the background
    frame.render_widget(Clear, dialog_area);

    let block = Block::default()
        .title(" Create New Agent ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Label
            Constraint::Length(1), // Spacer
            Constraint::Length(3), // Input box
            Constraint::Length(1), // Help text
        ])
        .split(inner);

    let label = Paragraph::new("Enter agent name:")
        .style(Style::default().fg(Color::White));
    frame.render_widget(label, layout[0]);

    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let input = Paragraph::new(app.input.as_str())
        .style(Style::default().fg(Color::Yellow))
        .block(input_block);
    frame.render_widget(input, layout[2]);

    let help = Paragraph::new("Press Enter to create, Esc to cancel")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(help, layout[3]);

    // Show cursor in the input field
    frame.set_cursor_position((
        layout[2].x + app.cursor_position as u16 + 1,
        layout[2].y + 1,
    ));
}

/// Render the confirm delete dialog.
fn render_confirm_delete_dialog(frame: &mut Frame, app: &App, area: Rect) {
    let dialog_area = centered_rect(50, 20, area);

    // Clear the background
    frame.render_widget(Clear, dialog_area);

    let block = Block::default()
        .title(" Confirm Delete ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red));

    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let agent_name = app
        .selected_agent()
        .map_or("?", |a| a.name.as_str());

    let text = Text::from(vec![
        Line::from(format!("Delete agent '{agent_name}'?")),
        Line::from(""),
        Line::from("This action cannot be undone."),
        Line::from(""),
        Line::from(vec![
            Span::styled("[y]", Style::default().fg(Color::Red).bold()),
            Span::raw(" Yes  "),
            Span::styled("[n]", Style::default().fg(Color::Green).bold()),
            Span::raw(" No"),
        ]),
    ]);

    let paragraph = Paragraph::new(text)
        .style(Style::default().fg(Color::White))
        .wrap(Wrap { trim: true });

    frame.render_widget(paragraph, inner);
}

/// Render error details when an agent has failed.
fn render_error_details(frame: &mut Frame, error: &str, area: Rect) {
    let lines = vec![
        Line::from(vec![
            Span::styled("Agent failed to provision", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Error: ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
        Line::from(Span::styled(error, Style::default().fg(Color::White))),
        Line::from(""),
        Line::from(""),
        Line::from(vec![
            Span::styled("[d]", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw(" Delete agent"),
        ]),
        Line::from(vec![
            Span::styled("[s]", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw(" Retry (start agent)"),
        ]),
    ];

    let text = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: true });

    frame.render_widget(text, area);
}

/// Calculate the number of visual lines after text wrapping.
///
/// This accounts for long lines that wrap to multiple visual lines.
fn calculate_wrapped_line_count(text: &Text, available_width: usize) -> usize {
    if available_width == 0 {
        return text.lines.len();
    }

    let mut total = 0;
    for line in &text.lines {
        // Calculate the display width of this line using ratatui's width method
        let line_width: usize = line.width();
        
        if line_width == 0 {
            // Empty lines still take one visual line
            total += 1;
        } else {
            // Calculate how many visual lines this text line will take
            // Round up: (line_width + available_width - 1) / available_width
            total += (line_width + available_width - 1) / available_width;
        }
    }
    total
}

/// Create a centered rectangle.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
