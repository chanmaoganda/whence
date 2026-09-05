//! The command line.
//!
//! `main` stays thin: this module owns the argument shapes and the dispatch,
//! and each command owns its own output.

mod completions;
mod index;
mod inspect;
mod report;
mod search;
mod show;
mod stats;
mod tui;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueHint};
use std::path::PathBuf;
use whence::model::Harness;
use whence::source::{self, Root, Transcript};

#[derive(Parser)]
#[command(
    name = "whence",
    version,
    about = "Search and replay your coding agents' history",
    // `whence <words>` searches. Everything else is a named subcommand.
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// Transcript root. Defaults to every harness installed here; the harness of
    /// a file under an explicit root is worked out by reading it.
    #[arg(long, global = true, value_hint = ValueHint::DirPath)]
    root: Option<PathBuf>,
    /// Only this harness. Repeatable: `--harness claude --harness codex`.
    #[arg(long, global = true, value_name = "NAME")]
    harness: Vec<Harness>,
    /// Index location (default: ~/.cache/whence/index)
    #[arg(long, global = true, value_hint = ValueHint::DirPath)]
    index: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    search: SearchArgs,
}

#[derive(Args, Clone, Debug)]
pub struct SearchArgs {
    /// Words to search for. Supports `+must`, `-not` and `"phrases"`.
    query: Vec<String>,
    /// Only this project; matched on path segments, e.g. `rtrade`.
    #[arg(long)]
    project: Option<String>,
    /// One of prompt, reply, think, edit.
    #[arg(long, value_parser = ["prompt", "reply", "think", "edit"])]
    kind: Option<String>,
    /// Only turns that called this tool, e.g. `Bash`.
    #[arg(long)]
    tool: Option<String>,
    /// Only since a date (2026-08-01) or an age (30d).
    #[arg(long)]
    since: Option<String>,
    /// How many results to keep. Defaults to 20 in the list and 200 in the TUI,
    /// where scrolling is free.
    #[arg(long)]
    limit: Option<usize>,
    /// Match loosely from the start, rather than only when nothing matched.
    #[arg(long, conflicts_with = "exact")]
    fuzzy: bool,
    /// Never relax the query, even when it finds nothing.
    #[arg(long)]
    exact: bool,
    /// Do not refresh the index before searching.
    #[arg(long)]
    no_refresh: bool,
}

impl SearchArgs {
    fn into_query(self, harness: Vec<Harness>, limit: usize) -> Result<whence::search::Query> {
        use whence::search::{self, Fuzzy};
        Ok(search::Query {
            text: self.query.join(" "),
            harness,
            project: self.project,
            kind: self.kind.as_deref().map(search::parse_kind).transpose()?,
            tool: self.tool,
            since: self.since.as_deref().map(search::parse_since).transpose()?,
            limit: self.limit.unwrap_or(limit),
            fuzzy: match (self.fuzzy, self.exact) {
                (true, _) => Fuzzy::Always,
                (_, true) => Fuzzy::Never,
                _ => Fuzzy::Auto,
            },
        })
    }
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
    /// Build or refresh the search index. Unchanged transcripts are not re-read.
    Index {
        /// Re-read every transcript instead of only what changed.
        #[arg(long)]
        force: bool,
    },
    /// Search prompts, replies and thinking. The same as `whence <words>`,
    /// spelled out for when a word collides with a subcommand name.
    Search {
        #[command(flatten)]
        args: SearchArgs,
    },
    /// Which sessions changed a file, and why.
    File {
        /// Path or path suffix, e.g. `src/normalize.rs`.
        #[arg(value_hint = ValueHint::AnyPath)]
        path: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Do not refresh the index first.
        #[arg(long)]
        no_refresh: bool,
    },
    /// Print a shell completion script: `whence completions fish`.
    Completions {
        /// bash, zsh or fish. Detected from $SHELL when omitted.
        shell: Option<completions::Shell>,
        /// Write the script where the shell already looks, instead of stdout.
        #[arg(long)]
        install: bool,
    },
    /// Browse the corpus: search as you type, read what you find.
    Tui {
        #[command(flatten)]
        args: SearchArgs,
    },
    /// Read a conversation a search pointed at: `whence show 641a2ec6#2`.
    Show {
        /// Session id prefix, optionally with the turn: `641a2ec6` or `641a2ec6#2`.
        target: String,
        /// Also print this many turns after the one named.
        #[arg(short = 'A', long, default_value_t = 0)]
        after: usize,
        /// Include the agent's thinking, where the transcript kept any.
        #[arg(long)]
        thinking: bool,
    },
}

impl Cli {
    pub fn run(mut self) -> Result<()> {
        let Some(command) = self.command.take() else {
            // No subcommand: the bare words are the query.
            if self.search.query.is_empty() {
                use clap::CommandFactory;
                Cli::command().print_help()?;
                println!();
                return Ok(());
            }
            return self.run_search(self.search.clone());
        };
        match command {
            Command::Sources => stats::sources(&self.roots()?),
            Command::Inspect { file, turns } => inspect::run(&file, turns),
            Command::Stats => stats::run(&self.transcripts()?),
            Command::Index { force } => index::build(&self, force),
            Command::Search { ref args } => self.run_search(args.clone()),
            Command::File {
                ref path,
                limit,
                no_refresh,
            } => {
                let index = index::for_query(&self, no_refresh)?;
                search::file_history(&index, path, limit)
            }
            Command::Tui { ref args } => self.run_tui(args.clone()),
            Command::Completions { shell, install } => completions::run(shell, install),
            Command::Show {
                ref target,
                after,
                thinking,
            } => show::run(&self.transcripts()?, target, after, thinking),
        }
    }

    fn run_search(&self, args: SearchArgs) -> Result<()> {
        let no_refresh = args.no_refresh;
        let query = args.into_query(self.harness.clone(), 20)?;
        let index = index::for_query(self, no_refresh)?;
        search::search(&index, &query)
    }

    fn run_tui(&self, args: SearchArgs) -> Result<()> {
        let no_refresh = args.no_refresh;
        let query = args.into_query(self.harness.clone(), 200)?;
        let index = index::for_query(self, no_refresh)?;
        tui::run(index, query)
    }

    /// The roots to read, after `--root` and `--harness` are applied.
    fn roots(&self) -> Result<Vec<Root>> {
        let mut roots = match &self.root {
            // An explicit root has no harness attached to it, so it stands in
            // for one root per harness; `transcripts` sniffs the files.
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

    fn index_dir(&self) -> Result<PathBuf> {
        self.index
            .clone()
            .or_else(whence::index::default_index_dir)
            .ok_or_else(|| anyhow::anyhow!("could not determine an index location; pass --index"))
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
