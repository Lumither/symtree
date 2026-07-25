//! Low-level, `App`-free rendering toolkit: styles, the symbol-kind palette,
//! tree glyphs, layout math, text width/truncation, and match highlighting.
//! Everything here is a pure function of its arguments.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::model::{SymbolKind, SymbolNode};
use crate::tree::VisibleNode;
use crate::tui::theme;

pub(in crate::tui) fn status_bar_style() -> Style {
    Style::default().bg(theme::SELECTION_BG)
}

/// Terminal column width of `text`, accounting for wide (CJK/emoji) and
/// zero-width glyphs — not the scalar count, which is wrong for non-ASCII.
pub(in crate::tui) fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub(in crate::tui) fn truncate_left(text: &str, max_width: usize) -> String {
    if display_width(text) <= max_width {
        return text.to_string();
    }

    if max_width == 0 {
        return String::new();
    }

    if max_width == 1 {
        return "~".to_string();
    }

    // Keep the rightmost glyphs that fit in `max_width - 1` columns, leaving one
    // column for the leading "~" truncation marker. Width-aware so a wide glyph
    // can't straddle the budget.
    let budget = max_width - 1;
    let mut width = 0;
    let mut tail: Vec<char> = Vec::new();
    for ch in text.chars().rev() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > budget {
            break;
        }
        width += w;
        tail.push(ch);
    }
    tail.reverse();
    format!("~{}", tail.into_iter().collect::<String>())
}

pub(in crate::tui) fn centered_rect(area: Rect, width_percent: u16, height: u16) -> Rect {
    let width = area
        .width
        .saturating_mul(width_percent)
        .saturating_div(100)
        .min(area.width.saturating_sub(2))
        .max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;

    Rect {
        x,
        y,
        width,
        height,
    }
}

pub(in crate::tui) fn kind_style(kind: SymbolKind) -> Style {
    use crate::model::LspSymbolKind as K;
    let lsp = match kind {
        SymbolKind::File => return Style::default().fg(Color::Cyan).bold(),
        SymbolKind::Lsp(lsp) => lsp,
    };
    // Exhaustive on purpose: adding an LSP kind forces a color decision here.
    match lsp {
        K::File => Style::default().fg(Color::Cyan).bold(),
        K::Module | K::Namespace | K::Package => Style::default().fg(Color::Magenta),
        K::Class | K::Enum | K::Interface | K::Struct | K::TypeParameter => {
            Style::default().fg(Color::Yellow).bold()
        }
        K::Method | K::Function => Style::default().fg(Color::Green).bold(),
        K::Property | K::Field | K::EnumMember => Style::default().fg(Color::Blue),
        K::Variable | K::Constant => Style::default().fg(Color::LightRed),
        K::Constructor
        | K::String
        | K::Number
        | K::Boolean
        | K::Array
        | K::Object
        | K::Key
        | K::Null
        | K::Event
        | K::Operator
        | K::Unknown => Style::default().fg(Color::Gray),
    }
}

pub(in crate::tui) fn marker_color(node: &SymbolNode) -> Color {
    if node.has_children() {
        Color::Cyan
    } else {
        Color::DarkGray
    }
}

pub(in crate::tui) fn selected_row_style() -> Style {
    Style::default().bg(theme::SELECTION_BG)
}

pub(in crate::tui) fn row_style(style: Style, selected: bool) -> Style {
    if selected {
        style.patch(selected_row_style())
    } else {
        style
    }
}

pub(in crate::tui) fn match_style(base: Style) -> Style {
    base.fg(Color::Yellow)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}

pub(in crate::tui) fn push_match_spans<'a>(
    spans: &mut Vec<Span<'a>>,
    text: &str,
    filter: &str,
    base_style: Style,
) {
    let Some((start, end)) = match_range(text, filter) else {
        spans.push(Span::styled(text.to_string(), base_style));
        return;
    };

    if start > 0 {
        spans.push(Span::styled(text[..start].to_string(), base_style));
    }
    spans.push(Span::styled(
        text[start..end].to_string(),
        match_style(base_style),
    ));
    if end < text.len() {
        spans.push(Span::styled(text[end..].to_string(), base_style));
    }
}

pub(in crate::tui) fn match_range(text: &str, filter: &str) -> Option<(usize, usize)> {
    let filter = filter.trim();
    if filter.is_empty() {
        return None;
    }

    let text_chars = text.char_indices().collect::<Vec<_>>();
    let filter_chars = filter.chars().collect::<Vec<_>>();
    if filter_chars.is_empty() || filter_chars.len() > text_chars.len() {
        return None;
    }

    for start in 0..=text_chars.len() - filter_chars.len() {
        let matched = filter_chars
            .iter()
            .enumerate()
            .all(|(offset, filter_char)| {
                text_chars[start + offset]
                    .1
                    .eq_ignore_ascii_case(filter_char)
            });
        if matched {
            let start_byte = text_chars[start].0;
            let end_index = start + filter_chars.len();
            let end_byte = text_chars
                .get(end_index)
                .map_or(text.len(), |(byte_index, _)| *byte_index);
            return Some((start_byte, end_byte));
        }
    }

    None
}

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::tui) struct Glyphs;

impl Glyphs {
    pub(in crate::tui) fn new() -> Self {
        Self
    }

    pub(in crate::tui) fn connector(self, visible: &VisibleNode) -> String {
        if visible.depth == 0 {
            return String::new();
        }

        let mut connector = String::new();
        for ancestor_is_last in visible
            .ancestor_is_last
            .iter()
            .skip(1)
            .take(visible.depth.saturating_sub(1))
        {
            connector.push_str(if *ancestor_is_last { "  " } else { "│ " });
        }

        connector.push_str(if visible.is_last { "└─" } else { "├─" });
        connector
    }

    pub(in crate::tui) fn marker(self, node: &SymbolNode) -> &'static str {
        if !node.has_children() {
            return " ";
        }

        if node.expanded { "▾" } else { "▸" }
    }

    pub(in crate::tui) fn selection_line(self) -> Line<'static> {
        Line::from(Span::styled(
            "▌ ",
            selected_row_style().fg(Color::Cyan).bold(),
        ))
    }
}
