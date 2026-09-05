//! The normalized model: what a session *means*, independent of which harness
//! wrote it and how that harness happens to serialize things.
//!
//! Everything downstream — the search index, the CLI, the TUI, the MCP server —
//! reads these types and never touches a `raw` module. That is what lets a new
//! harness be one new directory under [`crate::source`] rather than a change
//! rippling through every surface, and what lets a Claude Code release that
//! changes the on-disk format touch exactly one file.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

/// A coding agent whose transcripts we can read.
///
/// Ordering is the display order in mixed result lists, so it is alphabetical
/// rather than historical — no harness gets to be "the default one".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Harness {
    Claude,
    Codex,
}

impl Harness {
    /// Every harness, in display order. Adding a variant here and a module
    /// under [`crate::source`] is the whole cost of supporting a new agent.
    pub const ALL: [Harness; 2] = [Harness::Claude, Harness::Codex];

    /// The lowercase token used on the command line, in the index and in
    /// `--harness` filters. Stable: it is written into the index.
    pub fn as_str(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
        }
    }
}

impl fmt::Display for Harness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Harness {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Harness::ALL
            .into_iter()
            .find(|h| h.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| {
                let names: Vec<&str> = Harness::ALL.iter().map(|h| h.as_str()).collect();
                format!("unknown harness {s:?} — known: {}", names.join(", "))
            })
    }
}

/// One conversation, reconstructed from a single transcript file.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    /// Which agent wrote this transcript.
    pub harness: Harness,
    /// Working directory the session ran in, e.g. `/code/rust/whence`.
    pub project: String,
    pub source_path: PathBuf,
    pub kind: SessionKind,
    pub branch: Option<String>,
    /// Version of the agent itself (Claude Code's `version`, Codex's
    /// `cli_version`), for telling a format change from a parser bug.
    pub agent_version: Option<String>,
    /// The model that did the work, when the transcript names one.
    pub model: Option<String>,
    /// An auto-generated conversation title, where the harness produced one.
    pub title: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub turns: Vec<Turn>,
    /// Files this session edited, attributed to the work that changed them.
    pub file_touches: Vec<FileTouch>,
    pub usage: UsageTotals,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionKind {
    /// A conversation you drove yourself.
    Main,
    /// A subagent spawned by a main session.
    Subagent { agent: String },
}

/// One human prompt and everything the agent did in response to it.
#[derive(Debug, Clone)]
pub struct Turn {
    pub index: usize,
    /// `None` for work that happened before any human input — a resumed
    /// session, a hook-triggered run, replayed history after a fork.
    pub prompt: Option<Prompt>,
    pub steps: Vec<Step>,
    /// The user interrupted somewhere in this turn.
    pub interrupted: bool,
}

#[derive(Debug, Clone)]
pub struct Prompt {
    pub text: String,
    pub timestamp: Option<DateTime<Utc>>,
    /// Harness-native id for the line that carried this prompt, where it has one.
    pub uuid: Option<String>,
}

/// One model response, folded back together from the several transcript lines
/// that carry it.
///
/// Both supported harnesses split a single response across many lines, and both
/// repeat or accumulate token counts while doing so — see each adapter's module
/// docs. Folding is where the token arithmetic is either right or badly wrong.
#[derive(Debug, Clone)]
pub struct Step {
    /// Whatever the harness uses to identify one response: Claude's
    /// `requestId`, Codex's reasoning-item id.
    pub response_id: Option<String>,
    pub message_id: Option<String>,
    /// Every native line id folded into this response, in file order. Some
    /// harnesses attribute file edits to one of *these* rather than to
    /// `message_id`, so an edit can only be traced back through this list.
    pub uuids: Vec<String>,
    pub model: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub text: String,
    /// Reasoning text, where the transcript kept any in the clear. Usually
    /// empty: both harnesses encrypt it.
    pub thinking: String,
    pub tool_calls: Vec<ToolCall>,
    /// Counted exactly once per response.
    pub usage: UsageTotals,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    /// The id results are matched back on.
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
    /// Flattened result text, truncated at index time rather than here.
    pub result: Option<String>,
    pub is_error: bool,
    /// The user rejected this call at the permission prompt.
    pub denied: bool,
}

impl ToolCall {
    /// The call in one line: its name, plus the one argument that says what it
    /// did. Shared by every surface so a shell call reads the same in all of
    /// them — presentation over the model, not knowledge of a file format.
    pub fn summary(&self, max: usize) -> String {
        const INTERESTING: [&str; 7] = [
            "command",
            "cmd",
            "file_path",
            "path",
            "pattern",
            "query",
            "description",
        ];
        let arg = INTERESTING
            .iter()
            .find_map(|key| self.input.get(key).and_then(|v| v.as_str()))
            // Codex passes a whole script as the input rather than an object,
            // so fall back to the raw string when there is no named argument.
            .or_else(|| self.input.as_str())
            .map(|value| first_line(value, max))
            .unwrap_or_default();
        if arg.is_empty() {
            self.name.clone()
        } else {
            format!("{}({arg})", self.name)
        }
    }
}

/// Eight characters of a session id that actually distinguish it.
///
/// Not simply the first eight: the harnesses use different uuid versions and
/// the entropy is in different places. Claude Code uses v4, which is random
/// throughout, so any eight characters will do. Codex uses v7, whose leading 48
/// bits are a millisecond timestamp — two sessions started seconds apart share
/// their first eight characters, and on the reference corpus half of them
/// collide. For those, the distinguishing part is at the end.
///
/// Keyed on the uuid version nibble rather than on the harness, so a harness
/// added later gets the right answer without anyone remembering this.
pub fn short_id(id: &str) -> &str {
    if is_time_ordered_uuid(id) {
        return id.get(id.len() - 8..).unwrap_or(id);
    }
    id.get(..8).unwrap_or(id)
}

/// A uuid whose version nibble marks it time-ordered (v7, and v1/v6 which lead
/// with a timestamp too).
fn is_time_ordered_uuid(id: &str) -> bool {
    let bytes = id.as_bytes();
    // `xxxxxxxx-xxxx-Vxxx-xxxx-xxxxxxxxxxxx` — the version is at index 14.
    bytes.len() == 36
        && bytes[8] == b'-'
        && bytes[13] == b'-'
        && matches!(bytes[14], b'1' | b'6' | b'7')
}

/// A timestamp the way every surface prints one. `None` is blank rather than
/// absent, so a column of them stays a column.
pub fn when(ts: Option<DateTime<Utc>>) -> String {
    match ts {
        Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        None => " ".repeat(16),
    }
}

/// The first non-blank line, cut to `max` characters.
pub fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else {
        line.to_string()
    }
}

/// A file edit, linked back to the work that made it — the attribution
/// `git blame` cannot give you, because it records the commit, not the reason.
#[derive(Debug, Clone)]
pub struct FileTouch {
    pub path: String,
    /// Native id of the response that caused the edit. Which id this is depends
    /// on the harness; resolve it with [`Session::steps_by_uuid`].
    pub message_id: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

impl UsageTotals {
    pub fn add(&mut self, other: UsageTotals) {
        self.input += other.input;
        self.output += other.output;
        self.cache_creation += other.cache_creation;
        self.cache_read += other.cache_read;
    }

    pub fn is_zero(&self) -> bool {
        *self == UsageTotals::default()
    }
}

impl Session {
    /// The short id search results print and you type back at `whence show`.
    pub fn short_id(&self) -> &str {
        short_id(&self.id)
    }

    pub fn steps(&self) -> impl Iterator<Item = &Step> {
        self.turns.iter().flat_map(|t| t.steps.iter())
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.steps().flat_map(|s| s.tool_calls.iter())
    }

    /// Human prompts only — the text you actually typed.
    pub fn prompts(&self) -> impl Iterator<Item = &Prompt> {
        self.turns.iter().filter_map(|t| t.prompt.as_ref())
    }

    /// Every native line id mapped to the turn and response it belongs to, so a
    /// [`FileTouch`] can be resolved to the work that caused it.
    pub fn steps_by_uuid(&self) -> HashMap<&str, (&Turn, &Step)> {
        let mut map = HashMap::new();
        for turn in &self.turns {
            for step in &turn.steps {
                for uuid in &step.uuids {
                    map.insert(uuid.as_str(), (turn, step));
                }
                if let Some(id) = &step.message_id {
                    map.entry(id.as_str()).or_insert((turn, step));
                }
            }
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_round_trips_through_its_token() {
        for harness in Harness::ALL {
            assert_eq!(harness.as_str().parse::<Harness>().unwrap(), harness);
        }
        assert_eq!("CLAUDE".parse::<Harness>().unwrap(), Harness::Claude);
        assert!("gemini".parse::<Harness>().is_err());
    }

    #[test]
    fn short_id_distinguishes_time_ordered_uuids() {
        // Two real Codex (v7) ids from sessions started a second apart: they
        // share their first eight characters, so the tail is what identifies them.
        let a = "019f4eca-3588-7fc0-9b71-5a240f6025b7";
        let b = "019f4eca-901b-7d91-9f65-cda91498aa04";
        assert_eq!(&a[..8], &b[..8], "the fixture must actually collide");
        assert_ne!(short_id(a), short_id(b));
        assert_eq!(short_id(a), "5a240f6025b7"[4..].to_string());

        // A Claude (v4) id keeps the familiar leading form.
        let c = "05fd3fbf-c446-49e9-8d96-04f267693991";
        assert_eq!(short_id(c), "05fd3fbf");
    }

    #[test]
    fn short_id_tolerates_ids_that_are_not_uuids() {
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id(""), "");
    }

    #[test]
    fn tool_summary_falls_back_to_a_bare_string_input() {
        let call = ToolCall {
            id: "1".into(),
            name: "exec".into(),
            input: serde_json::json!("ls -la\nsecond line"),
            result: None,
            is_error: false,
            denied: false,
        };
        assert_eq!(call.summary(80), "exec(ls -la)");
    }
}
