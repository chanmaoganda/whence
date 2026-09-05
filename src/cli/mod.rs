//! The command line.
//!
//! `main` stays thin: this module owns the argument shapes and the dispatch,
//! and each command owns its own output.

mod inspect;
mod report;
mod stats;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueHint};
use std::path::PathBuf;
use whence::model::Harness;
use whence::source::{self, Root, Transcript};

#[derive(Parser)]
#[command(
    name = "whence",
    version,
    about = "Search and replay your coding agents' history"
)]
pub struct Cli {
    /// Transcript root. Defaults to every harness installed here; the harness of
    /// a file under an explicit root is worked out by reading it.
    #[arg(long, global = true, value_hint = ValueHint::DirPath)]
    root: Option<PathBuf>,
    /// Only this harness. Repeatable.
    #[arg(long, global = true, value_name = "NAME")]
    harness: Vec<Harness>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Which harnesses are installed here, and how much each has recorded.
    Sources,
    /// Normalize one transcript and show what it contains.
    Inspect {
        #[arg(value_hint = ValueHint::FilePath)]
        file: PathBuf,
        /// Also print each turn's prompt.
        #[arg(long)]
        turns: bool,
    },
    /// Summarize the whole corpus, across every harness.
    Stats,
}

impl Cli {
    pub fn run(self) -> Result<()> {
        match self.command {
            Command::Sources => stats::sources(&self.roots()?),
            Command::Inspect { file, turns } => inspect::run(&file, turns),
            Command::Stats => stats::run(&self.transcripts()?),
        }
    }

    /// The roots to read, after `--root` and `--harness` are applied.
    fn roots(&self) -> Result<Vec<Root>> {
        let mut roots = match &self.root {
            // An explicit root has no harness attached to it; `transcripts`
            // sniffs the files instead. Represented here as one root per
            // harness over the same directory.
            Some(path) => {
                anyhow::ensure!(path.is_dir(), "{} is not a directory", path.display());
                Harness::ALL
                    .into_iter()
                    .map(|harness| Root {
                        harness,
                        path: path.clone(),
                    })
                    .collect()
            }
            None => source::default_roots(),
        };
        if !self.harness.is_empty() {
            roots.retain(|r| self.harness.contains(&r.harness));
        }
        anyhow::ensure!(
            !roots.is_empty(),
            "no transcripts found. Looked for: {}",
            source::ALL
                .into_iter()
                .filter_map(|s| Some(format!(
                    "{} in {}",
                    s.harness(),
                    s.default_root()?.display()
                )))
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok(roots)
    }

    fn transcripts(&self) -> Result<Vec<Transcript>> {
        let roots = self.roots()?;
        let mut found = match &self.root {
            Some(path) => source::transcripts_under(path),
            None => source::transcripts(&roots),
        };
        if !self.harness.is_empty() {
            found.retain(|t| self.harness.contains(&t.harness));
        }
        anyhow::ensure!(!found.is_empty(), "no transcripts found under those roots");
        Ok(found)
    }
}

/// Read every transcript in parallel, dropping the ones that will not open.
pub fn load(transcripts: &[Transcript]) -> Vec<whence::model::Session> {
    use rayon::prelude::*;
    transcripts
        .par_iter()
        .filter_map(|t| {
            source::normalize_with(t.harness, &t.path)
                .ok()
                .map(|(session, _)| session)
        })
        .collect()
}
