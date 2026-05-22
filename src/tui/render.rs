use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::border,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};
use std::time::Instant;

use crate::{
    model::{SymbolKind, SymbolNode},
    tree::{VisibleNode, get_node},
};

use super::preview::{build_preview_lines, viewport_range};
use super::{App, GlyphMode, LOADING_INDICATOR_DELAY, Mode, PreviewRequest};

const SCROLLOFF: usize = 5;

pub(super) fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < 40 || area.height < 8 {
        render_too_small(frame, area);
        return;
    }

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(area);

    render_body(frame, app, sections[0]);
    render_footer(frame, app, sections[1]);

    if app.show_help {
        render_help(frame, app, area);
    }
}

pub(super) fn render_too_small(frame: &mut Frame, area: Rect) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new("symtree needs at least 40 columns and 8 rows"),
        area,
    );
}

pub(super) fn status_line(app: &App, width: u16) -> Line<'static> {
    let width = usize::from(width);
    let stats = format!(
        "files {} symbols {} visible {}",
        app.project.file_count(),
        app.project.symbol_count(),
        app.visible_nodes().len()
    );
    let stats_width = display_width(&stats);
    let path_width = width.saturating_sub(stats_width.saturating_add(1));
    let path = truncate_left(&app.project.root.display().to_string(), path_width);
    let gap = width
        .saturating_sub(display_width(&path).saturating_add(stats_width))
        .max(1);

    let mut spans = vec![
        Span::styled(path, status_bar_style()),
        Span::styled(" ".repeat(gap), status_bar_style()),
    ];
    spans.extend(status_stats_spans(app));

    Line::from(spans)
}

pub(super) fn status_stats_spans(app: &App) -> Vec<Span<'static>> {
    vec![
        Span::styled("files ", status_bar_style().fg(Color::Cyan).bold()),
        Span::styled(
            app.project.file_count().to_string(),
            status_bar_style().fg(Color::White).bold(),
        ),
        Span::styled(" symbols ", status_bar_style().fg(Color::Green).bold()),
        Span::styled(
            app.project.symbol_count().to_string(),
            status_bar_style().fg(Color::White).bold(),
        ),
        Span::styled(" visible ", status_bar_style().fg(Color::Yellow).bold()),
        Span::styled(
            app.visible_nodes().len().to_string(),
            status_bar_style().fg(Color::White).bold(),
        ),
    ]
}

pub(super) fn render_body(frame: &mut Frame, app: &mut App, area: Rect) {
    let wide = area.width >= 96;
    if wide {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
            .split(area);
        render_tree(frame, app, columns[0]);

        let details_height = preferred_details_height(app, columns[1].height);
        let right_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(details_height), Constraint::Min(3)])
            .split(columns[1]);
        render_details(frame, app, right_rows[0]);
        render_preview(frame, app, right_rows[1]);
    } else {
        render_tree(frame, app, area);
    }
}

pub(super) fn preferred_details_height(app: &App, max: u16) -> u16 {
    let detail_lines = if let Some(node) = app.selected_node() {
        selected_detail_lines(app, node).len()
    } else {
        1
    };
    let warning_lines = if app.project.warnings.is_empty() {
        0
    } else {
        2 + app.project.warnings.iter().take(5).count()
    };
    let target = (detail_lines + warning_lines + 2) as u16;
    let upper = max.saturating_sub(5).max(6);
    target.clamp(6, upper.max(6))
}

pub(super) fn render_tree(frame: &mut Frame, app: &mut App, area: Rect) {
    let visible = app.visible_nodes();
    let glyphs = Glyphs::new(app.glyph_mode);
    let items = visible
        .iter()
        .enumerate()
        .map(|(index, visible_node)| {
            render_tree_item(app, visible_node, glyphs, index == app.selected)
        })
        .collect::<Vec<_>>();

    let title = if app.filter.is_empty() {
        " Symbols ".to_string()
    } else {
        format!(" Symbols matching `{}` ", app.filter)
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_set(border_set(app.glyph_mode)),
        )
        .highlight_symbol(glyphs.selection_line())
        .highlight_style(selected_row_style())
        .repeat_highlight_symbol(true);

    app.list_state
        .select((!visible.is_empty()).then_some(app.selected));

    let inner_height = area.height.saturating_sub(2) as usize;
    if !visible.is_empty() && inner_height > 0 {
        let max_offset = visible.len().saturating_sub(inner_height);
        if app.center_selection_pending {
            let offset = app
                .selected
                .saturating_sub(inner_height / 2)
                .min(max_offset);
            *app.list_state.offset_mut() = offset;
            app.center_selection_pending = false;
        } else {
            let scrolloff = SCROLLOFF.min(inner_height.saturating_sub(1) / 2);
            let lower = (app.selected + scrolloff + 1).saturating_sub(inner_height);
            let upper = app.selected.saturating_sub(scrolloff);
            let current = app.list_state.offset();
            let clamped = current.max(lower).min(upper).min(max_offset);
            *app.list_state.offset_mut() = clamped;
        }
    }

    frame.render_stateful_widget(list, area, &mut app.list_state);
}

pub(super) fn render_tree_item<'a>(
    app: &App,
    visible: &VisibleNode,
    glyphs: Glyphs,
    selected: bool,
) -> ListItem<'a> {
    let node = get_node(&app.project.files, &visible.path).expect("visible path must resolve");
    let mut spans = Vec::new();

    spans.push(Span::styled(
        glyphs.connector(visible),
        row_style(Style::default().fg(Color::DarkGray), selected),
    ));
    spans.push(Span::styled(
        glyphs.marker(node),
        row_style(Style::default().fg(marker_color(node)), selected),
    ));
    spans.push(Span::styled(" ", row_style(Style::default(), selected)));
    push_match_spans(
        &mut spans,
        &format!("{:<7}", node.kind.short_label()),
        &app.filter,
        row_style(kind_style(node.kind), selected),
    );
    spans.push(Span::styled(" ", row_style(Style::default(), selected)));

    push_match_spans(
        &mut spans,
        &node.name,
        &app.filter,
        row_style(Style::default().fg(Color::White), selected),
    );

    if let Some(line) = node.line {
        spans.push(Span::styled(
            format!(":{line}"),
            row_style(Style::default().fg(Color::Blue), selected),
        ));
    }

    if let Some(detail) = &node.detail {
        spans.push(Span::styled("  ", row_style(Style::default(), selected)));
        push_match_spans(
            &mut spans,
            detail,
            &app.filter,
            row_style(Style::default().fg(Color::DarkGray), selected),
        );
    }

    let item = ListItem::new(Line::from(spans));
    if selected {
        item.style(selected_row_style())
    } else {
        item
    }
}

pub(super) fn render_details(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    if let Some(node) = app.selected_node() {
        lines.extend(selected_detail_lines(app, node));
    } else {
        lines.push(Line::from(Span::styled(
            "No symbol selected",
            Style::default().fg(Color::DarkGray),
        )));
    }

    if !app.project.warnings.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Warnings",
            Style::default().fg(Color::Red).bold(),
        )));
        for warning in app.project.warnings.iter().take(5) {
            lines.push(Line::from(vec![
                Span::styled("! ", Style::default().fg(Color::Red)),
                Span::styled(warning.clone(), Style::default().fg(Color::Gray)),
            ]));
        }
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Details ")
                    .borders(Borders::ALL)
                    .border_set(border_set(app.glyph_mode)),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

pub(super) fn render_preview(frame: &mut Frame, app: &mut App, area: Rect) {
    let preview_block = || {
        Block::default()
            .title(" Preview ")
            .borders(Borders::ALL)
            .border_set(border_set(app.glyph_mode))
    };

    if area.height < 3 || area.width < 3 {
        frame.render_widget(preview_block(), area);
        return;
    }

    let inner_height = area.height.saturating_sub(2) as usize;
    let inner_width = area.width.saturating_sub(2) as usize;

    let Some(target) = app.selected_target() else {
        frame.render_widget(preview_block(), area);
        render_message_box(frame, app, area, "No selection");
        return;
    };

    let cache_hit = app.preview_cache.as_ref().is_some_and(|c| {
        c.path == target.file
            && c.error.is_none()
            && c.highlighted.as_ref().is_some_and(|h| {
                if h.is_empty() {
                    return c.total_lines == 0;
                }
                let window_end = c.window_start + h.len();
                let (vp_start, vp_end) = viewport_range(target.line, inner_height, c.total_lines);
                vp_start >= c.window_start && vp_end <= window_end
            })
    });

    let render_line = if cache_hit {
        target.line
    } else {
        if app.preview_in_flight.is_none() {
            let _ = app.preview_request_tx.send(PreviewRequest {
                path: target.file.clone(),
                target_line: target.line,
                window: inner_height,
            });
            app.preview_in_flight = Some((target.file.clone(), Instant::now()));
        }
        cache_backdrop_line(app.preview_cache.as_ref(), app.preview_last_line)
    };

    let static_message: Option<String> = match app.preview_cache.as_ref() {
        Some(cache) => {
            if let Some(error) = &cache.error {
                Some(format!("Cannot preview: {error}"))
            } else if cache.highlighted.as_ref().is_some_and(|h| h.is_empty())
                && cache.total_lines == 0
            {
                Some("Empty file".to_string())
            } else if cache.highlighted.is_some() {
                let lines = build_preview_lines(cache, render_line, inner_height, inner_width);
                frame.render_widget(Paragraph::new(lines).block(preview_block()), area);
                None
            } else {
                Some("No preview available".to_string())
            }
        }
        None => {
            frame.render_widget(preview_block(), area);
            None
        }
    };

    if let Some(msg) = static_message {
        frame.render_widget(preview_block(), area);
        render_message_box(frame, app, area, &msg);
    }

    let should_show_overlay = app
        .preview_in_flight
        .as_ref()
        .is_some_and(|(_, t)| t.elapsed() >= LOADING_INDICATOR_DELAY);
    if should_show_overlay {
        render_message_box(frame, app, area, "Loading…");
    }
    app.loading_overlay_shown = should_show_overlay;

    app.preview_last_line = render_line;
}

fn cache_backdrop_line(cache: Option<&super::PreviewCache>, fallback: usize) -> usize {
    let Some(c) = cache else {
        return fallback;
    };
    let Some(h) = c.highlighted.as_ref() else {
        return fallback;
    };
    if h.is_empty() {
        return fallback;
    }
    let win_first = c.window_start + 1;
    let win_last = c.window_start + h.len();
    if fallback >= win_first && fallback <= win_last {
        fallback
    } else {
        c.window_start + h.len() / 2 + 1
    }
}

fn render_message_box(frame: &mut Frame, app: &App, area: Rect, label: &str) {
    let label_chars = label.chars().count() as u16;
    let max_inner = area.width.saturating_sub(4);
    let inner_w = label_chars
        .saturating_add(4)
        .min(max_inner.max(label_chars));
    let box_w = inner_w.saturating_add(2);
    let box_h: u16 = 3;
    if area.width < box_w.saturating_add(2) || area.height < box_h.saturating_add(2) {
        return;
    }
    let x = area.x + (area.width - box_w) / 2;
    let y = area.y + (area.height - box_h) / 2;
    let rect = Rect {
        x,
        y,
        width: box_w,
        height: box_h,
    };

    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            label.to_string(),
            Style::default().fg(Color::White),
        )))
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_set(border_set(app.glyph_mode)),
        ),
        rect,
    );
}
pub(super) fn selected_detail_lines(app: &App, node: &SymbolNode) -> Vec<Line<'static>> {
    let target = app.selected_target();
    let mut lines = vec![
        Line::from(Span::styled(
            node.name.clone(),
            Style::default().fg(Color::White).bold(),
        )),
        Line::from(vec![
            Span::styled("kind  ", Style::default().fg(Color::DarkGray)),
            Span::styled(node.kind.short_label(), kind_style(node.kind)),
        ]),
    ];

    if let Some(target) = target {
        lines.push(Line::from(vec![
            Span::styled("file  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                target.file.display().to_string(),
                Style::default().fg(Color::Gray),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("line  ", Style::default().fg(Color::DarkGray)),
            Span::styled(target.line.to_string(), Style::default().fg(Color::Blue)),
        ]));
    }

    if let Some(detail) = &node.detail {
        lines.push(Line::from(vec![
            Span::styled("type  ", Style::default().fg(Color::DarkGray)),
            Span::styled(detail.clone(), Style::default().fg(Color::Gray)),
        ]));
    }

    if node.has_children() {
        let state = if app.filter.is_empty() {
            if node.expanded {
                "expanded"
            } else {
                "collapsed"
            }
        } else {
            "expanded by filter"
        };
        lines.push(Line::from(vec![
            Span::styled("tree  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{state}, {} children", node.children.len()),
                Style::default().fg(Color::Gray),
            ),
        ]));
    }

    lines
}

pub(super) fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let lines = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);
    frame.render_widget(
        Paragraph::new(status_line(app, lines[0].width)).style(status_bar_style()),
        lines[0],
    );
    frame.render_widget(Paragraph::new(command_line(app)), lines[1]);
}

pub(super) fn command_line(app: &App) -> Line<'static> {
    match app.mode {
        Mode::Search => Line::from(vec![
            Span::styled("/", Style::default().fg(Color::Yellow).bold()),
            Span::styled(app.filter.clone(), Style::default().fg(Color::White)),
            command_cursor(),
        ]),
        Mode::Command => Line::from(vec![
            Span::styled(":", Style::default().fg(Color::Cyan).bold()),
            Span::styled(app.command.clone(), Style::default().fg(Color::White)),
            command_cursor(),
        ]),
        Mode::Normal if app.message.is_empty() => Line::raw(""),
        Mode::Normal => Line::from(Span::styled(
            app.message.clone(),
            Style::default().fg(Color::Gray),
        )),
    }
}

pub(super) fn command_cursor() -> Span<'static> {
    Span::styled(" ", Style::default().bg(Color::White))
}

pub(super) fn status_bar_style() -> Style {
    Style::default().bg(Color::Indexed(238))
}

pub(super) fn display_width(text: &str) -> usize {
    text.chars().count()
}

pub(super) fn truncate_left(text: &str, max_width: usize) -> String {
    if display_width(text) <= max_width {
        return text.to_string();
    }

    if max_width == 0 {
        return String::new();
    }

    if max_width == 1 {
        return "~".to_string();
    }

    let mut tail = text.chars().rev().take(max_width - 1).collect::<Vec<_>>();
    tail.reverse();
    format!("~{}", tail.into_iter().collect::<String>())
}
pub(super) fn render_help(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect(area, 70, 15);
    let lines = vec![
        Line::from(Span::styled(
            "Navigation",
            Style::default().fg(Color::Cyan).bold(),
        )),
        Line::from("  j/k, Up/Down       move row"),
        Line::from("  J/K                move 3 rows"),
        Line::from("  h/l, Left/Right    parent / first child"),
        Line::from("  u/i                next / previous sibling"),
        Line::from(""),
        Line::from(Span::styled(
            "Actions",
            Style::default().fg(Color::Cyan).bold(),
        )),
        Line::from("  Enter/Space        toggle branch"),
        Line::from("  /                  filter symbols"),
        Line::from("  :help              show this help"),
        Line::from("  o                  open selected in $EDITOR"),
        Line::from("  r                  reload symbols"),
        Line::from("  q                  quit"),
        Line::from(""),
        Line::from(Span::styled(
            "Esc / Enter / q      close",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Help ")
                    .borders(Borders::ALL)
                    .border_set(border_set(app.glyph_mode)),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

pub(super) fn centered_rect(area: Rect, width_percent: u16, height: u16) -> Rect {
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

pub(super) fn kind_style(kind: SymbolKind) -> Style {
    match kind {
        SymbolKind::File | SymbolKind::Lsp(1) => Style::default().fg(Color::Cyan).bold(),
        SymbolKind::Lsp(2) | SymbolKind::Lsp(3) | SymbolKind::Lsp(4) => {
            Style::default().fg(Color::Magenta)
        }
        SymbolKind::Lsp(5)
        | SymbolKind::Lsp(10)
        | SymbolKind::Lsp(11)
        | SymbolKind::Lsp(23)
        | SymbolKind::Lsp(26) => Style::default().fg(Color::Yellow).bold(),
        SymbolKind::Lsp(6) | SymbolKind::Lsp(12) => Style::default().fg(Color::Green).bold(),
        SymbolKind::Lsp(7) | SymbolKind::Lsp(8) | SymbolKind::Lsp(22) => {
            Style::default().fg(Color::Blue)
        }
        SymbolKind::Lsp(13) | SymbolKind::Lsp(14) => Style::default().fg(Color::LightRed),
        _ => Style::default().fg(Color::Gray),
    }
}

pub(super) fn marker_color(node: &SymbolNode) -> Color {
    if node.has_children() {
        Color::Cyan
    } else {
        Color::DarkGray
    }
}

pub(super) fn selected_row_style() -> Style {
    Style::default().bg(Color::Indexed(238))
}

pub(super) fn row_style(style: Style, selected: bool) -> Style {
    if selected {
        style.patch(selected_row_style())
    } else {
        style
    }
}

pub(super) fn match_style(base: Style) -> Style {
    base.fg(Color::Yellow)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}

pub(super) fn push_match_spans<'a>(
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

pub(super) fn match_range(text: &str, filter: &str) -> Option<(usize, usize)> {
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
#[derive(Debug, Clone, Copy)]
pub(super) struct Glyphs {
    mode: GlyphMode,
    selection: &'static str,
}

pub(super) const ASCII_BORDER: border::Set<'static> = border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

pub(super) fn border_set(mode: GlyphMode) -> border::Set<'static> {
    match mode {
        GlyphMode::Unicode => border::PLAIN,
        GlyphMode::Ascii => ASCII_BORDER,
    }
}

impl Glyphs {
    pub(super) fn new(mode: GlyphMode) -> Self {
        let selection = match mode {
            GlyphMode::Unicode => "▌ ",
            GlyphMode::Ascii => "> ",
        };
        Self { mode, selection }
    }

    pub(super) fn connector(self, visible: &VisibleNode) -> String {
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
            connector.push_str(match (self.mode, ancestor_is_last) {
                (GlyphMode::Unicode, true) => "  ",
                (GlyphMode::Unicode, false) => "│ ",
                (GlyphMode::Ascii, true) => "  ",
                (GlyphMode::Ascii, false) => "| ",
            });
        }

        connector.push_str(match (self.mode, visible.is_last) {
            (GlyphMode::Unicode, true) => "└─",
            (GlyphMode::Unicode, false) => "├─",
            (GlyphMode::Ascii, true) => "`-",
            (GlyphMode::Ascii, false) => "|-",
        });
        connector
    }

    fn marker(self, node: &SymbolNode) -> &'static str {
        if !node.has_children() {
            return " ";
        }

        match (self.mode, node.expanded) {
            (GlyphMode::Unicode, true) => "▾",
            (GlyphMode::Unicode, false) => "▸",
            (GlyphMode::Ascii, true) => "v",
            (GlyphMode::Ascii, false) => ">",
        }
    }

    fn selection_line(self) -> Line<'static> {
        Line::from(Span::styled(
            self.selection,
            selected_row_style().fg(Color::Cyan).bold(),
        ))
    }
}
