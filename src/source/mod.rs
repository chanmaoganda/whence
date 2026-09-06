//! Harness adapters: the only place that knows what a transcript looks like on
//! disk.
//!
//! Every agent writes JSONL, and there the similarity ends. Claude Code writes
//! one line per content block with a repeated `usage` payload; Codex writes an
//! event log where token counts accumulate. Both need real work to read
//! correctly, and neither's mistakes should be visible to anything above this
//! module.
//!
//! A [`Source`] turns files into [`Session`]s. Adding a harness means adding a
//! directory here, a [`Harness`] variant, and an entry in [`ALL`] — nothing
//! else in the tree changes.
//!
//! Two ways in, and the difference matters for cost:
//!
//! * [`default_roots`] asks each adapter where its own transcripts live, so the
//!   harness of every file is known from the directory it was found in. No file
//!   is opened to decide what it is.
//! * [`transcripts_under`] takes a directory you named, where the harness is
//!   not known, and [`sniff`]s each file's first lines. One short read per file.

use crate::model::{Harness, Session};
use std::fmt;
use std::io::BufRead;
use std::path::{Path, PathBuf};

pub mod claude;
pub mod codex;

/// How a file parsed. A non-zero `parse_errors` means a format change or a bug,
/// never a reason to have dropped the file.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ParseStats {
    pub lines: usize,
    pub parse_errors: usize,
}

/// One transcript file, and which adapter owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub harness: Harness,
    pub path: PathBuf,
}

/// A directory holding one harness's transcripts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub harness: Harness,
    pub path: PathBuf,
}

/// A session, in the terms a harness needs in order to open it again.
///
/// Borrowed rather than a whole [`Session`] because the results list has never
/// parsed one — a [`Hit`](crate::search::Hit) carries these three facts and
/// nothing more, and a result is exactly where you decide to go back.
#[derive(Debug, Clone, Copy)]
pub struct Resumable<'a> {
    /// The whole id. Eight characters find a session *here*; a harness wants
    /// all of it.
    pub id: &'a str,
    /// The directory the session ran in. Both harnesses look for a session
    /// under the project it belongs to, so the id alone finds nothing from
    /// anywhere else.
    pub project: &'a str,
    /// Where the transcript sits. It is the path, not the contents, that says
    /// a file is something other than a conversation you can walk back into.
    pub path: &'a Path,
}

/// How to pick a conversation back up: the harness's own command, and the
/// directory it has to run in.
///
/// Two fields rather than one string because the surfaces need different halves
/// of it. A shell needs the whole thing; the reader already names the project
/// in its header, so when the line is short the `cd` is the part to drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    /// `claude --resume <uuid>` — the id in full, always.
    pub command: String,
    /// The directory the session ran in; empty when the transcript never said.
    pub project: String,
}

impl Resume {
    /// The command spelled so it can be pasted anywhere: run in the session's
    /// own directory, because that is where its harness keeps it.
    pub fn pasteable(&self) -> String {
        if self.project.is_empty() {
            return self.command.clone();
        }
        format!("cd {} && {}", shell_quote(&self.project), self.command)
    }
}

impl fmt::Display for Resume {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.pasteable())
    }
}

impl<'a> From<&'a Session> for Resumable<'a> {
    fn from(session: &'a Session) -> Self {
        Resumable {
            id: &session.id,
            project: &session.project,
            path: &session.source_path,
        }
    }
}

/// Reading one agent's transcripts.
///
/// Implementations must be permissive in the same way: an unknown record type
/// is ignored, an unknown field is ignored, and a corrupt line is counted in
/// [`ParseStats::parse_errors`] rather than aborting the file. A transcript is
/// an append-only log written by a program that is still changing; refusing to
/// read one because a new field appeared would make the tool useless exactly
/// when you most want to look something up.
pub trait Source: Send + Sync {
    fn harness(&self) -> Harness;

    /// Where this harness keeps transcripts on this machine, if it is installed
    /// at all. Honours the harness's own home-directory override.
    fn default_root(&self) -> Option<PathBuf>;

    /// Every transcript belonging to this harness under `root`.
    ///
    /// This is a filename decision, not a content decision — it must not open
    /// files. Adapters use it to skip the other things a harness keeps beside
    /// its transcripts (Codex's `history.jsonl`, for instance).
    fn transcripts(&self, root: &Path) -> Vec<PathBuf>;

    /// Does this look like one of ours, judged from the first few non-empty
    /// lines of a file? Used only when the harness cannot be known from the
    /// directory. Must be cheap and must not panic on garbage.
    fn sniff(&self, lines: &[String]) -> bool;

    /// Rebuild one session from one file's lines.
    fn normalize(
        &self,
        path: &Path,
        lines: &mut dyn Iterator<Item = String>,
    ) -> (Session, ParseStats);

    /// What to type to pick this conversation back up in the agent that had
    /// it, or `None` when the harness will not take this transcript back.
    ///
    /// The other direction from everything else in this module: the rest reads
    /// a harness's files, and this hands one back. It spells the id in full and
    /// in the session's own directory, because the eight characters every
    /// surface prints are for finding a session again *in whence* — pasted at
    /// a shell they resume nothing.
    fn resume(&self, session: Resumable<'_>) -> Option<Resume>;
}

static CLAUDE: claude::ClaudeCode = claude::ClaudeCode;
static CODEX: codex::Codex = codex::Codex;

/// Every adapter, in [`Harness::ALL`] order.
pub const ALL: [&'static dyn Source; 2] = [&CLAUDE, &CODEX];

pub fn get(harness: Harness) -> &'static dyn Source {
    ALL.into_iter()
        .find(|s| s.harness() == harness)
        .expect("every Harness variant has an adapter in ALL")
}

/// How to resume a session in the harness that wrote it — see
/// [`Source::resume`].
pub fn resume(harness: Harness, session: Resumable<'_>) -> Option<Resume> {
    get(harness).resume(session)
}

/// Quote a path the shell would otherwise cut up. Directories with spaces in
/// them are ordinary, and a command you cannot paste is not an answer.
fn shell_quote(path: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "._-+=/:@~".contains(c);
    if path.chars().all(plain) {
        return path.to_string();
    }
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// Every harness that is actually installed here, with the directory its
/// transcripts live in. A harness you do not use contributes nothing.
pub fn default_roots() -> Vec<Root> {
    ALL.into_iter()
        .filter_map(|source| {
            let path = source.default_root()?;
            path.is_dir().then(|| Root {
                harness: source.harness(),
                path,
            })
        })
        .collect()
}

/// Every transcript under a set of known roots. The harness comes from the root
/// it was found in, so nothing is opened here.
pub fn transcripts(roots: &[Root]) -> Vec<Transcript> {
    roots
        .iter()
        .flat_map(|root| {
            let source = get(root.harness);
            source
                .transcripts(&root.path)
                .into_iter()
                .map(move |path| Transcript {
                    harness: root.harness,
                    path,
                })
        })
        .collect()
}

/// Every transcript under a directory you named, with each file's harness
/// decided by sniffing it. Files no adapter claims are left out.
///
/// This is the `--root` path. It costs one short read per file, which is why it
/// is not what the default scan uses.
pub fn transcripts_under(root: &Path) -> Vec<Transcript> {
    let mut found: Vec<Transcript> = ALL
        .into_iter()
        .flat_map(|source| {
            source
                .transcripts(root)
                .into_iter()
                .filter(|path| sniff_file(path).is_some_and(|h| h == source.harness()))
                .map(move |path| Transcript {
                    harness: source.harness(),
                    path,
                })
        })
        .collect();
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found.dedup_by(|a, b| a.path == b.path);
    found
}

/// The first `n` non-empty lines of a file — the evidence [`Source::sniff`]
/// judges on.
pub fn head_lines(path: &Path, n: usize) -> std::io::Result<Vec<String>> {
    let file = std::fs::File::open(path)?;
    Ok(std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .take(n)
        .collect())
}

/// How many lines a sniff gets to look at. Enough to see past a preamble of
/// harness-injected records, few enough to stay a single buffered read.
const SNIFF_LINES: usize = 8;

/// Which harness wrote this file, or `None` if nothing claims it.
pub fn sniff_file(path: &Path) -> Option<Harness> {
    let lines = head_lines(path, SNIFF_LINES).ok()?;
    if lines.is_empty() {
        return None;
    }
    ALL.into_iter()
        .find(|source| source.sniff(&lines))
        .map(|source| source.harness())
}

/// Transcripts whose filename contains `prefix`.
///
/// Session ids are uuids and results print only the first eight characters, so
/// that prefix is what you have in hand when you want to go read a conversation.
/// Claude Code names a file for its session id; Codex embeds it in a longer
/// name, so this matches anywhere in the stem rather than only at the start.
pub fn by_session_prefix(transcripts: &[Transcript], prefix: &str) -> Vec<Transcript> {
    let mut found: Vec<Transcript> = transcripts
        .iter()
        .filter(|t| {
            t.path
                .file_stem()
                .is_some_and(|stem| stem.to_string_lossy().contains(prefix))
        })
        .cloned()
        .collect();
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// Read one transcript whose harness is already known.
pub fn normalize_with(harness: Harness, path: &Path) -> std::io::Result<(Session, ParseStats)> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut lines = reader.lines().map_while(Result::ok);
    Ok(get(harness).normalize(path, &mut lines))
}

/// Read one transcript, working out which harness wrote it.
///
/// This is what `whence inspect` and `whence show` use: reading a session back
/// never depends on the index being built or current, and never depends on the
/// file having been found in a directory we recognise.
pub fn normalize_file(path: &Path) -> std::io::Result<(Session, ParseStats)> {
    let harness = sniff_file(path).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} does not look like a transcript from any harness whence knows",
                path.display()
            ),
        )
    })?;
    normalize_with(harness, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_harness_has_an_adapter() {
        for harness in Harness::ALL {
            assert_eq!(get(harness).harness(), harness);
        }
        assert_eq!(ALL.len(), Harness::ALL.len());
    }

    #[test]
    fn adapters_do_not_claim_each_others_transcripts() {
        let claude = vec![
            r#"{"type":"user","uuid":"a","sessionId":"s","message":{"content":"hi"}}"#.to_string(),
        ];
        let codex = vec![
            r#"{"timestamp":"2026-07-11T01:28:09.083Z","type":"session_meta","payload":{"session_id":"s","cwd":"/x"}}"#.to_string(),
        ];
        assert!(get(Harness::Claude).sniff(&claude));
        assert!(!get(Harness::Claude).sniff(&codex));
        assert!(get(Harness::Codex).sniff(&codex));
        assert!(!get(Harness::Codex).sniff(&claude));
    }

    #[test]
    fn nothing_claims_unrelated_jsonl() {
        let other = vec![r#"{"level":"info","msg":"server started"}"#.to_string()];
        for source in ALL {
            assert!(!source.sniff(&other), "{} claimed it", source.harness());
        }
    }
}
