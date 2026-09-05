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

use super::{ParseStats, Source};
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
