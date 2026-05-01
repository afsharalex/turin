//! Interactive TUI playback. Designed in three layers:
//!
//! - `Player` + `Event` + `Outcome`: pure state machine, fully unit-tested.
//! - `load`: reads `.turin/` from disk into a `Player`, also unit-tested.
//! - `render`: pure draw function over a ratatui `Frame`, tested via `TestBackend`.
//!
//! The actual event loop in `cmd::play` is the only untested glue.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use syntect::easy::HighlightLines;
use syntect::highlighting::{HighlightState, Style as SyntectStyle, Theme, ThemeSet};
use syntect::parsing::{ParseState, SyntaxReference, SyntaxSet};

use crate::stop;
use crate::tour;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Code,
    Commentary,
}

/// One stop, fully loaded into memory and ready to display.
#[derive(Debug, Clone)]
pub struct LoadedStop {
    pub title: String,
    pub body: String,
    pub file: PathBuf,
    /// Source text and lazily rendered code-line cache, shared by stops that
    /// reference the same file.
    source: Arc<LoadedSource>,
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
    pub stops: Vec<Option<LoadedStop>>,
    pub index: usize,
    /// Vertical offset of the code pane, in lines.
    pub scroll: usize,
    /// Vertical offset of the commentary pane, in lines.
    pub body_scroll: usize,
    pub active_pane: Pane,
    preload: Option<PreloadState>,
}

#[derive(Clone, Debug)]
struct PreloadState {
    parsed_stops: Arc<Vec<ParsedStop>>,
    project_root: PathBuf,
    sources: SharedSourceCache,
    prepared_stops: SharedStopCache,
    color: bool,
}

type SharedSourceCache = Arc<Mutex<HashMap<String, Arc<LoadedSource>>>>;
type SharedStopCache = Arc<Mutex<Vec<Option<LoadedStop>>>>;

const CHECKPOINT_INTERVAL: usize = 64;
const MAX_RESUME_DISTANCE: usize = 512;

/// Lines of context to show *above* the anchor when auto-positioning the
/// code pane on navigation.
pub const CONTEXT_ABOVE: usize = 5;

/// User intents recognized by the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Next,
    Prev,
    Goto(usize),
    ScrollDown,
    ScrollUp,
    ScrollPaneDown(Pane),
    ScrollPaneUp(Pane),
    Focus(Pane),
    TogglePane,
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
        self.cached_stop(idx)
            .and_then(|s| s.anchor_line)
            .map(|line| line.saturating_sub(CONTEXT_ABOVE + 1))
            .unwrap_or(0)
    }

    fn stop_count(&self) -> usize {
        self.stops.len()
    }

    fn cached_stop(&self, idx: usize) -> Option<LoadedStop> {
        self.stops.get(idx).and_then(Clone::clone).or_else(|| {
            self.preload.as_ref().and_then(|preload| {
                preload
                    .prepared_stops
                    .lock()
                    .ok()
                    .and_then(|stops| stops.get(idx).and_then(Clone::clone))
            })
        })
    }

    fn ensure_prepared(&mut self, idx: usize) {
        if idx >= self.stops.len() || self.stops[idx].is_some() {
            return;
        }

        if let Some(stop) = self.cached_stop(idx) {
            self.stops[idx] = Some(stop);
            return;
        }

        if let Some(preload) = &self.preload {
            let stop = prepare_stop(
                &preload.parsed_stops[idx],
                &preload.project_root,
                &preload.sources,
                preload.color,
            );
            if let Ok(mut stops) = preload.prepared_stops.lock()
                && idx < stops.len()
            {
                stops[idx] = Some(stop.clone());
            }
            self.stops[idx] = Some(stop);
        }
    }

    fn reset_scroll_for_current_stop(&mut self) {
        self.scroll = self.auto_scroll(self.index);
        self.body_scroll = 0;
    }

    fn scroll_pane_down(&mut self, pane: Pane) {
        match pane {
            Pane::Code => self.scroll += 1,
            Pane::Commentary => self.body_scroll += 1,
        }
    }

    fn scroll_pane_up(&mut self, pane: Pane) {
        match pane {
            Pane::Code => self.scroll = self.scroll.saturating_sub(1),
            Pane::Commentary => self.body_scroll = self.body_scroll.saturating_sub(1),
        }
    }

    /// Apply an event to the state, returning what the event loop should do next.
    pub fn handle(&mut self, ev: Event) -> Outcome {
        match ev {
            Event::Next => {
                if self.index + 1 < self.stop_count() {
                    self.index += 1;
                    self.ensure_prepared(self.index);
                    self.reset_scroll_for_current_stop();
                }
                Outcome::Continue
            }
            Event::Prev => {
                if self.index > 0 {
                    self.index -= 1;
                    self.ensure_prepared(self.index);
                    self.reset_scroll_for_current_stop();
                }
                Outcome::Continue
            }
            Event::Goto(i) => {
                if i < self.stop_count() {
                    self.index = i;
                    self.ensure_prepared(self.index);
                    self.reset_scroll_for_current_stop();
                }
                Outcome::Continue
            }
            Event::ScrollDown => {
                self.scroll_pane_down(self.active_pane);
                Outcome::Continue
            }
            Event::ScrollUp => {
                self.scroll_pane_up(self.active_pane);
                Outcome::Continue
            }
            Event::ScrollPaneDown(pane) => {
                self.active_pane = pane;
                self.scroll_pane_down(pane);
                Outcome::Continue
            }
            Event::ScrollPaneUp(pane) => {
                self.active_pane = pane;
                self.scroll_pane_up(pane);
                Outcome::Continue
            }
            Event::Focus(pane) => {
                self.active_pane = pane;
                Outcome::Continue
            }
            Event::TogglePane => {
                self.active_pane = match self.active_pane {
                    Pane::Code => Pane::Commentary,
                    Pane::Commentary => Pane::Code,
                };
                Outcome::Continue
            }
            Event::Quit => Outcome::Exit,
        }
    }
}

/// Read `.turin/` from disk and produce a fully-loaded `Player`.
/// `turin_dir` is the `.turin/` directory; the project root (where `file`
/// paths in stop frontmatter resolve from) is its parent.
pub fn load(turin_dir: &Path, color: bool) -> Player {
    let tour = tour::read(turin_dir);
    let project_root = turin_dir.parent().unwrap_or(Path::new("."));

    let parsed_stops: Vec<_> = tour
        .stops
        .iter()
        .map(|filename| parse_stop(turin_dir, filename))
        .collect();
    let parsed_stops = Arc::new(parsed_stops);
    let sources = Arc::new(Mutex::new(HashMap::new()));
    let prepared_stops = Arc::new(Mutex::new(vec![None; parsed_stops.len()]));

    let mut stops = vec![None; parsed_stops.len()];
    if !parsed_stops.is_empty() {
        let first = prepare_stop(&parsed_stops[0], project_root, &sources, color);
        if let Ok(mut prepared) = prepared_stops.lock() {
            prepared[0] = Some(first.clone());
        }
        stops[0] = Some(first);
    }

    let preload = PreloadState {
        parsed_stops,
        project_root: project_root.to_path_buf(),
        sources,
        prepared_stops,
        color,
    };
    start_background_preload(preload.clone(), 1);

    let mut player = Player {
        stops,
        index: 0,
        scroll: 0,
        body_scroll: 0,
        active_pane: Pane::Code,
        preload: Some(preload),
    };
    player.scroll = player.auto_scroll(0);
    player
}

#[derive(Debug)]
struct ParsedStop {
    filename: String,
    frontmatter: stop::StopFrontmatter,
    body: String,
}

#[derive(Debug)]
struct LoadedSource {
    file_text: String,
    line_ranges: Vec<SourceLineRange>,
    rendered_lines: Mutex<HashMap<usize, ratatui::text::Line<'static>>>,
    checkpoints: Mutex<BTreeMap<usize, HighlightCheckpoint>>,
    color: bool,
}

#[derive(Debug, Clone, Copy)]
struct SourceLineRange {
    full_start: usize,
    content_end: usize,
    full_end: usize,
}

#[derive(Debug, Clone)]
struct HighlightCheckpoint {
    highlight_state: HighlightState,
    parse_state: ParseState,
}

fn parse_stop(turin_dir: &Path, filename: &str) -> ParsedStop {
    let stop_path = turin_dir.join(filename);
    let (fm, body) = stop::parse(&stop_path);

    ParsedStop {
        filename: filename.to_string(),
        frontmatter: fm,
        body,
    }
}

fn start_background_preload(preload: PreloadState, start_idx: usize) {
    thread::spawn(move || {
        for idx in start_idx..preload.parsed_stops.len() {
            let already_prepared = preload
                .prepared_stops
                .lock()
                .ok()
                .and_then(|stops| stops.get(idx).map(Option::is_some))
                .unwrap_or(false);
            if already_prepared {
                continue;
            }

            let stop = prepare_stop(
                &preload.parsed_stops[idx],
                &preload.project_root,
                &preload.sources,
                preload.color,
            );
            if let Ok(mut stops) = preload.prepared_stops.lock()
                && idx < stops.len()
                && stops[idx].is_none()
            {
                stops[idx] = Some(stop);
            }
        }
    });
}

fn prepare_source(
    project_root: &Path,
    file: &str,
    sources: &SharedSourceCache,
    color: bool,
) -> Arc<LoadedSource> {
    if let Ok(cache) = sources.lock()
        && let Some(source) = cache.get(file)
    {
        return Arc::clone(source);
    }

    let source_path = project_root.join(file);
    let file_text = fs::read_to_string(&source_path).unwrap_or_default();
    let source = Arc::new(LoadedSource {
        line_ranges: line_ranges(&file_text),
        rendered_lines: Mutex::new(HashMap::new()),
        checkpoints: Mutex::new(BTreeMap::new()),
        color,
        file_text,
    });

    if let Ok(mut cache) = sources.lock() {
        Arc::clone(
            cache
                .entry(file.to_string())
                .or_insert_with(|| Arc::clone(&source)),
        )
    } else {
        source
    }
}

fn prepare_stop(
    parsed: &ParsedStop,
    project_root: &Path,
    sources: &SharedSourceCache,
    color: bool,
) -> LoadedStop {
    let fm = &parsed.frontmatter;
    let source = prepare_source(project_root, &fm.file, sources, color);

    let (anchor_line, anchor_warning) = resolve_anchor(&fm.anchor, &source.file_text);
    let highlight_lines = extract_highlight_lines(fm.highlight.as_ref());

    let title = fm
        .title
        .clone()
        .unwrap_or_else(|| parsed.filename.trim_end_matches(".md").to_string());

    let mut warnings = Vec::new();
    if let Some(w) = anchor_warning {
        warnings.push(w);
    }

    LoadedStop {
        title,
        body: parsed.body.clone(),
        file: PathBuf::from(&fm.file),
        source,
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

fn line_ranges(file_text: &str) -> Vec<SourceLineRange> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for segment in file_text.split_inclusive('\n') {
        let segment_end = start + segment.len();
        let mut content_end = segment_end;
        if segment.ends_with('\n') {
            content_end = content_end.saturating_sub(1);
            if content_end > start && file_text.as_bytes()[content_end - 1] == b'\r' {
                content_end -= 1;
            }
        }
        ranges.push(SourceLineRange {
            full_start: start,
            content_end,
            full_end: segment_end,
        });
        start = segment_end;
    }
    ranges
}

fn source_line(source: &LoadedSource, idx: usize) -> Option<&str> {
    let range = source.line_ranges.get(idx)?;
    source.file_text.get(range.full_start..range.content_end)
}

fn source_line_with_ending(source: &LoadedSource, idx: usize) -> Option<&str> {
    let range = source.line_ranges.get(idx)?;
    source.file_text.get(range.full_start..range.full_end)
}

fn cached_code_lines(
    source: &LoadedSource,
    file: &Path,
    start: usize,
    count: usize,
) -> Vec<ratatui::text::Line<'static>> {
    ensure_code_lines(source, file, start, count);

    let cache = source.rendered_lines.lock().ok();
    (start..source.line_ranges.len())
        .take(count)
        .map(|idx| {
            cache
                .as_ref()
                .and_then(|cache| cache.get(&idx).cloned())
                .unwrap_or_else(|| ratatui::text::Line::from(""))
        })
        .collect()
}

fn ensure_code_lines(source: &LoadedSource, file: &Path, start: usize, count: usize) {
    if count == 0 || start >= source.line_ranges.len() {
        return;
    }

    let end = start.saturating_add(count).min(source.line_ranges.len());
    let missing: Vec<usize> = if let Ok(cache) = source.rendered_lines.lock() {
        (start..end)
            .filter(|idx| !cache.contains_key(idx))
            .collect()
    } else {
        (start..end).collect()
    };
    if missing.is_empty() {
        return;
    }

    if !source.color {
        if let Ok(mut cache) = source.rendered_lines.lock() {
            for idx in missing {
                if let Some(text) = source_line(source, idx) {
                    cache
                        .entry(idx)
                        .or_insert_with(|| build_plain_code_line(idx + 1, text));
                }
            }
        }
        return;
    }

    highlight_code_range(source, file, missing[0], *missing.last().unwrap());
}

fn highlight_code_range(
    source: &LoadedSource,
    file: &Path,
    first_missing: usize,
    last_needed: usize,
) {
    let ss = syntax_set();
    let syntax = syntax_for_file(file, ss);
    let (resume_line, checkpoint) = nearest_checkpoint(source, syntax, first_missing);
    let mut highlighter =
        HighlightLines::from_state(theme(), checkpoint.highlight_state, checkpoint.parse_state);

    for idx in resume_line..=last_needed {
        let Some(text_with_ending) = source_line_with_ending(source, idx) else {
            break;
        };
        let text = source_line(source, idx).unwrap_or(text_with_ending);
        let regions: Vec<(SyntectStyle, &str)> = highlighter
            .highlight_line(text_with_ending, ss)
            .unwrap_or_default();
        let line = build_highlighted_code_line(idx + 1, text, regions);

        if let Ok(mut cache) = source.rendered_lines.lock() {
            cache.entry(idx).or_insert(line);
        }

        if should_store_checkpoint(idx, last_needed) {
            let (highlight_state, parse_state) = highlighter.state();
            let next_line = idx + 1;
            store_checkpoint(
                source,
                next_line,
                HighlightCheckpoint {
                    highlight_state: highlight_state.clone(),
                    parse_state: parse_state.clone(),
                },
            );
            highlighter = HighlightLines::from_state(theme(), highlight_state, parse_state);
        }
    }
}

fn should_store_checkpoint(idx: usize, last_needed: usize) -> bool {
    idx == last_needed || (idx + 1).is_multiple_of(CHECKPOINT_INTERVAL)
}

fn nearest_checkpoint(
    source: &LoadedSource,
    syntax: &SyntaxReference,
    target_line: usize,
) -> (usize, HighlightCheckpoint) {
    if let Ok(checkpoints) = source.checkpoints.lock()
        && let Some((&line, checkpoint)) = checkpoints.range(..=target_line).next_back()
        && target_line.saturating_sub(line) <= MAX_RESUME_DISTANCE
    {
        return (line, checkpoint.clone());
    }

    let fallback_line = target_line.saturating_sub(CHECKPOINT_INTERVAL);
    (fallback_line, initial_checkpoint(syntax))
}

fn store_checkpoint(source: &LoadedSource, line: usize, checkpoint: HighlightCheckpoint) {
    if let Ok(mut checkpoints) = source.checkpoints.lock() {
        checkpoints.entry(line).or_insert(checkpoint);
    }
}

fn initial_checkpoint(syntax: &SyntaxReference) -> HighlightCheckpoint {
    let (highlight_state, parse_state) = HighlightLines::new(syntax, theme()).state();
    HighlightCheckpoint {
        highlight_state,
        parse_state,
    }
}

fn syntax_for_file<'a>(file: &Path, ss: &'a SyntaxSet) -> &'a SyntaxReference {
    file.extension()
        .and_then(|e| e.to_str())
        .and_then(|ext| ss.find_syntax_by_extension(ext))
        .unwrap_or_else(|| ss.find_syntax_plain_text())
}

fn build_plain_code_line(line_no: usize, text: &str) -> ratatui::text::Line<'static> {
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};

    let gutter_style = Style::default().fg(Color::DarkGray);
    Line::from(vec![
        Span::styled(format!("{:>4} │ ", line_no), gutter_style),
        Span::raw(text.to_string()),
    ])
}

fn build_highlighted_code_line(
    line_no: usize,
    text: &str,
    regions: Vec<(SyntectStyle, &str)>,
) -> ratatui::text::Line<'static> {
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};

    let gutter_style = Style::default().fg(Color::DarkGray);
    let mut spans = Vec::with_capacity(regions.len() + 1);
    spans.push(Span::styled(format!("{:>4} │ ", line_no), gutter_style));
    for (sty, segment) in regions {
        let segment = segment.trim_end_matches(['\r', '\n']);
        if segment.is_empty() {
            continue;
        }
        let s = Style::default().fg(syntect_fg_to_ratatui(sty.foreground));
        spans.push(Span::styled(segment.to_string(), s));
    }
    if spans.len() == 1 && !text.is_empty() {
        spans.push(Span::raw(text.to_string()));
    }
    Line::from(spans)
}

fn highlight_code_line(mut line: ratatui::text::Line<'static>) -> ratatui::text::Line<'static> {
    use ratatui::style::{Color, Modifier};

    for span in &mut line.spans {
        span.style = span.style.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
    }
    line
}

/// Inclusive-exclusive line range to highlight, or `None` when the anchor
/// is unresolved.
fn highlight_range(stop: &LoadedStop) -> Option<std::ops::Range<usize>> {
    let start = stop.anchor_line?;
    let n = stop.highlight_lines.unwrap_or(1).max(1);
    Some(start..start + n)
}

fn visible_code_lines(
    stop: &LoadedStop,
    scroll: usize,
    pane_height: u16,
) -> Vec<ratatui::text::Line<'static>> {
    let visible_rows = usize::from(pane_height.saturating_sub(2));
    let highlight = highlight_range(stop);
    cached_code_lines(&stop.source, &stop.file, scroll, visible_rows)
        .into_iter()
        .enumerate()
        .map(|(offset, line)| {
            let line_no = scroll + offset + 1;
            if highlight
                .as_ref()
                .map(|r| r.contains(&line_no))
                .unwrap_or(false)
            {
                highlight_code_line(line)
            } else {
                line
            }
        })
        .collect()
}

/// Translate a crossterm key event into a player `Event`. Returns `None`
/// for keys we don't recognize, so the event loop can ignore them.
pub fn key_to_event(key: crossterm::event::KeyEvent) -> Option<Event> {
    use crossterm::event::{KeyCode, KeyModifiers};
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => Some(Event::Quit),
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(Event::Quit),
        (KeyCode::Char('n'), _) | (KeyCode::Char(']'), _) => Some(Event::Next),
        (KeyCode::Char('p'), _) | (KeyCode::Char('['), _) => Some(Event::Prev),
        (KeyCode::Char('j'), _) | (KeyCode::Down, _) => Some(Event::ScrollDown),
        (KeyCode::Char('k'), _) | (KeyCode::Up, _) => Some(Event::ScrollUp),
        (KeyCode::Tab, _) => Some(Event::TogglePane),
        (KeyCode::Left, _) => Some(Event::Focus(Pane::Code)),
        (KeyCode::Right, _) => Some(Event::Focus(Pane::Commentary)),
        (KeyCode::Char(c), _) if ('1'..='9').contains(&c) => {
            Some(Event::Goto((c as u8 - b'1') as usize))
        }
        _ => None,
    }
}

pub fn mouse_to_event(
    mouse: crossterm::event::MouseEvent,
    area: ratatui::layout::Rect,
) -> Option<Event> {
    use crossterm::event::MouseEventKind;

    let pane = pane_at(area, mouse.column, mouse.row)?;
    match mouse.kind {
        MouseEventKind::ScrollDown => Some(Event::ScrollPaneDown(pane)),
        MouseEventKind::ScrollUp => Some(Event::ScrollPaneUp(pane)),
        _ => None,
    }
}

fn pane_at(area: ratatui::layout::Rect, x: u16, y: u16) -> Option<Pane> {
    let layout = player_layout(area);
    if contains(layout.code, x, y) {
        Some(Pane::Code)
    } else if contains(layout.body, x, y) {
        Some(Pane::Commentary)
    } else {
        None
    }
}

fn contains(rect: ratatui::layout::Rect, x: u16, y: u16) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

struct PlayerLayout {
    code: ratatui::layout::Rect,
    body: ratatui::layout::Rect,
    status: ratatui::layout::Rect,
}

fn player_layout(area: ratatui::layout::Rect) -> PlayerLayout {
    use ratatui::layout::{Constraint, Layout};

    let outer = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    let panes = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(outer[0]);

    PlayerLayout {
        code: panes[0],
        body: panes[1],
        status: outer[1],
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
pub fn render(state: &Player, frame: &mut ratatui::Frame) {
    use ratatui::layout::{Constraint, Layout};
    use ratatui::widgets::{Block, Paragraph, Wrap};

    let area = frame.area();
    let layout = player_layout(area);
    let code_pane = layout.code;
    let body_pane = layout.body;
    let status_area = layout.status;

    let stop = state.cached_stop(state.index);

    let mut file_label = stop
        .as_ref()
        .map(|s| s.file.display().to_string())
        .unwrap_or_default();
    if state.active_pane == Pane::Code {
        file_label = format!("> {}", file_label);
    }
    let code_lines = stop
        .as_ref()
        .map(|s| visible_code_lines(s, state.scroll, code_pane.height))
        .unwrap_or_default();
    let code = Paragraph::new(ratatui::text::Text::from(code_lines))
        .block(Block::bordered().title(file_label));
    frame.render_widget(code, code_pane);

    let body_title = stop
        .as_ref()
        .map(|s| s.title.as_str())
        .unwrap_or("(no stops)")
        .to_string();
    let body_title = if state.active_pane == Pane::Commentary {
        format!("> {}", body_title)
    } else {
        body_title
    };
    let body_text = stop.as_ref().map(|s| s.body.as_str()).unwrap_or("");
    let body = Paragraph::new(body_text)
        .block(Block::bordered().title(body_title))
        .wrap(Wrap { trim: false })
        .scroll((state.body_scroll as u16, 0));
    frame.render_widget(body, body_pane);

    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};

    let (nav, progress) = if state.stops.is_empty() {
        ("no stops — q quit".to_string(), String::new())
    } else {
        (
            "n/] next  p/[ prev  tab pane  j/k scroll  wheel scrolls pane  q quit".to_string(),
            format!("{} / {}", state.index + 1, state.stops.len()),
        )
    };

    let warning = stop.as_ref().and_then(|s| s.warnings.first()).cloned();

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

    fn test_source(file_text: String, color: bool) -> Arc<LoadedSource> {
        Arc::new(LoadedSource {
            line_ranges: line_ranges(&file_text),
            rendered_lines: Mutex::new(HashMap::new()),
            checkpoints: Mutex::new(BTreeMap::new()),
            color,
            file_text,
        })
    }

    fn fake_stop(title: &str) -> LoadedStop {
        let file = PathBuf::from("foo.rs");
        LoadedStop {
            title: title.into(),
            body: String::new(),
            file,
            source: test_source(String::new(), true),
            anchor_line: Some(1),
            highlight_lines: None,
            warnings: Vec::new(),
        }
    }

    fn player_from_stops(stops: Vec<LoadedStop>) -> Player {
        Player {
            stops: stops.into_iter().map(Some).collect(),
            index: 0,
            scroll: 0,
            body_scroll: 0,
            active_pane: Pane::Code,
            preload: None,
        }
    }

    fn player_from_stops_at(stops: Vec<LoadedStop>, index: usize, scroll: usize) -> Player {
        Player {
            stops: stops.into_iter().map(Some).collect(),
            index,
            scroll,
            body_scroll: 0,
            active_pane: Pane::Code,
            preload: None,
        }
    }

    fn stop_at(player: &Player, idx: usize) -> &LoadedStop {
        player.stops[idx].as_ref().unwrap()
    }

    fn player_with(n: usize) -> Player {
        player_from_stops((0..n).map(|i| fake_stop(&format!("stop {}", i))).collect())
    }

    fn empty_player() -> Player {
        Player {
            stops: vec![],
            index: 0,
            scroll: 0,
            body_scroll: 0,
            active_pane: Pane::Code,
            preload: None,
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
    fn keyboard_scrolls_active_commentary_pane() {
        let mut p = player_with(1);
        p.handle(Event::Focus(Pane::Commentary));
        p.handle(Event::ScrollDown);
        assert_eq!(p.scroll, 0);
        assert_eq!(p.body_scroll, 1);

        p.handle(Event::ScrollUp);
        assert_eq!(p.body_scroll, 0);
    }

    #[test]
    fn toggle_pane_switches_keyboard_scroll_target() {
        let mut p = player_with(1);
        assert_eq!(p.active_pane, Pane::Code);
        p.handle(Event::TogglePane);
        assert_eq!(p.active_pane, Pane::Commentary);
        p.handle(Event::ScrollDown);
        assert_eq!(p.body_scroll, 1);
        assert_eq!(p.scroll, 0);
    }

    #[test]
    fn mouse_scrolls_specific_pane_and_focuses_it() {
        let mut p = player_with(1);
        p.handle(Event::ScrollPaneDown(Pane::Commentary));
        assert_eq!(p.body_scroll, 1);
        assert_eq!(p.scroll, 0);
        assert_eq!(p.active_pane, Pane::Commentary);

        p.handle(Event::ScrollPaneDown(Pane::Code));
        assert_eq!(p.scroll, 1);
        assert_eq!(p.body_scroll, 1);
        assert_eq!(p.active_pane, Pane::Code);
    }

    #[test]
    fn quit_returns_exit_outcome() {
        let mut p = player_with(1);
        assert_eq!(p.handle(Event::Quit), Outcome::Exit);
    }

    // ----- auto-positioning tests -----

    #[test]
    fn auto_scroll_zero_when_anchor_at_top() {
        let p = player_from_stops(vec![fake_stop("a")]); // anchor_line: Some(1)
        assert_eq!(p.auto_scroll(0), 0);
    }

    #[test]
    fn auto_scroll_leaves_context_above_anchor() {
        let mut s = fake_stop("a");
        s.anchor_line = Some(50);
        let p = player_from_stops(vec![s]);
        // anchor at line 50 with CONTEXT_ABOVE=5 means the pane skips the
        // first (50 - 5 - 1) = 44 lines, putting line 50 at row 5 of the viewport.
        assert_eq!(p.auto_scroll(0), 50 - CONTEXT_ABOVE - 1);
    }

    #[test]
    fn auto_scroll_zero_when_anchor_unresolved() {
        let mut s = fake_stop("a");
        s.anchor_line = None;
        let p = player_from_stops(vec![s]);
        assert_eq!(p.auto_scroll(0), 0);
    }

    #[test]
    fn next_auto_positions_scroll_for_new_stop() {
        let mut a = fake_stop("a");
        a.anchor_line = Some(10);
        let mut b = fake_stop("b");
        b.anchor_line = Some(80);
        let mut p = player_from_stops(vec![a, b]);
        p.handle(Event::Next);
        assert_eq!(p.scroll, 80 - CONTEXT_ABOVE - 1);
    }

    #[test]
    fn prev_auto_positions_scroll_for_new_stop() {
        let mut a = fake_stop("a");
        a.anchor_line = Some(10);
        let mut b = fake_stop("b");
        b.anchor_line = Some(80);
        let mut p = player_from_stops_at(vec![a, b], 1, 80);
        p.handle(Event::Prev);
        assert_eq!(p.scroll, 10 - CONTEXT_ABOVE - 1);
    }

    #[test]
    fn goto_auto_positions_scroll_for_target_stop() {
        let mut stops: Vec<LoadedStop> = (0..5).map(|i| fake_stop(&format!("{}", i))).collect();
        stops[3].anchor_line = Some(100);
        let mut p = player_from_stops(stops);
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
        assert_eq!(
            p.handle(Event::ScrollPaneDown(Pane::Commentary)),
            Outcome::Continue
        );
        assert_eq!(
            p.handle(Event::ScrollPaneUp(Pane::Commentary)),
            Outcome::Continue
        );
        assert_eq!(p.handle(Event::Focus(Pane::Code)), Outcome::Continue);
        assert_eq!(p.handle(Event::TogglePane), Outcome::Continue);
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
            super::load(&self.turin_dir, true)
        }

        fn load_no_color(&self) -> Player {
            super::load(&self.turin_dir, false)
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
        assert_eq!(stop_at(&p, 0).title, "Entry");
        assert!(
            stop_at(&p, 0).body.contains("body of the stop"),
            "body was: {:?}",
            stop_at(&p, 0).body
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

        assert_eq!(f.load().stops[0].as_ref().unwrap().anchor_line, Some(3));
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

        assert_eq!(f.load().stops[0].as_ref().unwrap().anchor_line, Some(3));
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

        assert_eq!(f.load().stops[0].as_ref().unwrap().anchor_line, None);
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
                .as_ref()
                .unwrap()
                .source
                .file_text
                .contains("specific marker text 12345")
        );
    }

    #[test]
    fn load_no_color_uses_plain_code_lines() {
        let f = LoadFixture::new();
        f.write_source("src/lib.rs", "fn main() {}\n");
        f.write_stop(
            "s.md",
            r#"file = "src/lib.rs"
anchor = { kind = "line", value = 1 }"#,
            "",
        );
        f.write_tour(&["s.md"]);

        let p = f.load_no_color();
        let stop = stop_at(&p, 0);
        let line = cached_code_lines(&stop.source, &stop.file, 0, 1)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[1].content.as_ref(), "fn main() {}");
        assert_eq!(line.spans[1].style, ratatui::style::Style::default());
    }

    #[test]
    fn line_ranges_handle_lf_and_crlf_lines() {
        let lf = LoadedSource {
            file_text: "a\nbb\nccc".into(),
            line_ranges: line_ranges("a\nbb\nccc"),
            rendered_lines: Mutex::new(HashMap::new()),
            checkpoints: Mutex::new(BTreeMap::new()),
            color: false,
        };
        assert_eq!(source_line(&lf, 0), Some("a"));
        assert_eq!(source_line(&lf, 1), Some("bb"));
        assert_eq!(source_line(&lf, 2), Some("ccc"));

        let crlf = LoadedSource {
            file_text: "a\r\nbb\r\nccc".into(),
            line_ranges: line_ranges("a\r\nbb\r\nccc"),
            rendered_lines: Mutex::new(HashMap::new()),
            checkpoints: Mutex::new(BTreeMap::new()),
            color: false,
        };
        assert_eq!(source_line(&crlf, 0), Some("a"));
        assert_eq!(source_line(&crlf, 1), Some("bb"));
        assert_eq!(source_line(&crlf, 2), Some("ccc"));
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

        assert_eq!(f.load().stops[0].as_ref().unwrap().highlight_lines, Some(8));
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
            stop_at(&p, 0).title.contains("untitled"),
            "expected title to fall back to filename, got {:?}",
            stop_at(&p, 0).title
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
        let file = PathBuf::from("src/foo.rs");
        LoadedStop {
            title: title.into(),
            body: String::new(),
            file,
            source: test_source(String::new(), true),
            anchor_line: Some(1),
            highlight_lines: None,
            warnings: Vec::new(),
        }
    }

    fn set_file_text(stop: &mut LoadedStop, text: impl Into<String>) {
        stop.source = test_source(text.into(), true);
    }

    #[test]
    fn render_shows_stop_title_in_right_pane() {
        let p = player_from_stops(vec![make_loaded_stop("Entry point")]);
        let buf = render_to_buffer(&p, 80, 20);
        let s = buf_to_string(&buf);
        assert!(s.contains("Entry point"), "buffer:\n{}", s);
    }

    #[test]
    fn render_shows_file_path_in_left_pane_header() {
        let mut stop = make_loaded_stop("X");
        stop.file = PathBuf::from("src/parser/lexer.rs");
        let p = player_from_stops(vec![stop]);
        let buf = render_to_buffer(&p, 80, 20);
        let s = buf_to_string(&buf);
        assert!(s.contains("src/parser/lexer.rs"), "buffer:\n{}", s);
    }

    #[test]
    fn render_shows_progress_in_status_bar() {
        let stops = (0..3)
            .map(|i| make_loaded_stop(&format!("Stop {}", i)))
            .collect();
        let p = player_from_stops_at(stops, 1, 0);
        let buf = render_to_buffer(&p, 80, 20);
        let s = buf_to_string(&buf);
        assert!(s.contains("2 / 3"), "buffer:\n{}", s);
    }

    #[test]
    fn render_status_bar_includes_key_hints() {
        let p = player_from_stops(vec![make_loaded_stop("X")]);
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
        let p = player_from_stops_at(stops, 1, 0);
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
        let p = player_from_stops(vec![stop]);
        let buf = render_to_buffer(&p, 80, 20);
        let s = buf_to_string(&buf);
        assert!(s.contains("specific body marker"), "buffer:\n{}", s);
    }

    #[test]
    fn render_scrolls_commentary_independently() {
        let mut stop = make_loaded_stop("X");
        stop.body = (1..=30)
            .map(|i| format!("commentary line {}\n", i))
            .collect();
        let mut p = player_from_stops(vec![stop]);
        p.body_scroll = 5;
        let buf = render_to_buffer(&p, 100, 20);
        let s = buf_to_string(&buf);
        assert!(s.contains("commentary line 6"), "buffer:\n{}", s);
        assert!(
            !s.contains("commentary line 1                     │"),
            "buffer:\n{}",
            s
        );
    }

    #[test]
    fn render_displays_source_code_text() {
        let mut stop = make_loaded_stop("X");
        set_file_text(&mut stop, "fn unique_marker_fn() {}\n");
        let p = player_from_stops(vec![stop]);
        let buf = render_to_buffer(&p, 80, 20);
        let s = buf_to_string(&buf);
        assert!(s.contains("unique_marker_fn"), "buffer:\n{}", s);
    }

    #[test]
    fn render_uses_cached_visible_code_line() {
        let mut stop = make_loaded_stop("X");
        stop.source = test_source("this text should not be rendered\n".into(), true);
        stop.source
            .rendered_lines
            .lock()
            .unwrap()
            .insert(0, ratatui::text::Line::from("cached marker line"));
        let p = player_from_stops(vec![stop]);
        let buf = render_to_buffer(&p, 80, 20);
        let s = buf_to_string(&buf);
        assert!(s.contains("cached marker line"), "buffer:\n{}", s);
        assert!(
            !s.contains("this text should not be rendered"),
            "buffer:\n{}",
            s
        );
    }

    #[test]
    fn render_scrolls_lazily_rendered_code_lines() {
        let mut stop = make_loaded_stop("X");
        set_file_text(
            &mut stop,
            (1..=30)
                .map(|i| format!("line {}\n", i))
                .collect::<String>(),
        );
        let p = player_from_stops_at(vec![stop], 0, 5);
        let buf = render_to_buffer(&p, 80, 20);
        let s = buf_to_string(&buf);
        assert!(s.contains("6 │ line 6"), "buffer:\n{}", s);
        assert!(!s.contains("│   1 │ line 1"), "buffer:\n{}", s);
    }

    #[test]
    fn render_caches_only_visible_code_lines() {
        let mut stop = make_loaded_stop("X");
        set_file_text(
            &mut stop,
            (1..=100)
                .map(|i| format!("line {}\n", i))
                .collect::<String>(),
        );
        let source = Arc::clone(&stop.source);
        let p = player_from_stops(vec![stop]);

        assert!(source.rendered_lines.lock().unwrap().is_empty());
        let _ = render_to_buffer(&p, 80, 10);

        let cached = source.rendered_lines.lock().unwrap();
        assert!(cached.contains_key(&0));
        assert!(cached.contains_key(&6));
        assert!(!cached.contains_key(&7));
        assert_eq!(cached.len(), 7);
    }

    #[test]
    fn render_stores_checkpoint_after_visible_range() {
        let mut stop = make_loaded_stop("X");
        set_file_text(
            &mut stop,
            (1..=100)
                .map(|i| format!("line {}\n", i))
                .collect::<String>(),
        );
        let source = Arc::clone(&stop.source);
        let p = player_from_stops(vec![stop]);

        let _ = render_to_buffer(&p, 80, 10);

        let checkpoints = source.checkpoints.lock().unwrap();
        assert!(checkpoints.contains_key(&7));
    }

    #[test]
    fn checkpointed_highlighting_carries_multiline_context() {
        let mut stop = make_loaded_stop("X");
        set_file_text(
            &mut stop,
            "fn main() {\n/* comment\nstill comment\n*/\nlet x = 1;\n}\n",
        );
        let source = Arc::clone(&stop.source);
        let p = player_from_stops(vec![stop]);

        let _ = render_to_buffer(&p, 100, 10);

        let cached = source.rendered_lines.lock().unwrap();
        let opening_comment = cached.get(&1).unwrap();
        let continued_comment = cached.get(&2).unwrap();
        let opening_style = opening_comment.spans.last().unwrap().style;
        let continued_style = continued_comment.spans.last().unwrap().style;
        assert_eq!(continued_style, opening_style);
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
    fn key_tab_toggles_active_pane() {
        use crossterm::event::KeyCode;
        assert_eq!(key_to_event(key(KeyCode::Tab)), Some(Event::TogglePane));
    }

    #[test]
    fn arrow_left_and_right_focus_panes() {
        use crossterm::event::KeyCode;
        assert_eq!(
            key_to_event(key(KeyCode::Left)),
            Some(Event::Focus(Pane::Code))
        );
        assert_eq!(
            key_to_event(key(KeyCode::Right)),
            Some(Event::Focus(Pane::Commentary))
        );
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

    #[test]
    fn mouse_wheel_targets_pane_under_cursor() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let area = Rect::new(0, 0, 100, 40);
        let code_mouse = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let body_mouse = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 80,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            mouse_to_event(code_mouse, area),
            Some(Event::ScrollPaneDown(Pane::Code))
        );
        assert_eq!(
            mouse_to_event(body_mouse, area),
            Some(Event::ScrollPaneUp(Pane::Commentary))
        );
    }

    #[test]
    fn mouse_wheel_ignores_status_bar() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let event = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 10,
            row: 39,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(mouse_to_event(event, Rect::new(0, 0, 100, 40)), None);
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
        set_file_text(
            &mut stop,
            (1..=30)
                .map(|i| format!("line {}\n", i))
                .collect::<String>(),
        );
        stop.anchor_line = Some(10);
        stop.highlight_lines = None; // default = 1 line
        let p = player_from_stops(vec![stop]);
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
        set_file_text(
            &mut stop,
            (1..=30)
                .map(|i| format!("line {}\n", i))
                .collect::<String>(),
        );
        stop.anchor_line = Some(10);
        stop.highlight_lines = Some(4);
        let p = player_from_stops(vec![stop]);
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
        set_file_text(&mut stop, "a\nb\nc\n");
        stop.anchor_line = None;
        let p = player_from_stops(vec![stop]);
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
        let p = player_from_stops(vec![s]);
        let buf = render_to_buffer(&p, 120, 20);
        let s = buf_to_string(&buf);
        assert!(s.contains("fell back to literal match"), "buffer:\n{}", s);
    }

    #[test]
    fn render_includes_line_numbers_in_code_pane() {
        let mut stop = make_loaded_stop("X");
        set_file_text(&mut stop, "alpha\nbeta\ngamma\n");
        let p = player_from_stops(vec![stop]);
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
            body_scroll: 0,
            active_pane: Pane::Code,
            preload: None,
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
        assert_eq!(stop_at(&p, 0).anchor_line, Some(2));
        assert_eq!(stop_at(&p, 0).warnings.len(), 1);
        assert!(
            stop_at(&p, 0).warnings[0].contains("tree-sitter not bundled"),
            "warning was: {:?}",
            stop_at(&p, 0).warnings
        );
        assert!(stop_at(&p, 0).warnings[0].contains("tokenize"));
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
        assert_eq!(stop_at(&p, 0).anchor_line, None);
        assert_eq!(stop_at(&p, 0).warnings.len(), 1);
        assert!(stop_at(&p, 0).warnings[0].contains("no quoted literal"));
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
        assert_eq!(stop_at(&p, 0).anchor_line, None);
        assert!(
            stop_at(&p, 0)
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

        let mut p = f.load();
        p.ensure_prepared(1);
        p.ensure_prepared(2);
        let titles: Vec<&str> = p
            .stops
            .iter()
            .map(|s| s.as_ref().unwrap().title.as_str())
            .collect();
        assert_eq!(titles, vec!["c.md", "a.md", "b.md"]);
    }
}
