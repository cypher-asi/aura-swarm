//! Markdown to ratatui text conversion.
//!
//! Converts markdown text to styled ratatui Lines for terminal display
//! with syntax highlighting for code blocks.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// Convert markdown text to styled ratatui Lines.
/// 
/// `available_width` is used to properly handle code blocks - code lines
/// longer than this will be truncated rather than wrapped to avoid
/// line number overlap issues.
pub fn render_markdown(text: &str, available_width: usize) -> Vec<Line<'static>> {
    let renderer = MarkdownRenderer::new(available_width);
    renderer.render(text)
}

/// Syntax highlighter using syntect.
struct SyntaxHighlighter {
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
}

impl SyntaxHighlighter {
    fn new() -> Self {
        Self {
            syntax_set: SyntaxSet::load_defaults_newlines(),
            theme_set: ThemeSet::load_defaults(),
        }
    }

    /// Highlight code and return styled spans for each line.
    fn highlight(&self, code: &str, lang: &str) -> Vec<Vec<Span<'static>>> {
        let syntax = self
            .syntax_set
            .find_syntax_by_token(lang)
            .or_else(|| self.syntax_set.find_syntax_by_extension(lang))
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let theme = &self.theme_set.themes["base16-ocean.dark"];
        let mut highlighter = HighlightLines::new(syntax, theme);
        let mut result = Vec::new();

        for line in LinesWithEndings::from(code) {
            let mut spans = Vec::new();
            
            match highlighter.highlight_line(line, &self.syntax_set) {
                Ok(ranges) => {
                    for (style, text) in ranges {
                        let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                        let mut ratatui_style = Style::default().fg(fg);
                        
                        if style.font_style.contains(FontStyle::BOLD) {
                            ratatui_style = ratatui_style.add_modifier(Modifier::BOLD);
                        }
                        if style.font_style.contains(FontStyle::ITALIC) {
                            ratatui_style = ratatui_style.add_modifier(Modifier::ITALIC);
                        }
                        if style.font_style.contains(FontStyle::UNDERLINE) {
                            ratatui_style = ratatui_style.add_modifier(Modifier::UNDERLINED);
                        }
                        
                        // Remove trailing newline from text
                        let text = text.trim_end_matches('\n').trim_end_matches('\r');
                        if !text.is_empty() {
                            spans.push(Span::styled(text.to_string(), ratatui_style));
                        }
                    }
                }
                Err(_) => {
                    // Fallback to plain text
                    let text = line.trim_end_matches('\n').trim_end_matches('\r');
                    spans.push(Span::styled(
                        text.to_string(),
                        Style::default().fg(Color::Yellow),
                    ));
                }
            }
            
            result.push(spans);
        }

        result
    }
}

/// Markdown renderer state.
struct MarkdownRenderer {
    lines: Vec<Line<'static>>,
    current_spans: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    in_code_block: bool,
    code_block_content: String,
    code_block_lang: Option<String>,
    list_depth: usize,
    ordered_list_index: Option<u64>,
    available_width: usize,
    highlighter: SyntaxHighlighter,
}

impl MarkdownRenderer {
    fn new(available_width: usize) -> Self {
        Self {
            lines: Vec::new(),
            current_spans: Vec::new(),
            style_stack: vec![Style::default()],
            in_code_block: false,
            code_block_content: String::new(),
            code_block_lang: None,
            list_depth: 0,
            ordered_list_index: None,
            available_width,
            highlighter: SyntaxHighlighter::new(),
        }
    }

    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, modifier: Modifier) {
        let new_style = self.current_style().add_modifier(modifier);
        self.style_stack.push(new_style);
    }

    fn push_color_style(&mut self, fg: Color) {
        let new_style = self.current_style().fg(fg);
        self.style_stack.push(new_style);
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn flush_line(&mut self) {
        if !self.current_spans.is_empty() {
            let spans = std::mem::take(&mut self.current_spans);
            self.lines.push(Line::from(spans));
        }
        // Don't add empty lines here - use add_blank_line() for intentional blank lines
    }

    fn add_blank_line(&mut self) {
        self.flush_line();
        self.lines.push(Line::from(""));
    }

    fn add_text(&mut self, text: &str) {
        if self.in_code_block {
            self.code_block_content.push_str(text);
            return;
        }

        let style = self.current_style();
        
        // Handle newlines within text - preserve blank lines
        let parts: Vec<&str> = text.split('\n').collect();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                // Flush current line first
                self.flush_line();
                // If this part is empty, it means there was a blank line (consecutive newlines)
                // Add an explicit blank line to preserve spacing
                if part.is_empty() && i < parts.len() - 1 {
                    self.lines.push(Line::from(""));
                }
            }
            
            if !part.is_empty() {
                self.current_spans.push(Span::styled((*part).to_string(), style));
            }
        }
    }

    fn render_code_block(&mut self) {
        let content = std::mem::take(&mut self.code_block_content);
        let lang = self.code_block_lang.take().unwrap_or_default();

        // Ensure we're on a fresh line
        self.flush_line();

        let gutter_style = Style::default().fg(Color::DarkGray);
        let line_num_style = Style::default().fg(Color::Rgb(100, 100, 100));
        
        // Calculate dimensions
        let code_lines: Vec<&str> = content.lines().collect();
        let num_lines = code_lines.len();
        let show_line_nums = num_lines > 1;
        let line_num_width = if show_line_nums {
            num_lines.to_string().len()
        } else {
            0
        };
        
        // Calculate prefix width: "│ " (2) + line_num + " │ " (3) or just "│ " (2)
        let prefix_width = if show_line_nums {
            2 + line_num_width + 3
        } else {
            4 // "│ " + "  " for single line padding
        };
        
        // Max code width before truncation
        let max_code_width = self.available_width.saturating_sub(prefix_width + 2); // -2 for safety margin

        // Language header if present
        if !lang.is_empty() {
            self.lines.push(Line::from(vec![
                Span::styled("┌─ ", gutter_style),
                Span::styled(lang.clone(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled(" ", gutter_style),
                Span::styled("─".repeat(self.available_width.saturating_sub(lang.len() + 5).min(40)), gutter_style),
            ]));
        } else {
            self.lines.push(Line::from(vec![
                Span::styled("┌", gutter_style),
                Span::styled("─".repeat(self.available_width.saturating_sub(2).min(44)), gutter_style),
            ]));
        }

        // Get syntax highlighted lines
        let highlighted_lines = self.highlighter.highlight(&content, &lang);

        // Render each line
        for (i, highlighted_spans) in highlighted_lines.iter().enumerate() {
            let mut spans = Vec::new();
            
            // Left border
            spans.push(Span::styled("│ ", gutter_style));
            
            // Line number (right-aligned)
            if show_line_nums {
                spans.push(Span::styled(
                    format!("{:>width$}", i + 1, width = line_num_width),
                    line_num_style,
                ));
                spans.push(Span::styled(" │ ", gutter_style));
            }
            
            // Calculate current code line width
            let code_width: usize = highlighted_spans.iter().map(|s| s.content.len()).sum();
            
            if code_width <= max_code_width {
                // Fits - add all spans
                spans.extend(highlighted_spans.iter().cloned());
            } else {
                // Truncate - need to cut spans to fit
                let mut remaining = max_code_width.saturating_sub(1); // -1 for ellipsis
                for span in highlighted_spans {
                    if remaining == 0 {
                        break;
                    }
                    if span.content.len() <= remaining {
                        spans.push(span.clone());
                        remaining -= span.content.len();
                    } else {
                        // Truncate this span
                        let truncated: String = span.content.chars().take(remaining).collect();
                        spans.push(Span::styled(truncated, span.style));
                        remaining = 0;
                    }
                }
                spans.push(Span::styled("…", Style::default().fg(Color::DarkGray)));
            }
            
            self.lines.push(Line::from(spans));
        }

        // Bottom border
        self.lines.push(Line::from(vec![
            Span::styled("└", gutter_style),
            Span::styled("─".repeat(self.available_width.saturating_sub(2).min(44)), gutter_style),
        ]));
        
        // Add blank line after code block
        self.lines.push(Line::from(""));
    }

    fn render(mut self, text: &str) -> Vec<Line<'static>> {
        let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
        let parser = Parser::new_ext(text, options);

        for event in parser {
            match event {
                Event::Start(tag) => self.handle_start_tag(tag),
                Event::End(tag) => self.handle_end_tag(tag),
                Event::Text(text) => self.add_text(&text),
                Event::Code(code) => {
                    // Inline code
                    self.current_spans.push(Span::styled(
                        format!("`{code}`"),
                        Style::default().fg(Color::Yellow).bg(Color::Rgb(40, 40, 40)),
                    ));
                }
                Event::SoftBreak => {
                    // In terminal, treat soft breaks as actual line breaks for readability
                    // (standard markdown would convert these to spaces)
                    self.flush_line();
                }
                Event::HardBreak => {
                    self.flush_line();
                }
                Event::Rule => {
                    self.flush_line();
                    self.lines.push(Line::from(Span::styled(
                        "─".repeat(self.available_width.min(60)),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                _ => {}
            }
        }

        // Flush any remaining content
        self.flush_line();

        // Remove trailing empty lines
        while self.lines.last().map_or(false, |l| l.spans.is_empty()) {
            self.lines.pop();
        }

        self.lines
    }

    fn handle_start_tag(&mut self, tag: Tag) {
        match tag {
            Tag::Heading { level, .. } => {
                // Add blank line before headers (except at start)
                if !self.lines.is_empty() || !self.current_spans.is_empty() {
                    self.add_blank_line();
                }
                
                let prefix = match level {
                    pulldown_cmark::HeadingLevel::H1 => "# ",
                    pulldown_cmark::HeadingLevel::H2 => "## ",
                    pulldown_cmark::HeadingLevel::H3 => "### ",
                    _ => "#### ",
                };
                
                self.current_spans.push(Span::styled(
                    prefix.to_string(),
                    Style::default().fg(Color::Magenta),
                ));
                self.push_style(Modifier::BOLD);
                self.push_color_style(Color::Magenta);
            }
            Tag::Paragraph => {
                // Add spacing between paragraphs
                if !self.lines.is_empty() && !self.current_spans.is_empty() {
                    self.flush_line();
                }
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.current_spans.push(Span::styled(
                    "│ ".to_string(),
                    Style::default().fg(Color::Blue),
                ));
                self.push_color_style(Color::Blue);
            }
            Tag::CodeBlock(kind) => {
                self.in_code_block = true;
                self.code_block_content.clear();
                self.code_block_lang = match kind {
                    CodeBlockKind::Fenced(lang) => {
                        let lang = lang.to_string();
                        if lang.is_empty() { None } else { Some(lang) }
                    }
                    CodeBlockKind::Indented => None,
                };
            }
            Tag::List(first_item) => {
                self.list_depth += 1;
                self.ordered_list_index = first_item;
                if self.list_depth == 1 && !self.lines.is_empty() {
                    self.flush_line();
                }
            }
            Tag::Item => {
                // Don't flush here - it's handled by TagEnd::Item or TagEnd::List
                let indent = "  ".repeat(self.list_depth.saturating_sub(1));
                let bullet = if let Some(idx) = self.ordered_list_index.as_mut() {
                    let bullet = format!("{indent}{}. ", *idx);
                    *idx += 1;
                    bullet
                } else {
                    format!("{indent}• ")
                };
                self.current_spans.push(Span::styled(
                    bullet,
                    Style::default().fg(Color::Cyan),
                ));
            }
            Tag::Emphasis => {
                self.push_style(Modifier::ITALIC);
            }
            Tag::Strong => {
                self.push_style(Modifier::BOLD);
            }
            Tag::Strikethrough => {
                self.push_style(Modifier::CROSSED_OUT);
            }
            Tag::Link { dest_url, .. } => {
                self.push_style(Modifier::UNDERLINED);
                self.push_color_style(Color::Blue);
                // Store URL for later (we'll show it after the link text)
                self.current_spans.push(Span::raw("")); // Placeholder
                let _ = dest_url; // We'll show URL after link text
            }
            _ => {}
        }
    }

    fn handle_end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.pop_style(); // color
                self.pop_style(); // bold
                self.flush_line();
            }
            TagEnd::Paragraph => {
                self.add_blank_line();
            }
            TagEnd::BlockQuote(_) => {
                self.pop_style();
                self.flush_line();
            }
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                self.render_code_block();
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.list_depth = self.list_depth.saturating_sub(1);
                if self.list_depth == 0 {
                    self.ordered_list_index = None;
                    // Add blank line after top-level list
                    self.add_blank_line();
                }
            }
            TagEnd::Item => {
                // Flush current item content
                self.flush_line();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.pop_style();
            }
            TagEnd::Link => {
                self.pop_style(); // color
                self.pop_style(); // underline
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_text() {
        let lines = render_markdown("Hello world", 80);
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_code_block() {
        let md = "```rust\nfn main() {}\n```";
        let lines = render_markdown(md, 80);
        assert!(lines.len() > 1);
    }

    #[test]
    fn test_bold_italic() {
        let md = "**bold** and *italic*";
        let lines = render_markdown(md, 80);
        assert!(!lines.is_empty());
    }
}
