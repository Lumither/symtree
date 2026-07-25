use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{
        LazyLock,
        mpsc::{Receiver, Sender},
    },
    time::Duration,
};

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Style as SyntectStyle, Theme, ThemeSet},
    parsing::{SyntaxReference, SyntaxSet},
};

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

const THEME_BYTES: &[u8] = include_bytes!("../../assets/material-theme.tmTheme");

static THEME: LazyLock<Theme> = LazyLock::new(|| {
    let mut cursor = Cursor::new(THEME_BYTES);
    ThemeSet::load_from_reader(&mut cursor).expect("embedded theme failed to parse")
});

pub(super) const LOADING_INDICATOR_DELAY: Duration = Duration::from_millis(100);

/// Files larger than this are not previewed. Reading and highlighting a
/// multi-megabyte (or multi-gigabyte) file would block the worker and balloon
/// memory; source files we care about are comfortably under this.
const MAX_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;

pub(super) struct PreviewCache {
    pub(super) path: PathBuf,
    pub(super) window_start: usize,
    pub(super) total_lines: usize,
    pub(super) highlighted: Option<Vec<Vec<(SyntectStyle, String)>>>,
    pub(super) error: Option<String>,
}

pub(super) struct PreviewRequest {
    pub(super) path: PathBuf,
    pub(super) target_line: usize,
    pub(super) window: usize,
}

pub(super) fn preview_worker(rx: Receiver<PreviewRequest>, tx: Sender<PreviewCache>) {
    while let Ok(mut req) = rx.recv() {
        while let Ok(next) = rx.try_recv() {
            req = next;
        }
        let cache = compute_preview(&req);
        if tx.send(cache).is_err() {
            break;
        }
    }
}

fn compute_preview(req: &PreviewRequest) -> PreviewCache {
    let path = req.path.clone();

    if let Ok(metadata) = fs::metadata(&path)
        && metadata.len() > MAX_PREVIEW_BYTES
    {
        return PreviewCache {
            path,
            window_start: 0,
            total_lines: 0,
            highlighted: None,
            error: Some(format!(
                "file too large to preview ({:.1} MiB)",
                metadata.len() as f64 / (1024.0 * 1024.0)
            )),
        };
    }

    let source = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            return PreviewCache {
                path,
                window_start: 0,
                total_lines: 0,
                highlighted: None,
                error: Some(err.to_string()),
            };
        }
    };

    let source = source.replace('\t', "    ");

    if source.is_empty() {
        return PreviewCache {
            path,
            window_start: 0,
            total_lines: 0,
            highlighted: Some(Vec::new()),
            error: None,
        };
    }

    let lines: Vec<&str> = source.lines().collect();
    let total = lines.len();

    let window = (req.window.saturating_mul(3)).max(64).min(total);
    let half = window / 2;
    let target_idx = req
        .target_line
        .saturating_sub(1)
        .min(total.saturating_sub(1));
    let start = target_idx
        .saturating_sub(half)
        .min(total.saturating_sub(window));
    let end = (start + window).min(total);

    let syntax = detect_syntax(&path, &source);
    let mut highlighter = HighlightLines::new(syntax, &THEME);
    // Syntect's highlighter is stateful: the color of a line can depend on
    // earlier ones (open block comments, multi-line strings, here-docs). Feed it
    // the lines preceding the window, discarding their output, so the window is
    // colored with the correct carried-over state rather than as if the file
    // began at `start`.
    for line in &lines[..start] {
        let _ = highlighter.highlight_line(line, &SYNTAX_SET);
    }
    let highlighted: Vec<Vec<(SyntectStyle, String)>> = lines[start..end]
        .iter()
        .map(|line| match highlighter.highlight_line(line, &SYNTAX_SET) {
            Ok(chunks) => chunks
                .into_iter()
                .map(|(style, text)| (style, text.to_string()))
                .collect(),
            Err(_) => vec![(SyntectStyle::default(), line.to_string())],
        })
        .collect();

    PreviewCache {
        path,
        window_start: start,
        total_lines: total,
        highlighted: Some(highlighted),
        error: None,
    }
}

fn detect_syntax(path: &Path, source: &str) -> &'static SyntaxReference {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    SYNTAX_SET
        .find_syntax_by_extension(ext)
        .or_else(|| {
            source
                .lines()
                .next()
                .and_then(|line| SYNTAX_SET.find_syntax_by_first_line(line))
        })
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text())
}

pub(super) fn viewport_range(
    target_line: usize,
    inner_height: usize,
    total: usize,
) -> (usize, usize) {
    if total == 0 || inner_height == 0 {
        return (0, 0);
    }
    let target_idx = target_line.saturating_sub(1).min(total.saturating_sub(1));
    let half = inner_height / 2;
    let max_start = total.saturating_sub(inner_height);
    let start = target_idx.saturating_sub(half).min(max_start);
    let end = (start + inner_height).min(total);
    (start, end)
}

pub(super) fn build_preview_lines(
    cache: &PreviewCache,
    target_line: usize,
    inner_height: usize,
    inner_width: usize,
) -> Vec<Line<'static>> {
    if inner_height == 0 {
        return Vec::new();
    }
    let highlighted = cache.highlighted.as_deref().unwrap_or(&[]);
    let total = cache.total_lines;

    let (start, end) = viewport_range(target_line, inner_height, total);

    let gutter_width = format!("{}", total.max(1)).chars().count().max(3);
    let highlight_bg = super::theme::SELECTION_BG;
    let window_start = cache.window_start;
    let window_end = window_start + highlighted.len();
    let tilde_style = Style::default().fg(Color::DarkGray);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner_height);

    for i in start..end {
        let line_no = i + 1;
        let is_target = line_no == target_line;

        let mut spans: Vec<Span<'static>> = Vec::new();

        let mut gutter_style = Style::default().fg(Color::DarkGray);
        if is_target {
            gutter_style = gutter_style.fg(Color::Yellow).bg(highlight_bg).bold();
        }
        spans.push(Span::styled(
            format!("{line_no:>gutter_width$} "),
            gutter_style,
        ));

        if i >= window_start && i < window_end {
            for (sty, text) in &highlighted[i - window_start] {
                let mut style = Style::default().fg(syntect_color_to_ratatui(sty.foreground));
                if is_target {
                    style = style.bg(highlight_bg);
                }
                if sty.font_style.contains(FontStyle::BOLD) {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if sty.font_style.contains(FontStyle::ITALIC) {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if sty.font_style.contains(FontStyle::UNDERLINE) {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                spans.push(Span::styled(text.clone(), style));
            }
        }

        if is_target {
            let consumed: usize = spans
                .iter()
                .map(|s| super::render::display_width(&s.content))
                .sum();
            if consumed < inner_width {
                spans.push(Span::styled(
                    " ".repeat(inner_width - consumed),
                    Style::default().bg(highlight_bg),
                ));
            }
        }

        lines.push(Line::from(spans));
    }

    let tilde = format!("{:>gutter_width$} ", "~");
    while lines.len() < inner_height {
        lines.push(Line::from(Span::styled(tilde.clone(), tilde_style)));
    }

    lines
}

fn syntect_color_to_ratatui(color: syntect::highlighting::Color) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned_chunks(chunks: Vec<(SyntectStyle, &str)>) -> Vec<(SyntectStyle, String)> {
        chunks
            .into_iter()
            .map(|(style, text)| (style, text.to_string()))
            .collect()
    }

    // A window that opens inside a multi-line block comment must be colored as a
    // comment, i.e. identically to highlighting the whole file up to that line —
    // not as if the file began at the window start.
    #[test]
    fn preview_window_inside_block_comment_uses_carried_over_state() {
        // Lines 0..150 sit inside an open block comment; 150.. is real code.
        let mut source = String::from("/* opening of a long block comment\n");
        for i in 1..150 {
            source.push_str(&format!("comment body line {i}\n"));
        }
        source.push_str("*/\n");
        for i in 0..50 {
            source.push_str(&format!("fn f{i}() {{}}\n"));
        }

        let path =
            std::env::temp_dir().join(format!("symtree_preview_ctx_{}.rs", std::process::id()));
        std::fs::write(&path, &source).expect("write temp source");

        let cache = compute_preview(&PreviewRequest {
            path: path.clone(),
            target_line: 130,
            window: 10,
        });
        std::fs::remove_file(&path).ok();

        let start = cache.window_start;
        assert!(
            start > 0,
            "test needs a window that does not start at line 0"
        );
        assert!(start < 150, "window must start inside the block comment");

        let highlighted = cache.highlighted.expect("highlighted lines");

        // Truth: highlight the whole file with full carried-over state.
        let syntax = SYNTAX_SET.find_syntax_by_extension("rs").unwrap();
        let lines: Vec<&str> = source.lines().collect();
        let mut full = HighlightLines::new(syntax, &THEME);
        let expected: Vec<Vec<(SyntectStyle, String)>> = lines
            .iter()
            .map(|line| owned_chunks(full.highlight_line(line, &SYNTAX_SET).unwrap()))
            .collect();

        assert_eq!(
            highlighted[0], expected[start],
            "first windowed line should match full-context highlighting"
        );

        // And it must differ from the buggy un-primed coloring (the line treated
        // as if it were the first line of the file), otherwise the test is vacuous.
        let mut naive = HighlightLines::new(syntax, &THEME);
        let naive_first = owned_chunks(naive.highlight_line(lines[start], &SYNTAX_SET).unwrap());
        assert_ne!(
            highlighted[0], naive_first,
            "priming should change the coloring of a line inside a block comment"
        );
    }
}
