//! Oh My Pi (`omp`).
//!
//! Transcripts live in `~/.omp/agent/sessions/<encoded-cwd>/<iso>_<uuid>.jsonl`,
//! one file per session, appended to as the conversation goes. Beside each file
//! omp may keep a directory of the same name holding per-tool output logs
//! (`8.bash.log`, `11.hub.log`); those are not transcripts and are skipped by
//! extension.
//!
//! The traps this adapter absorbs are documented on [`normalize`]. The short
//! version: `reasoningTokens` is counted inside `output` rather than beside it,
//! and every tool call is written down twice.

pub mod normalize;
pub mod raw;

use super::{ParseStats, Resumable, Resume, Source};
use crate::model::{Harness, Session};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Omp;

impl Source for Omp {
    fn harness(&self) -> Harness {
        Harness::Omp
    }

    /// `~/.omp/agent/sessions`, allowing for both ways omp relocates it:
    /// `$PI_CODING_AGENT_DIR` names the agent directory outright, and a named
    /// profile moves the whole config root to `~/.omp/profiles/<name>`.
    fn default_root(&self) -> Option<PathBuf> {
        if let Some(dir) = crate::home::non_empty("PI_CODING_AGENT_DIR") {
            return Some(PathBuf::from(dir).join("sessions"));
        }
        let mut root = crate::home::dir()?.join(".omp");
        if let Some(profile) =
            crate::home::non_empty("OMP_PROFILE").or_else(|| crate::home::non_empty("PI_PROFILE"))
        {
            root = root.join("profiles").join(profile);
        }
        Some(root.join("agent").join("sessions"))
    }

    fn transcripts(&self, root: &Path) -> Vec<PathBuf> {
        WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect()
    }

    /// An omp line is a session envelope naming its working directory, or a
    /// message carrying one of the three roles.
    ///
    /// Both halves are needed. The header is the most distinctive record in the
    /// format but a truncated or resumed file may not start with one; a bare
    /// `"type": "message"` is common enough elsewhere that it would claim other
    /// people's logs on its own.
    fn sniff(&self, lines: &[String]) -> bool {
        const ROLES: [&str; 3] = ["assistant", "user", "toolResult"];
        lines.iter().any(|line| {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                return false;
            };
            match v.get("type").and_then(|t| t.as_str()) {
                Some("session") => v.get("id").is_some() && v.get("cwd").is_some(),
                Some("message") => v
                    .pointer("/message/role")
                    .and_then(|r| r.as_str())
                    .is_some_and(|r| ROLES.contains(&r)),
                _ => false,
            }
        })
    }

    fn normalize(
        &self,
        path: &Path,
        lines: &mut dyn Iterator<Item = String>,
    ) -> (Session, ParseStats) {
        normalize::normalize(path, lines)
    }

    /// `omp --resume <uuid>`, run where the session ran.
    ///
    /// omp will take an id prefix, but what it matches against is the sessions
    /// of the current working directory, so the `cd` carries as much of the
    /// answer as the id does.
    fn resume(&self, session: Resumable<'_>) -> Option<Resume> {
        Some(Resume {
            command: format!("omp --resume {}", session.id),
            project: session.project.to_string(),
        })
    }
}
