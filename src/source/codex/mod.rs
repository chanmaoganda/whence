//! Codex.
//!
//! Transcripts — "rollout files" — live in
//! `~/.codex/sessions/<yyyy>/<mm>/<dd>/rollout-<iso8601>-<uuid>.jsonl`, dated
//! directories rather than per-project ones, so the project a session belongs
//! to is only knowable from inside the file.
//!
//! The traps this adapter absorbs are documented on [`normalize`]: cumulative
//! token counts, every utterance recorded twice, and harness preambles
//! disguised as user messages.

pub mod normalize;
pub mod raw;

use super::{ParseStats, Source};
use crate::model::{Harness, Session};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Codex;

/// Rollout files are named for their kind. Codex keeps other JSONL beside them —
/// `~/.codex/history.jsonl` above all — that is not a transcript.
const ROLLOUT_PREFIX: &str = "rollout-";

impl Source for Codex {
    fn harness(&self) -> Harness {
        Harness::Codex
    }

    /// `~/.codex/sessions`, or `$CODEX_HOME/sessions` when set.
    fn default_root(&self) -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("CODEX_HOME") {
            return Some(PathBuf::from(dir).join("sessions"));
        }
        Some(
            PathBuf::from(std::env::var_os("HOME")?)
                .join(".codex")
                .join("sessions"),
        )
    }

    fn transcripts(&self, root: &Path) -> Vec<PathBuf> {
        WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .filter(|p| {
                p.extension().is_some_and(|e| e == "jsonl")
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(ROLLOUT_PREFIX))
            })
            .collect()
    }

    /// A rollout line is `{timestamp, type, payload}` with one of the outer
    /// kinds. Requiring `payload` is what separates this from Claude Code's
    /// records, which also carry a `type` but never wrap their content.
    fn sniff(&self, lines: &[String]) -> bool {
        const KINDS: [&str; 5] = [
            "session_meta",
            "response_item",
            "event_msg",
            "turn_context",
            "world_state",
        ];
        lines.iter().any(|line| {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                return false;
            };
            v.get("payload").is_some()
                && v.get("type")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| KINDS.contains(&t))
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
