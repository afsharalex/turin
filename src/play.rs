//! Interactive TUI playback. Designed in three layers:
//!
//! - `Player` + `Event` + `Outcome`: pure state machine, fully unit-tested.
//! - `load`: reads `.turin/` from disk into a `Player`, also unit-tested.
//! - `render`: pure draw function over a ratatui `Frame`, tested via `TestBackend`.
//!
//! The actual event loop in `cmd::play` is the only untested glue.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SyntectStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::stop;
use crate::tour;

/// One stop, fully loaded into memory and ready to display.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields populated by the loader / consumed by the renderer (not yet built)
pub struct LoadedStop {
    pub title: String,
    pub body: String,
    pub file: PathBuf,
    pub file_text: String,
    /// 1-based line in `file_text` where the anchor resolved, or `None` if it failed.
    pub anchor_line: Option<usize>,
    /// Optional N-line highlight from frontmatter.
    pub highlight_lines: Option<usize>,
    /// Per-stop diagnostics surfaced to the user (e.g. tree-sitter fallbacks).
    pub warnings: Vec<String>,
}

/// In-memory state of an active play session.
#[derive(Debug)]
pub struct Player {
    pub stops: Vec<LoadedStop>,
    pub index: usize,
    /// Vertical offset of the code pane, in lines.
    pub scroll: usize,
}

/// Lines of context to show *above* the anchor when auto-positioning the
/// code pane on navigation.
pub const CONTEXT_ABOVE: usize = 5;

/// User intents recognized by the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Constructed by `cmd::play` (not yet wired up).
pub enum Event {
    Next,
    Prev,
    Goto(usize),
    ScrollDown,
    ScrollUp,
    Quit,
}

/// Whether the event loop should keep running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Exit,
}

impl Player {
    /// Initial scroll offset that puts the stop's anchor `CONTEXT_ABOVE`
    /// lines from the top of the code pane. Returns 0 when there's no
    /// resolved anchor.
    pub fn auto_scroll(&self, idx: usize) -> usize {
        self.stops
            .get(idx)
            .and_then(|s| s.anchor_line)
            .map(|line| line.saturating_sub(CONTEXT_ABOVE + 1))
            .unwrap_or(0)
    }

    /// Apply an event to the state, returning what the event loop should do next.
    #[allow(dead_code)] // Reachable once `cmd::play` is wired up.
    pub fn handle(&mut self, ev: Event) -> Outcome {
        match ev {
            Event::Next => {
                if self.index + 1 < self.stops.len() {
                    self.index += 1;
                    self.scroll = self.auto_scroll(self.index);
                }
                Outcome::Continue
            }
            Event::Prev => {
                if self.index > 0 {
                    self.index -= 1;
                    self.scroll = self.auto_scroll(self.index);
                }
                Outcome::Continue
            }
            Event::Goto(i) => {
                if i < self.stops.len() {
                    self.index = i;
                    self.scroll = self.auto_scroll(self.index);
                }
                Outcome::Continue
            }
            Event::ScrollDown => {
                self.scroll += 1;
                Outcome::Continue
            }
            Event::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(1);
                Outcome::Continue
            }
            Event::Quit => Outcome::Exit,
        }
    }
}

/// Read `.turin/` from disk and produce a fully-loaded `Player`.
/// `turin_dir` is the `.turin/` directory; the project root (where `file`
/// paths in stop frontmatter resolve from) is its parent.
#[allow(dead_code)] // Reachable once `cmd::play` is wired up.
pub fn load(turin_dir: &Path) -> Player {
    let tour = tour::read(turin_dir);
    let project_root = turin_dir.parent().unwrap_or(Path::new("."));

    let stops = tour
        .stops
        .iter()
        .map(|filename| load_stop(turin_dir, project_root, filename))
        .collect();

    let mut player = Player {
        stops,
        index: 0,
        scroll: 0,
    };
    player.scroll = player.auto_scroll(0);
    player
}

fn load_stop(turin_dir: &Path, project_root: &Path, filename: &str) -> LoadedStop {
    let stop_path = turin_dir.join(filename);
    let (fm, body) = stop::parse(&stop_path);

    let source_path = project_root.join(&fm.file);
    let file_text = fs::read_to_string(&source_path).unwrap_or_default();

    let (anchor_line, anchor_warning) = resolve_anchor(&fm.anchor, &file_text);
    let highlight_lines = extract_highlight_lines(fm.highlight.as_ref());

    let title = fm
        .title
        .clone()
        .unwrap_or_else(|| filename.trim_end_matches(".md").to_string());

    let mut warnings = Vec::new();
    if let Some(w) = anchor_warning {
        warnings.push(w);
    }

    LoadedStop {
        title,
        body,
        file: PathBuf::from(&fm.file),
        file_text,
        anchor_line,
        highlight_lines,
        warnings,
    }
}

/// Resolve an anchor against the source text, returning a 1-based line
/// number (or `None` if unresolved) and an optional warning to surface to
/// the user.
fn resolve_anchor(anchor: &stop::Anchor, file_text: &str) -> (Option<usize>, Option<String>) {
    match anchor.kind.as_str() {
        "line" => {
            let n = anchor.value.as_ref().and_then(|v| v.as_integer());
            (n.and_then(|n| usize::try_from(n).ok()), None)
        }
        "pattern" => {
            let line = anchor
                .value
                .as_ref()
                .and_then(|v| v.as_str())
                .and_then(|pat| regex_find_line(pat, file_text));
            (line, None)
        }
        "treesitter" => resolve_treesitter_fallback(anchor, file_text),
        other => (None, Some(format!("unknown anchor kind: {}", other))),
    }
}

/// CLI player has no bundled tree-sitter grammars. Fall back to the longest
/// double-quoted literal in the query (typically the identifier from
/// `(#eq? @cap "name")`) and pattern-match it as a literal regex.
fn resolve_treesitter_fallback(
    anchor: &stop::Anchor,
    file_text: &str,
) -> (Option<usize>, Option<String>) {
    let query = match anchor.query.as_deref() {
        Some(q) => q,
        None => {
            return (
                None,
                Some("tree-sitter anchor missing `query` field".into()),
            );
        }
    };
    let needle = extract_query_literals(query)
        .into_iter()
        .max_by_key(|s| s.len());
    match needle {
        Some(n) if !n.is_empty() => {
            let line = regex_find_line(&regex::escape(n), file_text);
            let warning = format!(
                "tree-sitter not bundled — fell back to literal match on \"{}\"",
                n
            );
            (line, Some(warning))
        }
        _ => (
            None,
            Some("tree-sitter not bundled and no quoted literal in query for fallback".into()),
        ),
    }
}

fn regex_find_line(pat: &str, file_text: &str) -> Option<usize> {
    let re = regex::Regex::new(pat).ok()?;
    let m = re.find(file_text)?;
    Some(
        file_text[..m.start()]
            .bytes()
            .filter(|b| *b == b'\n')
            .count()
            + 1,
    )
}

/// Extract every substring between unescaped double quotes from a tree-sitter query.
fn extract_query_literals(query: &str) -> Vec<&str> {
    let bytes = query.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b'"' {
                if bytes[j] == b'\\' && j + 1 < bytes.len() {
                    j += 2;
                } else {
                    j += 1;
                }
            }
            if j < bytes.len() {
                if let Ok(s) = std::str::from_utf8(&bytes[start..j]) {
                    result.push(s);
                }
                i = j + 1;
                continue;
            } else {
                break;
            }
        }
        i += 1;
    }
    result
}

fn extract_highlight_lines(highlight: Option<&toml::Value>) -> Option<usize> {
    let n = highlight?.get("lines")?.as_integer()?;
    usize::try_from(n).ok()
}

fn syntax_set() -> &'static SyntaxSet {
    static CELL: OnceLock<SyntaxSet> = OnceLock::new();
    CELL.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme() -> &'static Theme {
    static CELL: OnceLock<Theme> = OnceLock::new();
    CELL.get_or_init(|| {
        let ts = ThemeSet::load_defaults();
        ts.themes
            .get("base16-ocean.dark")
            .or_else(|| ts.themes.values().next())
            .cloned()
            .expect("syntect ships with at least one default theme")
    })
}

/// Convert a syntect color into the closest ratatui color we can express.
/// Syntect emits 24-bit RGB; ratatui supports 24-bit Rgb directly.
fn syntect_fg_to_ratatui(c: syntect::highlighting::Color) -> ratatui::style::Color {
    ratatui::style::Color::Rgb(c.r, c.g, c.b)
}

/// Build the styled lines for the code pane. Lines are syntax-highlighted via
/// syntect; lines in the anchor's highlight range get a distinct background
/// layered on top of the syntax colors. Each line is prefixed with a 4-wide
/// right-aligned line number and a vertical bar separator.
fn build_code_lines(stop: &LoadedStop) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};

    let highlight = highlight_range(stop);
    let gutter_style = Style::default().fg(Color::DarkGray);

    let ss = syntax_set();
    let syntax = stop
        .file
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|ext| ss.find_syntax_by_extension(ext))
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut h = HighlightLines::new(syntax, theme());

    stop.file_text
        .lines()
        .enumerate()
        .map(|(i, text)| {
            let line_no = i + 1;
            let is_highlighted = highlight
                .as_ref()
                .map(|r| r.contains(&line_no))
                .unwrap_or(false);

            let regions: Vec<(SyntectStyle, &str)> = h.highlight_line(text, ss).unwrap_or_default();

            let mut spans = Vec::with_capacity(regions.len() + 1);
            spans.push(Span::styled(format!("{:>4} │ ", line_no), gutter_style));
            for (sty, segment) in regions {
                let mut s = Style::default().fg(syntect_fg_to_ratatui(sty.foreground));
                if is_highlighted {
                    s = s.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
                }
                spans.push(Span::styled(segment.to_string(), s));
            }
            // If the line was empty, syntect may have returned no regions —
            // make sure we still produce a styled span so the highlight bg
            // applies to the whole row.
            if spans.len() == 1 && is_highlighted {
                spans.push(Span::styled(
                    String::new(),
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            Line::from(spans)
        })
        .collect()
}

/// Inclusive-exclusive line range to highlight, or `None` when the anchor
/// is unresolved.
fn highlight_range(stop: &LoadedStop) -> Option<std::ops::Range<usize>> {
    let start = stop.anchor_line?;
    let n = stop.highlight_lines.unwrap_or(1).max(1);
    Some(start..start + n)
}

/// Translate a crossterm key event into a player `Event`. Returns `None`
/// for keys we don't recognize, so the event loop can ignore them.
#[allow(dead_code)] // Reachable once `cmd::play` is wired up.
pub fn key_to_event(key: crossterm::event::KeyEvent) -> Option<Event> {
    use crossterm::event::{KeyCode, KeyModifiers};
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => Some(Event::Quit),
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(Event::Quit),
        (KeyCode::Char('n'), _) | (KeyCode::Char(']'), _) => Some(Event::Next),
        (KeyCode::Char('p'), _) | (KeyCode::Char('['), _) => Some(Event::Prev),
        (KeyCode::Char('j'), _) | (KeyCode::Down, _) => Some(Event::ScrollDown),
        (KeyCode::Char('k'), _) | (KeyCode::Up, _) => Some(Event::ScrollUp),
        (KeyCode::Char(c), _) if ('1'..='9').contains(&c) => {
            Some(Event::Goto((c as u8 - b'1') as usize))
        }
        _ => None,
    }
}

/// Draw the player UI into the given ratatui frame.
///
/// Layout:
///   ┌──────────────┬───────────┐
///   │ code pane    │ commentary│
///   ├──────────────┴───────────┤
///   │ status bar (1 line)      │
///   └──────────────────────────┘
#[allow(dead_code)] // Reachable once `cmd::play` is wired up.
pub fn render(state: &Player, frame: &mut ratatui::Frame) {
    use ratatui::layout::{Constraint, Layout};
    use ratatui::widgets::{Block, Paragraph, Wrap};

    let area = frame.area();
    let outer = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    let panes = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(outer[0]);
    let code_pane = panes[0];
    let body_pane = panes[1];
    let status_area = outer[1];

    let stop = state.stops.get(state.index);

    let file_label = stop
        .map(|s| s.file.display().to_string())
        .unwrap_or_default();
    let code_lines = stop.map(build_code_lines).unwrap_or_default();
    let code = Paragraph::new(ratatui::text::Text::from(code_lines))
        .block(Block::bordered().title(file_label))
        .scroll((state.scroll as u16, 0));
    frame.render_widget(code, code_pane);

    let body_title = stop
        .map(|s| s.title.as_str())
        .unwrap_or("(no stops)")
        .to_string();
    let body_text = stop.map(|s| s.body.as_str()).unwrap_or("");
    let body = Paragraph::new(body_text)
        .block(Block::bordered().title(body_title))
        .wrap(Wrap { trim: false });
    frame.render_widget(body, body_pane);

    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};

    let (nav, progress) = if state.stops.is_empty() {
        ("no stops — q quit".to_string(), String::new())
    } else {
        (
            "n/] next  p/[ prev  j/k scroll  q quit".to_string(),
            format!("{} / {}", state.index + 1, state.stops.len()),
        )
    };

    let warning = stop.and_then(|s| s.warnings.first()).cloned();

    let mut left_spans = vec![Span::raw(nav)];
    if let Some(w) = warning {
        left_spans.push(Span::raw("  "));
        left_spans.push(Span::styled(
            format!("! {}", w),
            Style::default().fg(Color::Yellow),
        ));
    }

    let progress_width = progress.chars().count() as u16;
    let status_chunks =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(progress_width)])
            .split(status_area);

    frame.render_widget(Paragraph::new(Line::from(left_spans)), status_chunks[0]);
    if progress_width > 0 {
        frame.render_widget(Paragraph::new(progress), status_chunks[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_stop(title: &str) -> LoadedStop {
        LoadedStop {
            title: title.into(),
            body: String::new(),
            file: PathBuf::from("foo.rs"),
            file_text: String::new(),
            anchor_line: Some(1),
            highlight_lines: None,
            warnings: Vec::new(),
        }
    }

    fn player_with(n: usize) -> Player {
        Player {
            stops: (0..n).map(|i| fake_stop(&format!("stop {}", i))).collect(),
            index: 0,
            scroll: 0,
        }
    }

    fn empty_player() -> Player {
        Player {
            stops: vec![],
            index: 0,
            scroll: 0,
        }
    }

    #[test]
    fn next_advances_index() {
        let mut p = player_with(3);
        assert_eq!(p.handle(Event::Next), Outcome::Continue);
        assert_eq!(p.index, 1);
    }

    #[test]
    fn next_clamps_at_last_stop() {
        let mut p = player_with(3);
        p.index = 2;
        p.handle(Event::Next);
        assert_eq!(p.index, 2);
    }

    #[test]
    fn next_on_empty_player_is_noop() {
        let mut p = empty_player();
        p.handle(Event::Next);
        assert_eq!(p.index, 0);
    }

    #[test]
    fn prev_retreats_index() {
        let mut p = player_with(3);
        p.index = 2;
        p.handle(Event::Prev);
        assert_eq!(p.index, 1);
    }

    #[test]
    fn prev_clamps_at_zero() {
        let mut p = player_with(3);
        p.handle(Event::Prev);
        assert_eq!(p.index, 0);
    }

    #[test]
    fn navigation_resets_scroll() {
        let mut p = player_with(3);
        p.scroll = 50;
        p.handle(Event::Next);
        assert_eq!(p.scroll, 0);

        p.scroll = 50;
        p.handle(Event::Prev);
        assert_eq!(p.scroll, 0);
    }

    #[test]
    fn goto_jumps_to_index() {
        let mut p = player_with(5);
        p.handle(Event::Goto(3));
        assert_eq!(p.index, 3);
    }

    #[test]
    fn goto_out_of_bounds_is_noop() {
        let mut p = player_with(3);
        p.handle(Event::Goto(99));
        assert_eq!(p.index, 0);
    }

    #[test]
    fn goto_resets_scroll() {
        let mut p = player_with(3);
        p.scroll = 50;
        p.handle(Event::Goto(2));
        assert_eq!(p.scroll, 0);
    }

    #[test]
    fn scroll_down_increments_scroll() {
        let mut p = player_with(1);
        p.handle(Event::ScrollDown);
        assert_eq!(p.scroll, 1);
    }

    #[test]
    fn scroll_up_decrements_scroll() {
        let mut p = player_with(1);
        p.scroll = 5;
        p.handle(Event::ScrollUp);
        assert_eq!(p.scroll, 4);
    }

    #[test]
    fn scroll_up_clamps_at_zero() {
        let mut p = player_with(1);
        p.handle(Event::ScrollUp);
        assert_eq!(p.scroll, 0);
    }

    #[test]
    fn quit_returns_exit_outcome() {
        let mut p = player_with(1);
        assert_eq!(p.handle(Event::Quit), Outcome::Exit);
    }

    // ----- auto-positioning tests -----

    #[test]
    fn auto_scroll_zero_when_anchor_at_top() {
        let p = Player {
            stops: vec![fake_stop("a")], // anchor_line: Some(1)
            index: 0,
            scroll: 0,
        };
        assert_eq!(p.auto_scroll(0), 0);
    }

    #[test]
    fn auto_scroll_leaves_context_above_anchor() {
        let mut s = fake_stop("a");
        s.anchor_line = Some(50);
        let p = Player {
            stops: vec![s],
            index: 0,
            scroll: 0,
        };
        // anchor at line 50 with CONTEXT_ABOVE=5 means the pane skips the
        // first (50 - 5 - 1) = 44 lines, putting line 50 at row 5 of the viewport.
        assert_eq!(p.auto_scroll(0), 50 - CONTEXT_ABOVE - 1);
    }

    #[test]
    fn auto_scroll_zero_when_anchor_unresolved() {
        let mut s = fake_stop("a");
        s.anchor_line = None;
        let p = Player {
            stops: vec![s],
            index: 0,
            scroll: 0,
        };
        assert_eq!(p.auto_scroll(0), 0);
    }

    #[test]
    fn next_auto_positions_scroll_for_new_stop() {
        let mut a = fake_stop("a");
        a.anchor_line = Some(10);
        let mut b = fake_stop("b");
        b.anchor_line = Some(80);
        let mut p = Player {
            stops: vec![a, b],
            index: 0,
            scroll: 0,
        };
        p.handle(Event::Next);
        assert_eq!(p.scroll, 80 - CONTEXT_ABOVE - 1);
    }

    #[test]
    fn prev_auto_positions_scroll_for_new_stop() {
        let mut a = fake_stop("a");
        a.anchor_line = Some(10);
        let mut b = fake_stop("b");
        b.anchor_line = Some(80);
        let mut p = Player {
            stops: vec![a, b],
            index: 1,
            scroll: 80,
        };
        p.handle(Event::Prev);
        assert_eq!(p.scroll, 10 - CONTEXT_ABOVE - 1);
    }

    #[test]
    fn goto_auto_positions_scroll_for_target_stop() {
        let mut stops: Vec<LoadedStop> = (0..5).map(|i| fake_stop(&format!("{}", i))).collect();
        stops[3].anchor_line = Some(100);
        let mut p = Player {
            stops,
            index: 0,
            scroll: 0,
        };
        p.handle(Event::Goto(3));
        assert_eq!(p.scroll, 100 - CONTEXT_ABOVE - 1);
    }

    #[test]
    fn non_quit_events_return_continue() {
        let mut p = player_with(2);
        assert_eq!(p.handle(Event::Next), Outcome::Continue);
        assert_eq!(p.handle(Event::Prev), Outcome::Continue);
        assert_eq!(p.handle(Event::Goto(1)), Outcome::Continue);
        assert_eq!(p.handle(Event::ScrollDown), Outcome::Continue);
        assert_eq!(p.handle(Event::ScrollUp), Outcome::Continue);
    }

    // ----- loader tests -----

    /// Test fixture: builds a temporary project with a `.turin/` directory.
    struct LoadFixture {
        _tmp: tempfile::TempDir,
        turin_dir: PathBuf,
        project_root: PathBuf,
    }

    impl LoadFixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let project_root = tmp.path().to_path_buf();
            let turin_dir = project_root.join(".turin");
            std::fs::create_dir_all(&turin_dir).unwrap();
            Self {
                _tmp: tmp,
                turin_dir,
                project_root,
            }
        }

        fn write_source(&self, rel: &str, content: &str) {
            let path = self.project_root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }

        fn write_stop(&self, filename: &str, frontmatter: &str, body: &str) {
            let content = format!("---\n{}\n---\n\n{}", frontmatter.trim(), body);
            std::fs::write(self.turin_dir.join(filename), content).unwrap();
        }

        fn write_tour(&self, stops: &[&str]) {
            let json = serde_json::json!({
                "tour": { "title": "T" },
                "stops": stops,
            });
            std::fs::write(
                self.turin_dir.join("tour.json"),
                serde_json::to_string_pretty(&json).unwrap(),
            )
            .unwrap();
        }

        fn load(&self) -> Player {
            super::load(&self.turin_dir)
        }
    }

    #[test]
    fn load_empty_tour_yields_no_stops() {
        let f = LoadFixture::new();
        f.write_tour(&[]);
        let p = f.load();
        assert_eq!(p.stops.len(), 0);
        assert_eq!(p.index, 0);
        assert_eq!(p.scroll, 0);
    }

    #[test]
    fn load_populates_title_and_body_from_frontmatter() {
        let f = LoadFixture::new();
        f.write_source("src/lib.rs", "fn main() {}\n");
        f.write_stop(
            "entry.md",
            r#"file = "src/lib.rs"
anchor = { kind = "line", value = 1 }
title = "Entry""#,
            "The body of the stop.\nMultiple lines.\n",
        );
        f.write_tour(&["entry.md"]);

        let p = f.load();
        assert_eq!(p.stops.len(), 1);
        assert_eq!(p.stops[0].title, "Entry");
        assert!(
            p.stops[0].body.contains("body of the stop"),
            "body was: {:?}",
            p.stops[0].body
        );
    }

    #[test]
    fn load_resolves_line_anchor_directly() {
        let f = LoadFixture::new();
        f.write_source("src/lib.rs", "a\nb\nc\nd\n");
        f.write_stop(
            "s.md",
            r#"file = "src/lib.rs"
anchor = { kind = "line", value = 3 }"#,
            "",
        );
        f.write_tour(&["s.md"]);

        assert_eq!(f.load().stops[0].anchor_line, Some(3));
    }

    #[test]
    fn load_resolves_pattern_anchor_to_first_matching_line() {
        let f = LoadFixture::new();
        f.write_source(
            "src/lib.rs",
            "use std::io;\n\nfn main() {\n    println!(\"hi\");\n}\n",
        );
        f.write_stop(
            "s.md",
            r#"file = "src/lib.rs"
anchor = { kind = "pattern", value = "fn main" }"#,
            "",
        );
        f.write_tour(&["s.md"]);

        assert_eq!(f.load().stops[0].anchor_line, Some(3));
    }

    #[test]
    fn load_marks_unresolved_pattern_as_none() {
        let f = LoadFixture::new();
        f.write_source("src/lib.rs", "irrelevant content\n");
        f.write_stop(
            "s.md",
            r#"file = "src/lib.rs"
anchor = { kind = "pattern", value = "nonexistent_marker_xyz" }"#,
            "",
        );
        f.write_tour(&["s.md"]);

        assert_eq!(f.load().stops[0].anchor_line, None);
    }

    #[test]
    fn load_reads_referenced_source_file_into_file_text() {
        let f = LoadFixture::new();
        f.write_source("src/lib.rs", "specific marker text 12345\n");
        f.write_stop(
            "s.md",
            r#"file = "src/lib.rs"
anchor = { kind = "line", value = 1 }"#,
            "",
        );
        f.write_tour(&["s.md"]);

        assert!(
            f.load().stops[0]
                .file_text
                .contains("specific marker text 12345")
        );
    }

    #[test]
    fn load_passes_through_highlight_lines() {
        let f = LoadFixture::new();
        f.write_source("src/lib.rs", "x\n");
        f.write_stop(
            "s.md",
            r#"file = "src/lib.rs"
anchor = { kind = "line", value = 1 }
highlight = { lines = 8 }"#,
            "",
        );
        f.write_tour(&["s.md"]);

        assert_eq!(f.load().stops[0].highlight_lines, Some(8));
    }

    #[test]
    fn load_falls_back_to_filename_slug_when_title_missing() {
        let f = LoadFixture::new();
        f.write_source("src/lib.rs", "x\n");
        f.write_stop(
            "untitled-stop.md",
            r#"file = "src/lib.rs"
anchor = { kind = "line", value = 1 }"#,
            "",
        );
        f.write_tour(&["untitled-stop.md"]);

        let p = f.load();
        assert!(
            p.stops[0].title.contains("untitled"),
            "expected title to fall back to filename, got {:?}",
            p.stops[0].title
        );
    }

    // ----- renderer tests -----

    fn render_to_buffer(state: &Player, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| super::render(state, f)).unwrap();
        term.backend().buffer().clone()
    }

    fn buf_to_string(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn make_loaded_stop(title: &str) -> LoadedStop {
        LoadedStop {
            title: title.into(),
            body: String::new(),
            file: PathBuf::from("src/foo.rs"),
            file_text: String::new(),
            anchor_line: Some(1),
            highlight_lines: None,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn render_shows_stop_title_in_right_pane() {
        let p = Player {
            stops: vec![make_loaded_stop("Entry point")],
            index: 0,
            scroll: 0,
        };
        let buf = render_to_buffer(&p, 80, 20);
        let s = buf_to_string(&buf);
        assert!(s.contains("Entry point"), "buffer:\n{}", s);
    }

    #[test]
    fn render_shows_file_path_in_left_pane_header() {
        let mut stop = make_loaded_stop("X");
        stop.file = PathBuf::from("src/parser/lexer.rs");
        let p = Player {
            stops: vec![stop],
            index: 0,
            scroll: 0,
        };
        let buf = render_to_buffer(&p, 80, 20);
        let s = buf_to_string(&buf);
        assert!(s.contains("src/parser/lexer.rs"), "buffer:\n{}", s);
    }

    #[test]
    fn render_shows_progress_in_status_bar() {
        let stops = (0..3)
            .map(|i| make_loaded_stop(&format!("Stop {}", i)))
            .collect();
        let p = Player {
            stops,
            index: 1,
            scroll: 0,
        };
        let buf = render_to_buffer(&p, 80, 20);
        let s = buf_to_string(&buf);
        assert!(s.contains("2 / 3"), "buffer:\n{}", s);
    }

    #[test]
    fn render_status_bar_includes_key_hints() {
        let p = Player {
            stops: vec![make_loaded_stop("X")],
            index: 0,
            scroll: 0,
        };
        let buf = render_to_buffer(&p, 80, 20);
        let s = buf_to_string(&buf);
        assert!(
            s.contains("next") && s.contains("prev") && s.contains("quit"),
            "buffer:\n{}",
            s
        );
        // The bracket-key aliases should be shown alongside n and p.
        assert!(s.contains("n/]"), "expected n/] alias, buffer:\n{}", s);
        assert!(s.contains("p/["), "expected p/[ alias, buffer:\n{}", s);
    }

    #[test]
    fn render_progress_right_aligned_in_status_bar() {
        let stops: Vec<LoadedStop> = (0..3)
            .map(|i| make_loaded_stop(&format!("Stop {}", i)))
            .collect();
        let p = Player {
            stops,
            index: 1,
            scroll: 0,
        };
        let buf = render_to_buffer(&p, 80, 10);
        let last_row_y = buf.area.height - 1;
        let last_row: String = (0..buf.area.width)
            .map(|x| buf[(x, last_row_y)].symbol().to_string())
            .collect::<Vec<_>>()
            .concat();
        assert!(
            last_row.trim_end().ends_with("2 / 3"),
            "expected status bar to end with progress; row was:\n{:?}",
            last_row
        );
    }

    #[test]
    fn render_displays_body_text() {
        let mut stop = make_loaded_stop("X");
        stop.body = "specific body marker xyz".into();
        let p = Player {
            stops: vec![stop],
            index: 0,
            scroll: 0,
        };
        let buf = render_to_buffer(&p, 80, 20);
        let s = buf_to_string(&buf);
        assert!(s.contains("specific body marker"), "buffer:\n{}", s);
    }

    #[test]
    fn render_displays_source_code_text() {
        let mut stop = make_loaded_stop("X");
        stop.file_text = "fn unique_marker_fn() {}\n".into();
        let p = Player {
            stops: vec![stop],
            index: 0,
            scroll: 0,
        };
        let buf = render_to_buffer(&p, 80, 20);
        let s = buf_to_string(&buf);
        assert!(s.contains("unique_marker_fn"), "buffer:\n{}", s);
    }

    // ----- key translation tests -----

    fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    fn ctrl_key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::CONTROL)
    }

    #[test]
    fn key_q_is_quit() {
        use crossterm::event::KeyCode;
        assert_eq!(key_to_event(key(KeyCode::Char('q'))), Some(Event::Quit));
    }

    #[test]
    fn key_esc_is_quit() {
        use crossterm::event::KeyCode;
        assert_eq!(key_to_event(key(KeyCode::Esc)), Some(Event::Quit));
    }

    #[test]
    fn key_ctrl_c_is_quit() {
        use crossterm::event::KeyCode;
        assert_eq!(
            key_to_event(ctrl_key(KeyCode::Char('c'))),
            Some(Event::Quit)
        );
    }

    #[test]
    fn key_n_and_bracket_are_next() {
        use crossterm::event::KeyCode;
        assert_eq!(key_to_event(key(KeyCode::Char('n'))), Some(Event::Next));
        assert_eq!(key_to_event(key(KeyCode::Char(']'))), Some(Event::Next));
    }

    #[test]
    fn key_p_and_bracket_are_prev() {
        use crossterm::event::KeyCode;
        assert_eq!(key_to_event(key(KeyCode::Char('p'))), Some(Event::Prev));
        assert_eq!(key_to_event(key(KeyCode::Char('['))), Some(Event::Prev));
    }

    #[test]
    fn key_j_and_down_scroll_down() {
        use crossterm::event::KeyCode;
        assert_eq!(
            key_to_event(key(KeyCode::Char('j'))),
            Some(Event::ScrollDown)
        );
        assert_eq!(key_to_event(key(KeyCode::Down)), Some(Event::ScrollDown));
    }

    #[test]
    fn key_k_and_up_scroll_up() {
        use crossterm::event::KeyCode;
        assert_eq!(key_to_event(key(KeyCode::Char('k'))), Some(Event::ScrollUp));
        assert_eq!(key_to_event(key(KeyCode::Up)), Some(Event::ScrollUp));
    }

    #[test]
    fn digits_jump_to_zero_indexed_stop() {
        use crossterm::event::KeyCode;
        assert_eq!(key_to_event(key(KeyCode::Char('1'))), Some(Event::Goto(0)));
        assert_eq!(key_to_event(key(KeyCode::Char('5'))), Some(Event::Goto(4)));
        assert_eq!(key_to_event(key(KeyCode::Char('9'))), Some(Event::Goto(8)));
    }

    #[test]
    fn unrecognized_keys_return_none() {
        use crossterm::event::KeyCode;
        assert_eq!(key_to_event(key(KeyCode::Char('z'))), None);
        assert_eq!(key_to_event(key(KeyCode::F(1))), None);
    }

    /// Count rows in the buffer that contain at least one cell with the given
    /// background color, restricted to x < `cutoff_x` (the code pane).
    fn rows_with_bg(
        buf: &ratatui::buffer::Buffer,
        bg: ratatui::style::Color,
        cutoff_x: u16,
    ) -> Vec<u16> {
        let area = buf.area;
        (0..area.height)
            .filter(|y| (0..cutoff_x.min(area.width)).any(|x| buf[(x, *y)].bg == bg))
            .collect()
    }

    #[test]
    fn render_highlights_anchor_line_with_distinct_bg() {
        let mut stop = make_loaded_stop("X");
        stop.file_text = (1..=30).map(|i| format!("line {}\n", i)).collect();
        stop.anchor_line = Some(10);
        stop.highlight_lines = None; // default = 1 line
        let p = Player {
            stops: vec![stop],
            index: 0,
            scroll: 0,
        };
        let buf = render_to_buffer(&p, 80, 20);
        let highlighted = rows_with_bg(&buf, ratatui::style::Color::DarkGray, 48);
        assert_eq!(
            highlighted.len(),
            1,
            "expected exactly one highlighted row, got {:?}\n{}",
            highlighted,
            buf_to_string(&buf)
        );
    }

    #[test]
    fn render_highlights_full_region_when_highlight_lines_set() {
        let mut stop = make_loaded_stop("X");
        stop.file_text = (1..=30).map(|i| format!("line {}\n", i)).collect();
        stop.anchor_line = Some(10);
        stop.highlight_lines = Some(4);
        let p = Player {
            stops: vec![stop],
            index: 0,
            scroll: 0,
        };
        let buf = render_to_buffer(&p, 80, 20);
        let highlighted = rows_with_bg(&buf, ratatui::style::Color::DarkGray, 48);
        assert_eq!(
            highlighted.len(),
            4,
            "expected 4 highlighted rows, got {:?}\n{}",
            highlighted,
            buf_to_string(&buf)
        );
    }

    #[test]
    fn render_no_highlight_when_anchor_unresolved() {
        let mut stop = make_loaded_stop("X");
        stop.file_text = "a\nb\nc\n".into();
        stop.anchor_line = None;
        let p = Player {
            stops: vec![stop],
            index: 0,
            scroll: 0,
        };
        let buf = render_to_buffer(&p, 80, 20);
        let highlighted = rows_with_bg(&buf, ratatui::style::Color::DarkGray, 48);
        assert!(
            highlighted.is_empty(),
            "expected no highlight, got rows {:?}",
            highlighted
        );
    }

    #[test]
    fn render_shows_first_warning_in_status_bar() {
        let mut s = make_loaded_stop("X");
        s.warnings = vec!["fell back to literal match on \"tokenize\"".into()];
        let p = Player {
            stops: vec![s],
            index: 0,
            scroll: 0,
        };
        let buf = render_to_buffer(&p, 120, 20);
        let s = buf_to_string(&buf);
        assert!(s.contains("fell back to literal match"), "buffer:\n{}", s);
    }

    #[test]
    fn render_includes_line_numbers_in_code_pane() {
        let mut stop = make_loaded_stop("X");
        stop.file_text = "alpha\nbeta\ngamma\n".into();
        let p = Player {
            stops: vec![stop],
            index: 0,
            scroll: 0,
        };
        let buf = render_to_buffer(&p, 80, 20);
        let s = buf_to_string(&buf);
        // Each line should be preceded by its 1-based line number and a separator.
        assert!(s.contains("1 │ alpha"), "buffer:\n{}", s);
        assert!(s.contains("2 │ beta"), "buffer:\n{}", s);
        assert!(s.contains("3 │ gamma"), "buffer:\n{}", s);
    }

    #[test]
    fn render_handles_empty_player_without_panic() {
        let p = Player {
            stops: vec![],
            index: 0,
            scroll: 0,
        };
        let buf = render_to_buffer(&p, 80, 20);
        let s = buf_to_string(&buf);
        // Renderer must convey that there's nothing to play.
        assert!(s.to_lowercase().contains("no stops"), "buffer:\n{}", s);
    }

    // ----- treesitter fallback -----

    #[test]
    fn extract_query_literals_returns_quoted_substrings() {
        let q = r#"(function_item name: (identifier) @n (#eq? @n "tokenize"))"#;
        let lits = extract_query_literals(q);
        assert_eq!(lits, vec!["tokenize"]);
    }

    #[test]
    fn extract_query_literals_picks_up_multiple_quotes() {
        let q = r#"(#match? @n "foo") (#eq? @n "tokenizer")"#;
        let lits = extract_query_literals(q);
        assert_eq!(lits, vec!["foo", "tokenizer"]);
    }

    #[test]
    fn extract_query_literals_handles_escaped_quotes() {
        let q = r#""abc\"def" "x""#;
        let lits = extract_query_literals(q);
        assert_eq!(lits.len(), 2);
        assert!(lits.iter().any(|s| s.contains("abc")));
    }

    #[test]
    fn load_treesitter_falls_back_to_longest_literal() {
        let f = LoadFixture::new();
        f.write_source("src/lib.rs", "fn helper() {}\nfn tokenize() {}\n");
        f.write_stop(
            "s.md",
            r#"file = "src/lib.rs"
anchor = { kind = "treesitter", query = "(function_item name: (identifier) @n (#eq? @n \"tokenize\"))" }"#,
            "",
        );
        f.write_tour(&["s.md"]);
        let p = f.load();
        assert_eq!(p.stops[0].anchor_line, Some(2));
        assert_eq!(p.stops[0].warnings.len(), 1);
        assert!(
            p.stops[0].warnings[0].contains("tree-sitter not bundled"),
            "warning was: {:?}",
            p.stops[0].warnings
        );
        assert!(p.stops[0].warnings[0].contains("tokenize"));
    }

    #[test]
    fn load_treesitter_with_no_quoted_literal_warns_and_returns_none() {
        let f = LoadFixture::new();
        f.write_source("src/lib.rs", "x\n");
        f.write_stop(
            "s.md",
            r#"file = "src/lib.rs"
anchor = { kind = "treesitter", query = "(identifier)" }"#,
            "",
        );
        f.write_tour(&["s.md"]);
        let p = f.load();
        assert_eq!(p.stops[0].anchor_line, None);
        assert_eq!(p.stops[0].warnings.len(), 1);
        assert!(p.stops[0].warnings[0].contains("no quoted literal"));
    }

    #[test]
    fn load_unknown_anchor_kind_warns() {
        let f = LoadFixture::new();
        f.write_source("src/lib.rs", "x\n");
        f.write_stop(
            "s.md",
            r#"file = "src/lib.rs"
anchor = { kind = "magic", value = "x" }"#,
            "",
        );
        f.write_tour(&["s.md"]);
        let p = f.load();
        assert_eq!(p.stops[0].anchor_line, None);
        assert!(
            p.stops[0]
                .warnings
                .first()
                .map(|w| w.contains("unknown anchor kind"))
                .unwrap_or(false)
        );
    }

    #[test]
    fn load_sets_initial_scroll_to_auto_position_for_first_stop() {
        let f = LoadFixture::new();
        // 50-line file, anchor at line 30 → scroll should land at 30 - 5 - 1 = 24
        let source: String = (1..=50).map(|i| format!("line {}\n", i)).collect();
        f.write_source("src/lib.rs", &source);
        f.write_stop(
            "s.md",
            r#"file = "src/lib.rs"
anchor = { kind = "line", value = 30 }"#,
            "",
        );
        f.write_tour(&["s.md"]);

        let p = f.load();
        assert_eq!(p.scroll, 30 - CONTEXT_ABOVE - 1);
    }

    #[test]
    fn load_preserves_stop_order_from_index() {
        let f = LoadFixture::new();
        f.write_source("src/lib.rs", "x\n");
        for name in ["a.md", "b.md", "c.md"] {
            f.write_stop(
                name,
                &format!(
                    r#"file = "src/lib.rs"
anchor = {{ kind = "line", value = 1 }}
title = "{}""#,
                    name
                ),
                "",
            );
        }
        f.write_tour(&["c.md", "a.md", "b.md"]);

        let p = f.load();
        let titles: Vec<&str> = p.stops.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["c.md", "a.md", "b.md"]);
    }
}
