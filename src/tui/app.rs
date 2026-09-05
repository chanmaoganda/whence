//! What the browser is showing, and what each key does to it.
//!
//! No terminal appears anywhere in this file. Every transition is a method on
//! [`App`] that a test can call directly; the only things the drawing code hands
//! back are the width and height it measured, because line breaking cannot
//! happen before the terminal has been asked how big it is.

use crate::index::SearchIndex;
use crate::model::{short_id, Harness, Session};
use crate::search::{Hit, Query};
use crate::source;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;
use ratatui::widgets::ListState;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// How many parsed transcripts to keep. Moving down a result list and back up
/// again is the common motion, and re-reading a 40 MB transcript to redraw a
/// preview you saw two keystrokes ago is the one avoidable cost here.
const CACHE: usize = 4;

/// The kind filter, cycled with Tab. `None` is "every kind".
const KINDS: [&str; 4] = ["prompt", "reply", "think", "edit"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// The query, the results, and a preview of the selected one.
    Search,
    /// One conversation, scrolled.
    Read,
}

pub struct App {
    index: SearchIndex,
    /// Filters that came from the command line and stay put: project, tool,
    /// since, limit, fuzziness. Text, kind and harness are driven from here.
    base: Query,
    /// Harnesses the user allowed on the command line; the interactive filter
    /// can only narrow within these, never widen past them.
    allowed: Vec<Harness>,

    pub query: String,
    /// Byte offset of the caret in `query`. Moved by characters, never bytes —
    /// most of this corpus is Chinese.
    pub cursor: usize,
    pub kind: Option<String>,
    /// `0` is every allowed harness; otherwise `allowed[harness - 1]`.
    pub harness: usize,

    pub hits: Vec<Hit>,
    pub relaxed: bool,
    pub error: Option<String>,
    pub list: ListState,

    pub view: View,
    /// The conversation behind the selected hit, laid out for the side pane.
    pub preview: Option<Reading>,
    /// The conversation being read full-screen.
    pub reading: Option<Reading>,
    pub thinking: bool,
    pub quit: bool,

    cache: Vec<(PathBuf, Rc<Session>)>,
    search_pending: bool,
    preview_pending: bool,
}

impl App {
    pub fn new(index: SearchIndex, query: Query) -> Self {
        let allowed = if query.harness.is_empty() {
            Harness::ALL.to_vec()
        } else {
            query.harness.clone()
        };
        App {
            cursor: query.text.len(),
            query: query.text.clone(),
            kind: query.kind.clone(),
            harness: 0,
            index,
            base: query,
            allowed,
            hits: Vec::new(),
            relaxed: false,
            error: None,
            list: ListState::default(),
            view: View::Search,
            preview: None,
            reading: None,
            thinking: false,
            quit: false,
            cache: Vec::new(),
            // The opening screen is a search too: with no words it is the most
            // recent thing each harness recorded, which is a better answer to
            // "what was I doing" than an empty pane.
            search_pending: true,
            preview_pending: false,
        }
    }

    /// Do the work the last batch of keys asked for. Called once per frame,
    /// before drawing, so a burst of keys costs one search and one file read.
    pub fn settle(&mut self) {
        if std::mem::take(&mut self.search_pending) {
            self.run_search();
        }
        if std::mem::take(&mut self.preview_pending) {
            self.load_preview();
        }
    }

    pub fn selected(&self) -> Option<&Hit> {
        self.list.selected().and_then(|i| self.hits.get(i))
    }

    /// The `whence show` target for whatever is on screen.
    pub fn exit_hint(&self) -> Option<String> {
        if let Some(reading) = &self.reading {
            return Some(format!(
                "{}#{}",
                short_id(&reading.session.id),
                reading.turn
            ));
        }
        let hit = self.selected()?;
        Some(format!("{}#{}", short_id(&hit.session), hit.turn))
    }

    /// Which harnesses the current filter admits, in the form a [`Query`] wants.
    pub fn harness_filter(&self) -> Vec<Harness> {
        match self.harness {
            0 => self.base.harness.clone(),
            i => vec![self.allowed[i - 1]],
        }
    }

    /// How the harness filter reads in the status line.
    pub fn harness_label(&self) -> String {
        match self.harness {
            0 if self.allowed.len() == Harness::ALL.len() => "all".to_string(),
            0 => self
                .allowed
                .iter()
                .map(|h| h.as_str())
                .collect::<Vec<_>>()
                .join("+"),
            i => self.allowed[i - 1].to_string(),
        }
    }

    pub fn handle(&mut self, event: Event) {
        if let Event::Resize(..) = event {
            // Laid-out text is width-specific; both panes notice on the next
            // draw and re-break their lines.
            return;
        }
        let Some(key) = super::key_press(&event) else {
            return;
        };
        if self.quit_key(key) {
            self.quit = true;
            return;
        }
        match self.view {
            View::Search => self.search_key(key),
            View::Read => self.read_key(key),
        }
    }

    fn quit_key(&self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c' | 'd') if ctrl => true,
            // With nothing typed there is nothing left to back out of.
            KeyCode::Esc => self.view == View::Search && self.query.is_empty(),
            _ => false,
        }
    }

    // ---- the search view -------------------------------------------------

    fn search_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('u') if ctrl => self.edit(|q, c| {
                q.drain(..*c);
                *c = 0;
            }),
            KeyCode::Char('w') if ctrl => self.edit(|q, c| {
                let head = &q[..*c];
                let cut = head.trim_end();
                let start = cut
                    .char_indices()
                    .rev()
                    .find(|(_, ch)| ch.is_whitespace())
                    .map_or(0, |(i, ch)| i + ch.len_utf8());
                q.drain(start..*c);
                *c = start;
            }),
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.query.len(),
            KeyCode::Char('n') if ctrl => self.move_selection(1),
            KeyCode::Char('p') if ctrl => self.move_selection(-1),
            KeyCode::Char(ch) if !ctrl => self.edit(|q, c| {
                q.insert(*c, ch);
                *c += ch.len_utf8();
            }),
            KeyCode::Backspace => self.edit(|q, c| {
                if let Some((at, ch)) = q[..*c].char_indices().next_back() {
                    q.remove(at);
                    *c -= ch.len_utf8();
                }
            }),
            KeyCode::Delete => self.edit(|q, c| {
                if q[*c..].chars().next().is_some() {
                    q.remove(*c);
                }
            }),
            KeyCode::Left => self.cursor = prev_char(&self.query, self.cursor),
            KeyCode::Right => self.cursor = next_char(&self.query, self.cursor),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.query.len(),
            KeyCode::Esc => self.edit(|q, c| {
                q.clear();
                *c = 0;
            }),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Tab => self.cycle_kind(),
            KeyCode::BackTab => self.cycle_harness(),
            KeyCode::Enter => self.open(),
            _ => {}
        }
    }

    /// Change the query text, and remember that the results no longer match it.
    fn edit(&mut self, change: impl FnOnce(&mut String, &mut usize)) {
        let before = self.query.clone();
        change(&mut self.query, &mut self.cursor);
        if self.query != before {
            self.search_pending = true;
        }
    }

    fn move_selection(&mut self, by: isize) {
        if self.hits.is_empty() {
            return;
        }
        let last = self.hits.len() - 1;
        let at = self.list.selected().unwrap_or(0) as isize;
        let next = at.saturating_add(by).clamp(0, last as isize) as usize;
        if Some(next) != self.list.selected() {
            self.list.select(Some(next));
            self.preview_pending = true;
        }
    }

    fn cycle_kind(&mut self) {
        let at = self
            .kind
            .as_deref()
            .and_then(|k| KINDS.iter().position(|c| *c == k))
            .map_or(0, |i| i + 1);
        self.kind = KINDS.get(at % (KINDS.len() + 1)).map(|k| k.to_string());
        self.search_pending = true;
    }

    fn cycle_harness(&mut self) {
        self.harness = (self.harness + 1) % (self.allowed.len() + 1);
        self.search_pending = true;
    }

    fn open(&mut self) {
        let Some(hit) = self.selected().cloned() else {
            return;
        };
        match self.session_of(&hit) {
            Ok(session) => {
                self.reading = Some(Reading::new(session, hit.turn as usize));
                self.view = View::Read;
                self.error = None;
            }
            Err(err) => self.error = Some(err),
        }
    }

    // ---- the reading view ------------------------------------------------

    fn read_key(&mut self, key: KeyEvent) {
        let Some(reading) = &mut self.reading else {
            self.view = View::Search;
            return;
        };
        let page = reading.height.saturating_sub(2).max(1) as isize;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.reading = None;
                self.view = View::Search;
            }
            KeyCode::Down | KeyCode::Char('j') => reading.scroll_by(1),
            KeyCode::Up | KeyCode::Char('k') => reading.scroll_by(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => reading.scroll_by(page),
            KeyCode::PageUp => reading.scroll_by(-page),
            KeyCode::Home | KeyCode::Char('g') => reading.scroll_by(isize::MIN),
            KeyCode::End | KeyCode::Char('G') => reading.scroll_by(isize::MAX),
            KeyCode::Char(']') => reading.step_turn(1),
            KeyCode::Char('[') => reading.step_turn(-1),
            KeyCode::Char('t') => {
                self.thinking = !self.thinking;
            }
            _ => {}
        }
    }

    // ---- the work settle() does ------------------------------------------

    fn run_search(&mut self) {
        let query = Query {
            text: self.query.clone(),
            kind: self.kind.clone(),
            harness: self.harness_filter(),
            ..self.base.clone()
        };
        match self.index.search(&query) {
            Ok(results) => {
                self.hits = results.hits;
                self.relaxed = results.relaxed;
                self.error = None;
            }
            Err(err) => {
                // A half-typed query — one quote, a trailing `+` — is a parse
                // error, not a crash. Say so and keep taking keys.
                self.hits.clear();
                self.relaxed = false;
                self.error = Some(first_sentence(&err.to_string()));
            }
        }
        self.list.select((!self.hits.is_empty()).then_some(0));
        self.preview = None;
        self.preview_pending = true;
    }

    fn load_preview(&mut self) {
        let Some(hit) = self.selected().cloned() else {
            self.preview = None;
            return;
        };
        match self.session_of(&hit) {
            Ok(session) => self.preview = Some(Reading::new(session, hit.turn as usize)),
            Err(err) => {
                self.preview = None;
                self.error = Some(err);
            }
        }
    }

    /// The session a hit came from, read straight off disk.
    ///
    /// The index stored the transcript's path, so this needs no directory scan
    /// and no second lookup — and, as everywhere else, the conversation you read
    /// back is the file's, not the index's summary of it.
    fn session_of(&mut self, hit: &Hit) -> Result<Rc<Session>, String> {
        let harness: Harness = hit.harness.parse()?;
        let path = Path::new(&hit.source);
        if let Some(at) = self.cache.iter().position(|(p, _)| p == path) {
            let entry = self.cache.remove(at);
            self.cache.insert(0, entry);
            return Ok(self.cache[0].1.clone());
        }
        let (session, _) = source::normalize_with(harness, path)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        let session = Rc::new(session);
        self.cache.insert(0, (path.to_path_buf(), session.clone()));
        self.cache.truncate(CACHE);
        Ok(session)
    }
}

/// One conversation on screen: the session, where in it we are, and the lines
/// that were last broken to fit.
///
/// Both the preview pane and the full-screen reader are this — the same session
/// laid out at two different widths — so there is one code path for turning a
/// transcript into text you can look at.
pub struct Reading {
    pub session: Rc<Session>,
    /// The turn we came in at, as a [`crate::model::Turn::index`].
    pub turn: usize,
    pub scroll: usize,
    /// Set during a draw; key handling clamps against them.
    pub height: u16,
    pub total: usize,
    layout: Option<Laid>,
    /// A turn to jump to once we know how wide the lines are.
    goto: Option<usize>,
}

/// A conversation broken to a particular width.
pub struct Laid {
    pub width: u16,
    pub thinking: bool,
    pub lines: Vec<Line<'static>>,
    /// The line each turn starts on, parallel to `session.turns`.
    pub starts: Vec<usize>,
}

impl Reading {
    pub fn new(session: Rc<Session>, turn: usize) -> Self {
        Reading {
            session,
            turn,
            scroll: 0,
            height: 0,
            total: 0,
            layout: None,
            goto: Some(turn),
        }
    }

    /// Break the conversation to `width` if that has not already been done, and
    /// resolve any pending jump. Called from the draw, which is the first moment
    /// a width exists.
    pub fn lay(&mut self, width: u16, height: u16, thinking: bool) {
        let stale = match &self.layout {
            Some(laid) => laid.width != width || laid.thinking != thinking,
            None => true,
        };
        if stale {
            self.layout = Some(super::ui::conversation(&self.session, thinking, width));
        }
        let laid = self.layout.as_ref().expect("just laid out");
        let jump = self
            .goto
            .take()
            .and_then(|turn| position_of(&self.session, turn))
            .and_then(|at| laid.starts.get(at).copied());

        self.total = laid.lines.len();
        self.height = height;
        if let Some(line) = jump {
            self.scroll = line;
        }
        self.scroll = self.scroll.min(self.max_scroll());

        // Which turn we are on is a fact about where the scroll ended up, not a
        // memory of how we got here — otherwise the header keeps naming the turn
        // you arrived at long after you have scrolled somewhere else.
        let at = self
            .layout
            .as_ref()
            .expect("just laid out")
            .starts
            .iter()
            .rposition(|&start| start <= self.scroll);
        if let Some(turn) = at.and_then(|at| self.session.turns.get(at)) {
            self.turn = turn.index;
        }
    }

    pub fn laid(&self) -> Option<&Laid> {
        self.layout.as_ref()
    }

    fn max_scroll(&self) -> usize {
        self.total.saturating_sub(self.height as usize)
    }

    fn scroll_by(&mut self, by: isize) {
        let at = self.scroll as isize;
        self.scroll = at.saturating_add(by).clamp(0, self.max_scroll() as isize) as usize;
    }

    /// Jump to the previous or next turn, and remember which one we are on so
    /// the header can say.
    fn step_turn(&mut self, by: isize) {
        let Some(at) = position_of(&self.session, self.turn) else {
            return;
        };
        let last = self.session.turns.len().saturating_sub(1);
        let next = (at as isize).saturating_add(by).clamp(0, last as isize) as usize;
        let Some(turn) = self.session.turns.get(next) else {
            return;
        };
        self.turn = turn.index;
        self.goto = Some(turn.index);
    }
}

/// Where a turn sits in `session.turns`. Turns carry their own index — one that
/// `whence show` accepts and that a hit refers to — which is not a position in
/// the list.
pub fn position_of(session: &Session, index: usize) -> Option<usize> {
    session.turns.iter().position(|t| t.index == index)
}

fn prev_char(text: &str, at: usize) -> usize {
    text[..at].char_indices().next_back().map_or(0, |(i, _)| i)
}

fn next_char(text: &str, at: usize) -> usize {
    text[at..]
        .chars()
        .next()
        .map_or(at, |ch| at + ch.len_utf8())
}

/// The first line of an error, for a one-line status bar.
fn first_sentence(text: &str) -> String {
    crate::model::first_line(text, 120)
}
