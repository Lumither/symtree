use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::border,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};
use std::time::{Duration, Instant};

use crate::{
    model::{SymbolKind, SymbolNode},
    query::{Expr, PredicateKey},
    tree::{VisibleNode, get_node},
};

use super::preview::{build_preview_lines, viewport_range};
use super::{App, GlyphMode, LOADING_INDICATOR_DELAY, Mode, PreviewRequest};

const SCROLLOFF: usize = 5;

fn clamp_overlay_scroll(app: &App, total_lines: usize, popup_height: u16) -> u16 {
    let inner = popup_height.saturating_sub(2) as usize;
    let max = total_lines.saturating_sub(inner);
    app.overlay_scroll.min(max) as u16
}

fn fit_overlay_height(area: Rect, content_lines: usize) -> u16 {
    let content = u16::try_from(content_lines).unwrap_or(u16::MAX);
    content
        .saturating_add(2)
        .min(area.height.saturating_sub(2))
        .max(3)
}

pub(super) fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < 40 || area.height < 8 {
        render_too_small(frame, area);
        return;
    }

    let footer_rows: u16 = if app.query.is_some() { 3 } else { 2 };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(footer_rows)])
        .split(area);

    render_body(frame, app, sections[0]);
    let (_, footer) = crate::timed!(render_footer(frame, app, sections[1]));
    app.perf.borrow_mut().record_render_footer(footer);

    if matches!(app.mode, Mode::Search | Mode::Command) {
        render_autocomplete(frame, app, sections[1]);
    }

    if app.show_help {
        render_help(frame, app, area);
    } else if app.show_keymap {
        render_keymap(frame, app, area);
    } else if app.show_warnings {
        render_warnings(frame, app, area);
    } else if app.show_lsp {
        render_lsp(frame, app, area);
    } else {
        #[cfg(feature = "debug_perf")]
        if app.show_perf {
            render_perf(frame, app, area);
        }
    }
}

fn render_autocomplete(frame: &mut Frame, app: &App, footer_area: Rect) {
    let candidates = match app.mode {
        Mode::Search => super::autocomplete::search_candidates(app),
        Mode::Command => super::autocomplete::command_candidates(app),
        Mode::Normal => return,
    };
    if candidates.is_empty() {
        return;
    }

    let visible_count = candidates.len().min(8) as u16;
    let max_width = candidates
        .iter()
        .map(|c| c.chars().count())
        .max()
        .unwrap_or(0) as u16;
    let box_w = max_width.saturating_add(2).min(footer_area.width);
    let box_h = visible_count;

    if footer_area.y < box_h {
        return;
    }
    let rect = Rect {
        x: footer_area.x,
        y: footer_area.y.saturating_sub(box_h),
        width: box_w,
        height: box_h,
    };

    let selected = app.candidate_index.min(candidates.len().saturating_sub(1));
    let inner_w = box_w as usize;
    let popup_bg = Color::Indexed(236);
    let selected_bg = Color::Indexed(240);

    let lines: Vec<Line<'static>> = candidates
        .iter()
        .enumerate()
        .take(visible_count as usize)
        .map(|(i, c)| {
            let is_selected = i == selected;
            let bg = if is_selected { selected_bg } else { popup_bg };
            let fg = if is_selected {
                Color::White
            } else {
                Color::Indexed(250)
            };
            let mut text = format!(" {c}");
            let used = text.chars().count();
            if used < inner_w {
                text.push_str(&" ".repeat(inner_w - used));
            }
            let mut style = Style::default().fg(fg).bg(bg);
            if is_selected {
                style = style.bold();
            }
            Line::from(Span::styled(text, style))
        })
        .collect();

    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(popup_bg)),
        rect,
    );
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
    let indexing = indexing_text(app);
    let stats = format!(
        "files {} symbols {} visible {}",
        app.project.file_count(),
        app.symbol_count_cache,
        app.visible_nodes().len()
    );
    let right_width = display_width(&indexing).saturating_add(display_width(&stats));
    let path_width = width.saturating_sub(right_width.saturating_add(1));
    let path = truncate_left(&app.project.root.display().to_string(), path_width);
    let gap = width
        .saturating_sub(display_width(&path).saturating_add(right_width))
        .max(1);

    let mut spans = vec![
        Span::styled(path, status_bar_style()),
        Span::styled(" ".repeat(gap), status_bar_style()),
    ];
    if !indexing.is_empty() {
        spans.push(Span::styled(
            indexing,
            status_bar_style().fg(Color::Indexed(244)),
        ));
    }
    spans.extend(status_stats_spans(app));

    Line::from(spans)
}

fn indexing_text(app: &App) -> String {
    if !app.load_in_flight {
        return String::new();
    }
    if app.discovered_file_count > 0 {
        format!(
            "indexing {}/{}  ",
            app.loaded_file_count, app.discovered_file_count
        )
    } else {
        format!("indexing {}  ", app.loaded_file_count)
    }
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
            app.symbol_count_cache.to_string(),
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
        let (_, tree) = crate::timed!(render_tree(frame, app, columns[0]));

        let details_height = preferred_details_height(app, columns[1].height);
        let right_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(details_height), Constraint::Min(3)])
            .split(columns[1]);
        let (_, details) = crate::timed!(render_details(frame, app, right_rows[0]));
        let (_, preview) = crate::timed!(render_preview(frame, app, right_rows[1]));

        let mut p = app.perf.borrow_mut();
        p.record_render_tree(tree);
        p.record_render_details(details);
        p.record_render_preview(preview);
    } else {
        let (_, tree) = crate::timed!(render_tree(frame, app, area));
        let mut p = app.perf.borrow_mut();
        p.record_render_tree(tree);
        p.record_render_details(Duration::ZERO);
        p.record_render_preview(Duration::ZERO);
    }
}

pub(super) fn preferred_details_height(app: &App, max: u16) -> u16 {
    let detail_lines = if let Some(node) = app.selected_node() {
        selected_detail_lines(app, node).len()
    } else {
        1
    };
    let target = (detail_lines + 2) as u16;
    let upper = max.saturating_sub(5).max(6);
    target.clamp(6, upper.max(6))
}

pub(super) fn render_tree(frame: &mut Frame, app: &mut App, area: Rect) {
    let inner_height = area.height.saturating_sub(2) as usize;
    let glyphs = Glyphs::new(app.glyph_mode);
    let visible_len = app.visible_nodes().len();

    let viewport_start = if visible_len == 0 || inner_height == 0 {
        0
    } else {
        let max_offset = visible_len.saturating_sub(inner_height);
        if app.center_selection_pending {
            app.selected
                .saturating_sub(inner_height / 2)
                .min(max_offset)
        } else {
            let scrolloff = SCROLLOFF.min(inner_height.saturating_sub(1) / 2);
            let lower = (app.selected + scrolloff + 1).saturating_sub(inner_height);
            let upper = app.selected.saturating_sub(scrolloff);
            let current = app.scroll_offset;
            current.max(lower).min(upper).min(max_offset)
        }
    };
    let viewport_end = (viewport_start + inner_height).min(visible_len);
    app.scroll_offset = viewport_start;
    app.center_selection_pending = false;

    *app.list_state.offset_mut() = 0;
    let relative_selected =
        if visible_len > 0 && app.selected >= viewport_start && app.selected < viewport_end {
            Some(app.selected - viewport_start)
        } else {
            None
        };
    app.list_state.select(relative_selected);

    let title = if app.filter.is_empty() {
        " Symbols ".to_string()
    } else {
        format!(" Symbols matching `{}` ", app.filter)
    };

    let items: Vec<ListItem> = {
        let visible = app.visible_nodes();
        visible[viewport_start..viewport_end]
            .iter()
            .enumerate()
            .map(|(i, vn)| {
                let absolute = viewport_start + i;
                render_tree_item(app, vn, glyphs, absolute == app.selected)
            })
            .collect()
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
    let has_query = app.query.is_some();
    let constraints: Vec<Constraint> = if has_query {
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ]
    } else {
        vec![Constraint::Length(1), Constraint::Length(1)]
    };
    let lines = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let (status_row, command_row) = if has_query {
        let query_spans = query_status_line(app);
        frame.render_widget(
            Paragraph::new(Line::from(query_spans)).style(status_bar_style()),
            lines[0],
        );
        (1, 2)
    } else {
        (0, 1)
    };

    frame.render_widget(
        Paragraph::new(status_line(app, lines[status_row].width)).style(status_bar_style()),
        lines[status_row],
    );
    frame.render_widget(Paragraph::new(command_line(app)), lines[command_row]);
}

fn query_status_line(app: &App) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        " query: ",
        status_bar_style().fg(Color::White).bold(),
    )];
    if let Some(expr) = app.query.as_ref() {
        render_expr_spans(expr, &mut spans);
    }
    spans
}

fn render_expr_spans(expr: &Expr, out: &mut Vec<Span<'static>>) {
    match expr {
        Expr::Or(items) => {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(Span::styled(
                        " || ",
                        status_bar_style().fg(Color::Indexed(244)),
                    ));
                }
                render_expr_spans(item, out);
            }
        }
        Expr::And(items) => {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(Span::styled(" ", status_bar_style()));
                }
                render_expr_spans(item, out);
            }
        }
        Expr::Not(inner) => {
            out.push(Span::styled("!", status_bar_style().fg(Color::Red).bold()));
            render_expr_spans(inner, out);
        }
        Expr::Predicate { key, value } => {
            let color = predicate_color(*key);
            out.push(Span::styled(
                format!("{}:", key.as_str()),
                status_bar_style().fg(color).bold(),
            ));
            out.push(Span::styled(value.clone(), status_bar_style().fg(color)));
        }
        Expr::Text { pattern, regex } => {
            let text = if regex.is_some() {
                format!("\"{pattern}\"")
            } else {
                pattern.clone()
            };
            out.push(Span::styled(text, status_bar_style().fg(Color::White)));
        }
    }
}

fn predicate_color(key: PredicateKey) -> Color {
    match key {
        PredicateKey::Lang => Color::Cyan,
        PredicateKey::Kind => Color::Green,
        PredicateKey::File => Color::Blue,
        PredicateKey::Name => Color::White,
    }
}

pub(super) fn command_line(app: &App) -> Line<'static> {
    match app.mode {
        Mode::Search => build_input_line(
            "/",
            Style::default().fg(Color::Yellow).bold(),
            &app.filter,
            app.cursor_pos,
        ),
        Mode::Command => build_input_line(
            ":",
            Style::default().fg(Color::Cyan).bold(),
            &app.command,
            app.cursor_pos,
        ),
        Mode::Normal if app.message.is_empty() => Line::raw(""),
        Mode::Normal => Line::from(Span::styled(
            app.message.clone(),
            Style::default().fg(Color::Gray),
        )),
    }
}

fn build_input_line(prefix: &str, prefix_style: Style, text: &str, cursor: usize) -> Line<'static> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let before: String = chars[..cursor].iter().collect();
    let cursor_style = Style::default().fg(Color::Black).bg(Color::White);
    let mut spans = vec![
        Span::styled(prefix.to_string(), prefix_style),
        Span::styled(before, Style::default().fg(Color::White)),
    ];
    if cursor < chars.len() {
        spans.push(Span::styled(chars[cursor].to_string(), cursor_style));
        let after: String = chars[cursor + 1..].iter().collect();
        if !after.is_empty() {
            spans.push(Span::styled(after, Style::default().fg(Color::White)));
        }
    } else {
        spans.push(Span::styled(" ", cursor_style));
    }
    Line::from(spans)
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
#[cfg(feature = "debug_perf")]
pub(super) fn render_perf(frame: &mut Frame, app: &App, area: Rect) {
    let perf = app.perf.borrow();
    let mut lines: Vec<Line<'static>> = Vec::new();

    let section = |title: &str| {
        Line::from(Span::styled(
            title.to_string(),
            Style::default().fg(Color::Cyan).bold(),
        ))
    };
    let row = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("  {label:<22}"), Style::default().fg(Color::Gray)),
            Span::styled(value, Style::default().fg(Color::White)),
        ])
    };

    lines.push(section("Frames"));
    lines.push(row("count", perf.frame_count.to_string()));
    lines.push(row("last", format_duration(perf.last_frame)));
    lines.push(row("avg", format_duration(perf.avg_frame())));
    lines.push(row("max", format_duration(perf.max_frame)));
    lines.push(Line::raw(""));

    lines.push(section("Last render breakdown"));
    lines.push(row("tree", format_duration(perf.last_tree)));
    lines.push(row("details", format_duration(perf.last_details)));
    lines.push(row("preview", format_duration(perf.last_preview)));
    lines.push(row("footer", format_duration(perf.last_footer)));
    lines.push(Line::raw(""));

    lines.push(section("flatten_visible cache rebuilds"));
    lines.push(row("count", perf.flatten_calls.to_string()));
    lines.push(row("last", format_duration(perf.flatten_last)));
    lines.push(row(
        "last size",
        format!("{} nodes", perf.flatten_last_size),
    ));
    lines.push(row("avg", format_duration(perf.avg_flatten())));
    lines.push(row("max", format_duration(perf.flatten_max)));
    lines.push(Line::raw(""));

    lines.push(section("Load events"));
    lines.push(row("total drained", perf.load_events_drained.to_string()));
    lines.push(row("last drain size", perf.last_drain_size.to_string()));

    let popup = centered_rect(area, 70, fit_overlay_height(area, lines.len()));
    let scroll = clamp_overlay_scroll(app, lines.len(), popup.height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Performance ")
                    .borders(Borders::ALL)
                    .border_set(border_set(app.glyph_mode)),
            )
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        popup,
    );
}

#[cfg(feature = "debug_perf")]
fn format_duration(d: Duration) -> String {
    let us = d.as_micros();
    if us < 1_000 {
        format!("{us} µs")
    } else if us < 1_000_000 {
        format!("{:.2} ms", us as f64 / 1_000.0)
    } else {
        format!("{:.2} s", us as f64 / 1_000_000.0)
    }
}

pub(super) fn render_warnings(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if app.project.warnings.is_empty() {
        lines.push(Line::from(Span::styled(
            "No warnings",
            Style::default().fg(Color::Gray),
        )));
    } else {
        for warning in &app.project.warnings {
            lines.push(Line::from(vec![
                Span::styled("! ", Style::default().fg(Color::Red).bold()),
                Span::styled(warning.clone(), Style::default().fg(Color::Gray)),
            ]));
        }
    }

    let popup = centered_rect(area, 80, fit_overlay_height(area, lines.len()));
    let scroll = clamp_overlay_scroll(app, lines.len(), popup.height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(" Warnings ({}) ", app.project.warnings.len()))
                    .borders(Borders::ALL)
                    .border_set(border_set(app.glyph_mode)),
            )
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        popup,
    );
}

pub(super) fn render_lsp(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::from(Span::styled(
        "Configured languages",
        Style::default().fg(Color::Cyan).bold(),
    )));
    lines.push(Line::raw(""));

    let mut entries: Vec<(u8, &str, Color, &crate::languages::LanguageDef, usize)> = app
        .languages
        .iter()
        .map(|lang| {
            let installed = crate::lsp::lsp_is_available(&lang.lsp);
            let files_seen = app
                .project
                .files
                .iter()
                .filter(|f| {
                    std::path::Path::new(&f.name)
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| lang.extensions.iter().any(|x| x == e))
                })
                .count();
            let (rank, label, color) = if installed && files_seen > 0 {
                (0u8, "active", Color::Green)
            } else if installed {
                (1u8, "idle", Color::DarkGray)
            } else {
                (2u8, "missing", Color::DarkGray)
            };
            (rank, label, color, lang, files_seen)
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.3.lsp.cmp(&b.3.lsp)));

    for (_, status_text, status_color, lang, files_seen) in entries {
        let program = crate::languages::lsp_program(&lang.lsp);
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<14}", program),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!("{:>4} files  ", files_seen),
                Style::default().fg(Color::Gray),
            ),
            Span::styled(
                format!("[{status_text}]"),
                Style::default().fg(status_color).bold(),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            format!("    ext: {}", lang.extensions.join(", ")),
            Style::default().fg(Color::DarkGray),
        )));
    }

    let popup = centered_rect(area, 80, fit_overlay_height(area, lines.len()));
    let scroll = clamp_overlay_scroll(app, lines.len(), popup.height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(" LSP ({}) ", app.languages.len()))
                    .borders(Borders::ALL)
                    .border_set(border_set(app.glyph_mode)),
            )
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        popup,
    );
}

#[allow(clippy::vec_init_then_push)]
pub(super) fn render_help(frame: &mut Frame, app: &App, area: Rect) {
    let section = |title: &str| {
        Line::from(Span::styled(
            title.to_string(),
            Style::default().fg(Color::Cyan).bold(),
        ))
    };
    let cmd = |name: &str, desc: &str| {
        Line::from(vec![
            Span::styled(
                format!("  :{name:<20}"),
                Style::default().fg(Color::White),
            ),
            Span::styled(desc.to_string(), Style::default().fg(Color::Gray)),
        ])
    };
    let example = |line: &str| {
        Line::from(Span::styled(
            format!("  {line}"),
            Style::default().fg(Color::Gray),
        ))
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(section("Overview"));
    lines.push(Line::from(Span::styled(
        "  symtree — multi-language symbol-tree browser",
        Style::default().fg(Color::Gray),
    )));
    lines.push(Line::from(Span::styled(
        "  Use :keymap for key bindings",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::raw(""));

    lines.push(section("Commands"));
    lines.push(cmd("help", "show this page"));
    lines.push(cmd("keymap", "show key bindings"));
    lines.push(cmd("warnings", "list load warnings (alias: :w)"));
    lines.push(cmd("lsp", "list configured LSPs and status"));
    #[cfg(feature = "debug_perf")]
    lines.push(cmd("perf", "performance stats (:perf reset)"));
    lines.push(cmd("collapse", "collapse every node"));
    lines.push(cmd("expand", "expand every node"));
    lines.push(cmd("query EXPR", "filter symbols (see Query)"));
    lines.push(cmd("query", "edit current query / :query clear"));
    lines.push(cmd("q | quit", "exit symtree"));
    lines.push(Line::raw(""));

    lines.push(section("Query language"));
    lines.push(Line::from(Span::styled(
        "  Predicates:  lang:<id>  kind:<k>  file:<sub>  name:<sub>",
        Style::default().fg(Color::Gray),
    )));
    lines.push(Line::from(Span::styled(
        "  Bare word = case-insensitive substring on name",
        Style::default().fg(Color::Gray),
    )));
    lines.push(Line::from(Span::styled(
        "  Quoted   = regex, e.g. \"^handle_.*\"",
        Style::default().fg(Color::Gray),
    )));
    lines.push(Line::from(Span::styled(
        "  Operators: implicit AND  ·  ||  OR  ·  ! negation",
        Style::default().fg(Color::Gray),
    )));
    lines.push(Line::raw(""));

    lines.push(section("Examples"));
    lines.push(example(":query lang:rust kind:fn"));
    lines.push(example(":query lang:rust || lang:python"));
    lines.push(example(":query !kind:fn \"^handle_\""));
    lines.push(example(":query file:src/tui name:render"));
    lines.push(Line::raw(""));

    lines.push(section("Environment"));
    lines.push(Line::from(Span::styled(
        "  LSP_SEARCH_PATH    prepended to PATH for LSP binaries",
        Style::default().fg(Color::Gray),
    )));

    let popup = centered_rect(area, 70, fit_overlay_height(area, lines.len()));
    let scroll = clamp_overlay_scroll(app, lines.len(), popup.height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Help ")
                    .borders(Borders::ALL)
                    .border_set(border_set(app.glyph_mode)),
            )
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        popup,
    );
}

#[allow(clippy::vec_init_then_push)]
pub(super) fn render_keymap(frame: &mut Frame, app: &App, area: Rect) {
    let section = |title: &str| {
        Line::from(Span::styled(
            title.to_string(),
            Style::default().fg(Color::Cyan).bold(),
        ))
    };
    let bind = |keys: &str, desc: &str| {
        Line::from(vec![
            Span::styled(format!("  {keys:<22}"), Style::default().fg(Color::White)),
            Span::styled(desc.to_string(), Style::default().fg(Color::Gray)),
        ])
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(section("Tree navigation"));
    lines.push(bind("j / k", "move down / up one row"));
    lines.push(bind("Down / Up", "same as j / k"));
    lines.push(bind("J / K", "move 3 rows"));
    lines.push(bind("PageDown / PageUp", "scroll one page"));
    lines.push(bind("Home / End", "first / last visible row"));
    lines.push(bind("h / Left", "jump to parent"));
    lines.push(bind("l / Right", "jump to first child"));
    lines.push(bind("u / i", "previous / next sibling"));
    lines.push(bind("Enter / Space", "toggle branch expand/collapse"));
    lines.push(Line::raw(""));

    lines.push(section("Actions"));
    lines.push(bind("/", "filter (live, applied as you type)"));
    lines.push(bind(":", "command prompt"));
    lines.push(bind("o", "open selection in $EDITOR"));
    lines.push(bind("r", "reload symbols"));
    lines.push(bind("q  /  Ctrl-C", "quit"));
    lines.push(Line::raw(""));

    lines.push(section("Prompt editing  (/ and :)"));
    lines.push(bind("Left / Right", "move cursor one char"));
    lines.push(bind("Home / Ctrl-A", "cursor to start"));
    lines.push(bind("End / Ctrl-E", "cursor to end"));
    lines.push(bind("Ctrl-Left/Right", "jump by word"));
    lines.push(bind("Backspace / Del", "delete before / at cursor"));
    lines.push(bind("Ctrl-U", "clear buffer"));
    lines.push(bind("Tab", "accept autocomplete candidate"));
    lines.push(bind("Up / Down", "select candidate"));
    lines.push(bind("Enter", "submit"));
    lines.push(bind("Esc", "cancel"));
    lines.push(Line::raw(""));

    let popup = centered_rect(area, 70, fit_overlay_height(area, lines.len()));
    let scroll = clamp_overlay_scroll(app, lines.len(), popup.height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Keymap ")
                    .borders(Borders::ALL)
                    .border_set(border_set(app.glyph_mode)),
            )
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
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
