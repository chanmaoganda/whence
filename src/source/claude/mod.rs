//! Claude Code.
//!
//! Transcripts live in `~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`,
//! with subagent conversations nested under a `subagents/` directory. One file
//! is one session, appended to as the conversation goes.
//!
//! The traps this adapter exists to absorb are documented on [`normalize`]. The
//! short version: one API response spans many lines and repeats its `usage` on
//! every one of them, and `"type": "user"` usually is not the human.

pub mod normalize;
pub mod raw;

use super::{ParseStats, Resumable, Resume, Source};
use crate::model::{Harness, Session};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct ClaudeCode;

impl Source for ClaudeCode {
    fn harness(&self) -> Harness {
        Harness::Claude
    }

    /// `~/.claude/projects`, or `$CLAUDE_CONFIG_DIR/projects` when set.
    fn default_root(&self) -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
            return Some(PathBuf::from(dir).join("projects"));
        }
        Some(crate::home::dir()?.join(".claude").join("projects"))
    }

    /// Every `.jsonl` under the root, including the subagent and workflow
    /// transcripts nested in subdirectories.
    fn transcripts(&self, root: &Path) -> Vec<PathBuf> {
        WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect()
    }

    /// A Claude Code line is a conversational record carrying the session
    /// envelope: a known `type`, plus a `uuid` or `sessionId`.
    ///
    /// Requiring the envelope and not just the `type` is what keeps this from
    /// claiming any log file that happens to have `"type": "user"` in it.
    fn sniff(&self, lines: &[String]) -> bool {
        const TYPES: [&str; 6] = [
            "user",
            "assistant",
            "system",
            "summary",
            "ai-title",
            "file-history-delta",
        ];
        lines.iter().any(|line| {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                return false;
            };
            let has_envelope = v.get("uuid").is_some() || v.get("sessionId").is_some();
            let known_type = v
                .get("type")
                .and_then(|t| t.as_str())
                .is_some_and(|t| TYPES.contains(&t));
            has_envelope && known_type
        })
    }

    fn normalize(
        &self,
        path: &Path,
        lines: &mut dyn Iterator<Item = String>,
    ) -> (Session, ParseStats) {
        normalize::normalize(path, lines)
    }

    /// `claude --resume <uuid>`, run where the session ran: Claude Code keeps
    /// transcripts per project directory and looks for the id only in the one
    /// belonging to the current working directory.
    ///
    /// A subagent is not a conversation `--resume` will take — it was never a
    /// session you drove — so what comes back is the conversation that spawned
    /// it, which is the directory its `subagents/` folder sits in. That is the
    /// one you would actually want to walk back into anyway.
    fn resume(&self, session: Resumable<'_>) -> Option<Resume> {
        let spawned_by = spawning_session(session.path);
        let id = spawned_by.as_deref().unwrap_or(session.id);
        Some(Resume {
            command: format!("claude --resume {id}"),
            project: session.project.to_string(),
        })
    }
}

/// The session a subagent transcript was spawned by, from
/// `<project>/<session-uuid>/subagents/<subagent-uuid>.jsonl`. `None` for a
/// conversation that is its own session.
///
/// A loop rather than two `parent()` calls: a subagent can spawn a subagent,
/// and only the outermost id is one Claude Code will reopen.
fn spawning_session(path: &Path) -> Option<String> {
    let mut dir = path.parent()?;
    let mut found = None;
    while dir.file_name().is_some_and(|name| name == "subagents") {
        let session = dir.parent()?;
        found = Some(session.file_name()?.to_string_lossy().into_owned());
        dir = session.parent()?;
    }
    found
}

/// Transcripts whose filename starts with `prefix`. Session ids are uuids and
/// results print only the first eight characters, so that prefix is what you
/// have in hand when you want to go read the conversation.
pub fn by_session_prefix(root: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = ClaudeCode
        .transcripts(root)
        .into_iter()
        .filter(|p| {
            p.file_stem()
                .is_some_and(|stem| stem.to_string_lossy().starts_with(prefix))
        })
        .collect();
    found.sort();
    found
}
