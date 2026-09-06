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

    /// Every hit the search returned, best first. The list on screen is
    /// [`Tree`] over these — the hits themselves stay flat, because a hit's
    /// rank is a fact about the whole corpus and grouping must not reorder it.
    pub hits: Vec<Hit>,
    /// The hits gathered under the session each came from. One session is one
    /// row until you open it: forty near-identical matches inside a single
    /// conversation used to be forty rows, and the next conversation was off
    /// the bottom of the screen.
    pub tree: Tree,
    pub relaxed: bool,
    /// The query as the analyzer cut it. What you typed is in `query`; this is
    /// what is actually being looked for, and they are not the same thing once
    /// a path or a Chinese phrase is involved.
    pub terms: Vec<String>,
    pub error: Option<String>,
    pub list: ListState,

    pub view: View,
    /// The conversation behind the selected hit, laid out for the side pane.
    pub preview: Option<Reading>,
    /// The conversation being read full-screen.
    pub reading: Option<Reading>,
    pub thinking: bool,
    /// Every tool call spelled out, rather than a run of them folded to a line.
    pub tools: bool,
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
            tree: Tree::default(),
            relaxed: false,
            terms: Vec::new(),
            error: None,
            list: ListState::default(),
            view: View::Search,
            preview: None,
            reading: None,
            thinking: false,
            tools: false,
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

    /// What the reader is showing beyond the conversation itself. Carried as
    /// one value because it is also what a laid-out page is keyed on: change
    /// either flag and the lines have to be built again.
    pub fn show(&self) -> Show {
        Show {
            thinking: self.thinking,
            tools: self.tools,
        }
    }

    /// The row the cursor is on.
    pub fn row(&self) -> Option<Row> {
        self.list
            .selected()
            .and_then(|i| self.tree.rows().get(i))
            .copied()
    }

    /// The hit the cursor is on. A session row stands for its best hit, so a
    /// folded conversation still has a preview and still answers `whence show`.
    pub fn selected(&self) -> Option<&Hit> {
        let at = self.tree.hit_at(self.row()?)?;
        self.hits.get(at)
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
            // The caret's own left and right. The arrows are the tree's, so the
            // box keeps the readline keys it already answers to.
            KeyCode::Char('b') if ctrl => self.cursor = prev_char(&self.query, self.cursor),
            KeyCode::Char('f') if ctrl => self.cursor = next_char(&self.query, self.cursor),
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
            // All four arrows drive the tree, always. Sharing them with the
            // caret meant `→` opened a session and `←` only moved the caret
            // back through what you had just typed — the same key doing two
            // things depending on a caret you were not looking at.
            KeyCode::Left => self.fold(),
            KeyCode::Right => self.unfold(),
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
        let rows = self.tree.rows().len();
        if rows == 0 {
            return;
        }
        let at = self.list.selected().unwrap_or(0) as isize;
        let next = at.saturating_add(by).clamp(0, rows as isize - 1) as usize;
        self.select_row(next);
    }

    /// Move to a row, and load its conversation if that is a different one.
    fn select_row(&mut self, at: usize) {
        let was = self.selected().map(|hit| hit.source.clone());
        if Some(at) == self.list.selected() {
            return;
        }
        self.list.select(Some(at));
        let now = self.selected().map(|hit| hit.source.clone());
        // Walking the matches inside one conversation is the common motion, and
        // the transcript behind them does not change as you do it.
        if was != now || self.preview.is_none() {
            self.preview_pending = true;
        }
    }

    /// Open the session under the cursor, or step into it if it is already
    /// open. The matches inside a conversation are the branch; the conversation
    /// is the thing you are choosing between.
    fn unfold(&mut self) {
        let Some(Row::Session(g)) = self.row() else {
            return;
        };
        if self.tree.groups[g].open {
            self.move_selection(1);
            return;
        }
        self.tree.set_open(g, true);
        self.reselect(Row::Session(g));
    }

    /// Shut the session under the cursor. From inside one, come back out to it
    /// first — the way every tree behaves, and the way back to the *other*
    /// sessions once a long one has filled the screen.
    fn fold(&mut self) {
        match self.row() {
            Some(Row::Hit(g, _)) => self.reselect(Row::Session(g)),
            Some(Row::Session(g)) => {
                self.tree.set_open(g, false);
                self.reselect(Row::Session(g));
            }
            None => {}
        }
    }

    /// Put the cursor back on a row after the visible rows have been rebuilt.
    fn reselect(&mut self, row: Row) {
        if let Some(at) = self.tree.position_of(row) {
            self.select_row(at);
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
                self.reading = Some(Reading::new(session, hit.turn as usize, found(&hit)));
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
            KeyCode::Char('o') => {
                self.tools = !self.tools;
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
                self.terms = results.terms;
                self.error = None;
            }
            Err(err) => {
                // A half-typed query — one quote, a trailing `+` — is a parse
                // error, not a crash. Say so and keep taking keys.
                self.hits.clear();
                self.relaxed = false;
                self.terms.clear();
                self.error = Some(first_sentence(&err.to_string()));
            }
        }
        self.tree = Tree::of(&self.hits);
        self.list
            .select((!self.tree.rows().is_empty()).then_some(0));
        self.preview = None;
        self.preview_pending = true;
    }

    fn load_preview(&mut self) {
        let Some(hit) = self.selected().cloned() else {
            self.preview = None;
            return;
        };
        match self.session_of(&hit) {
            Ok(session) => {
                self.preview = Some(Reading::new(session, hit.turn as usize, found(&hit)))
            }
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

/// The results, gathered under the conversation each came from.
///
/// A flat list of hits is the wrong shape for this corpus. One session asks the
/// same question thirty times in slightly different words, so a query that
/// matches it at all matches it thirty times — and thirty near-identical rows
/// push every *other* conversation off the screen, which is the one thing the
/// list was for. Grouping puts one row per session on screen and keeps the
/// matches inside it one keystroke away.
///
/// The grouping never reorders: sessions appear in the order their best hit
/// did, and the hits inside a session in the order the searcher ranked them.
/// Rank is a fact about the whole corpus and this is a view over it.
#[derive(Default)]
pub struct Tree {
    pub groups: Vec<Group>,
    /// The rows actually on screen, rebuilt whenever a group opens or shuts.
    rows: Vec<Row>,
}

/// Every hit from one session.
pub struct Group {
    pub session: String,
    /// Indices into [`App::hits`], in the order the conversation happened.
    /// Rank orders the sessions; inside one, a conversation reads forwards.
    pub hits: Vec<usize>,
    /// Which of `hits` the searcher liked best — the passage a shut session
    /// shows, and the turn `⏎` opens it at. Not `hits[0]`: the best answer is
    /// as often at the end of a conversation as at the start.
    pub best: usize,
    pub open: bool,
}

/// One line of the tree — or rather one *entry*, since both kinds draw as
/// several lines. Carrying positions rather than references is what lets the
/// hits stay in one flat vector that nothing has to clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// A conversation: the group's index.
    Session(usize),
    /// One match inside it: the group, then which of its hits.
    Hit(usize, usize),
}

impl Tree {
    pub fn of(hits: &[Hit]) -> Tree {
        let mut groups: Vec<Group> = Vec::new();
        for (at, hit) in hits.iter().enumerate() {
            match groups.iter_mut().find(|g| g.session == hit.session) {
                Some(group) => group.hits.push(at),
                None => groups.push(Group {
                    session: hit.session.clone(),
                    hits: vec![at],
                    best: 0,
                    open: false,
                }),
            }
        }
        for group in &mut groups {
            let best = group.hits[0];
            // A stable sort, so two matches in one turn keep the order the
            // searcher put them in.
            group.hits.sort_by_key(|&at| hits[at].turn);
            group.best = group.hits.iter().position(|&at| at == best).unwrap_or(0);
        }
        // A tree of one branch is a list: with nothing to choose between, the
        // fold would only be something to open before you could read anything.
        if groups.len() == 1 {
            groups[0].open = true;
        }
        let mut tree = Tree {
            groups,
            rows: Vec::new(),
        };
        tree.relayout();
        tree
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// How many conversations the results came from — the number the flat list
    /// could never say.
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn set_open(&mut self, group: usize, open: bool) {
        if let Some(g) = self.groups.get_mut(group) {
            if g.open == open {
                return;
            }
            g.open = open;
            self.relayout();
        }
    }

    /// The hit a row stands for. A shut session stands for its best one, so a
    /// row that shows no excerpt still has a conversation behind it.
    pub fn hit_at(&self, row: Row) -> Option<usize> {
        let (group, at) = match row {
            Row::Session(g) => {
                let group = self.groups.get(g)?;
                (group, group.best)
            }
            Row::Hit(g, i) => (self.groups.get(g)?, i),
        };
        group.hits.get(at).copied()
    }

    pub fn position_of(&self, row: Row) -> Option<usize> {
        self.rows.iter().position(|&r| r == row)
    }

    fn relayout(&mut self) {
        self.rows.clear();
        for (g, group) in self.groups.iter().enumerate() {
            self.rows.push(Row::Session(g));
            if group.open {
                self.rows
                    .extend((0..group.hits.len()).map(|i| Row::Hit(g, i)));
            }
        }
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
    /// The words the hit matched, picked out wherever they appear in the
    /// conversation. A transcript is long and the reason you opened it is one
    /// sentence somewhere inside it.
    pub words: Vec<String>,
    pub scroll: usize,
    /// Set during a draw; key handling clamps against them.
    pub height: u16,
    pub total: usize,
    layout: Option<Laid>,
    /// A turn to jump to once we know how wide the lines are.
    goto: Option<usize>,
}

/// What the reader is showing beyond the conversation itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Show {
    /// Reasoning text, where the harness left any in the clear.
    pub thinking: bool,
    /// Every tool call on its own line. Folded by default: a turn can be forty
    /// of them between two sentences, and the sentences are what you came for.
    pub tools: bool,
}

/// A conversation broken to a particular width.
pub struct Laid {
    pub width: u16,
    pub show: Show,
    pub lines: Vec<Line<'static>>,
    /// The line each turn starts on, parallel to `session.turns`.
    pub starts: Vec<usize>,
}

impl Reading {
    pub fn new(session: Rc<Session>, turn: usize, words: Vec<String>) -> Self {
        Reading {
            session,
            turn,
            words,
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
    pub fn lay(&mut self, width: u16, height: u16, show: Show) {
        let stale = match &self.layout {
            Some(laid) => laid.width != width || laid.show != show,
            None => true,
        };
        if stale {
            self.layout = Some(super::ui::conversation(
                &self.session,
                show,
                width,
                &self.words,
            ));
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

/// The words a hit matched, in the spelling the transcript used — which is what
/// the reader has to look for, not what was typed.
fn found(hit: &Hit) -> Vec<String> {
    hit.matched.iter().map(|m| m.word.clone()).collect()
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
