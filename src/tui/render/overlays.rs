//! Full-screen popup overlays: help, keymap, warnings, LSP status, and (under
//! `debug_perf`) the performance page. They share `render_scroll_overlay` for
//! the centered/bordered/scrollable frame.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
#[cfg(feature = "debug_perf")]
use std::time::Duration;

use super::centered_rect;
use crate::tui::App;

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

/// A cyan section-header line for the overlay pages.
fn section_line(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        title.to_string(),
        Style::default().fg(Color::Cyan).bold(),
    ))
}

/// Render `lines` as a centered, bordered, scrollable popup. Every overlay
/// (help / keymap / warnings / lsp / perf) shares this tail, which used to be
/// copy-pasted verbatim in each.
fn render_scroll_overlay(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    title: String,
    width_percent: u16,
    lines: Vec<Line<'static>>,
) {
    let popup = centered_rect(area, width_percent, fit_overlay_height(area, lines.len()));
    let scroll = clamp_overlay_scroll(app, lines.len(), popup.height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        popup,
    );
}

#[cfg(feature = "debug_perf")]
pub(super) fn render_perf(frame: &mut Frame, app: &App, area: Rect) {
    let perf = app.perf.borrow();
    let mut lines: Vec<Line<'static>> = Vec::new();

    let row = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("  {label:<22}"), Style::default().fg(Color::Gray)),
            Span::styled(value, Style::default().fg(Color::White)),
        ])
    };

    lines.push(section_line("Frames"));
    lines.push(row("count", perf.frame_count.to_string()));
    lines.push(row("last", format_duration(perf.last_frame)));
    lines.push(row("avg", format_duration(perf.avg_frame())));
    lines.push(row("max", format_duration(perf.max_frame)));
    lines.push(Line::raw(""));

    lines.push(section_line("Last render breakdown"));
    lines.push(row("tree", format_duration(perf.last_tree)));
    lines.push(row("details", format_duration(perf.last_details)));
    lines.push(row("preview", format_duration(perf.last_preview)));
    lines.push(row("footer", format_duration(perf.last_footer)));
    lines.push(Line::raw(""));

    lines.push(section_line("flatten_visible cache rebuilds"));
    lines.push(row("count", perf.flatten_calls.to_string()));
    lines.push(row("last", format_duration(perf.flatten_last)));
    lines.push(row(
        "last size",
        format!("{} nodes", perf.flatten_last_size),
    ));
    lines.push(row("avg", format_duration(perf.avg_flatten())));
    lines.push(row("max", format_duration(perf.flatten_max)));
    lines.push(Line::raw(""));

    lines.push(section_line("Load events"));
    lines.push(row("total drained", perf.load_events_drained.to_string()));
    lines.push(row("last drain size", perf.last_drain_size.to_string()));

    render_scroll_overlay(frame, app, area, " Performance ".to_string(), 70, lines);
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

    let title = format!(" Warnings ({}) ", app.project.warnings.len());
    render_scroll_overlay(frame, app, area, title, 80, lines);
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

    let title = format!(" LSP ({}) ", app.languages.len());
    render_scroll_overlay(frame, app, area, title, 80, lines);
}

#[allow(clippy::vec_init_then_push)]
pub(super) fn render_help(frame: &mut Frame, app: &App, area: Rect) {
    let cmd = |name: &str, desc: &str| {
        Line::from(vec![
            Span::styled(format!("  :{name:<20}"), Style::default().fg(Color::White)),
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

    lines.push(section_line("Overview"));
    lines.push(Line::from(Span::styled(
        "  symtree — multi-language symbol-tree browser",
        Style::default().fg(Color::Gray),
    )));
    lines.push(Line::from(Span::styled(
        "  Use :keymap for key bindings",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::raw(""));

    lines.push(section_line("Commands"));
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

    lines.push(section_line("Query language"));
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

    lines.push(section_line("Examples"));
    lines.push(example(":query lang:rust kind:fn"));
    lines.push(example(":query lang:rust || lang:python"));
    lines.push(example(":query !kind:fn \"^handle_\""));
    lines.push(example(":query file:src/tui name:render"));
    lines.push(Line::raw(""));

    lines.push(section_line("Environment"));
    lines.push(Line::from(Span::styled(
        "  LSP_SEARCH_PATH    prepended to PATH for LSP binaries",
        Style::default().fg(Color::Gray),
    )));

    render_scroll_overlay(frame, app, area, " Help ".to_string(), 70, lines);
}

#[allow(clippy::vec_init_then_push)]
pub(super) fn render_keymap(frame: &mut Frame, app: &App, area: Rect) {
    let bind = |keys: &str, desc: &str| {
        Line::from(vec![
            Span::styled(format!("  {keys:<22}"), Style::default().fg(Color::White)),
            Span::styled(desc.to_string(), Style::default().fg(Color::Gray)),
        ])
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(section_line("Tree navigation"));
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

    lines.push(section_line("Actions"));
    lines.push(bind("/", "filter (live, applied as you type)"));
    lines.push(bind(":", "command prompt"));
    lines.push(bind("o", "open selection in $EDITOR"));
    lines.push(bind("r", "reload symbols"));
    lines.push(bind(":q  /  Ctrl-C", "quit"));
    lines.push(Line::raw(""));

    lines.push(section_line("Prompt editing  (/ and :)"));
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

    render_scroll_overlay(frame, app, area, " Keymap ".to_string(), 70, lines);
}
