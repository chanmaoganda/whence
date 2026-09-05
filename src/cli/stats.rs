//! `whence sources` and `whence stats` — the corpus from above.

use super::load;
use super::report::{pct, thousands, top, when};
use anyhow::Result;
use std::collections::HashMap;
use whence::model::{Harness, Session, SessionKind, UsageTotals};
use whence::source::{self, Root, Transcript};

/// What is installed on this machine, and how big each corpus is.
pub fn sources(roots: &[Root]) -> Result<()> {
    println!("{:<8}  {:>6}  root", "harness", "files");
    for root in roots {
        let count = source::get(root.harness).transcripts(&root.path).len();
        println!(
            "{:<8}  {:>6}  {}",
            root.harness.to_string(),
            count,
            root.path.display()
        );
    }
    let missing: Vec<String> = source::ALL
        .into_iter()
        .filter(|s| !roots.iter().any(|r| r.harness == s.harness()))
        .map(|s| s.harness().to_string())
        .collect();
    if !missing.is_empty() {
        println!("\nnot found here: {}", missing.join(", "));
    }
    Ok(())
}

pub fn run(transcripts: &[Transcript]) -> Result<()> {
    let started = std::time::Instant::now();
    let sessions = load(transcripts);
    let elapsed = started.elapsed();
    anyhow::ensure!(!sessions.is_empty(), "no transcripts could be read");

    let mut usage = UsageTotals::default();
    let mut tools: HashMap<String, usize> = HashMap::new();
    let mut projects: HashMap<String, usize> = HashMap::new();
    let mut files_touched: HashMap<String, usize> = HashMap::new();
    let mut models: HashMap<String, usize> = HashMap::new();
    let mut per_harness: HashMap<Harness, Corpus> = HashMap::new();
    let (mut prompts, mut interrupted, mut subagents, mut tool_errors) = (0, 0, 0, 0);

    for s in &sessions {
        let entry = per_harness.entry(s.harness).or_default();
        entry.absorb(s);

        usage.add(s.usage);
        prompts += s.prompts().count();
        interrupted += s.turns.iter().filter(|t| t.interrupted).count();
        if matches!(s.kind, SessionKind::Subagent { .. }) {
            subagents += 1;
        }
        *projects.entry(s.project.clone()).or_default() += 1;
        if let Some(model) = &s.model {
            *models.entry(model.clone()).or_default() += 1;
        }
        for call in s.tool_calls() {
            *tools.entry(call.name.clone()).or_default() += 1;
            if call.is_error {
                tool_errors += 1;
            }
        }
        for touch in &s.file_touches {
            *files_touched.entry(touch.path.clone()).or_default() += 1;
        }
    }

    println!(
        "sessions    {} ({} main, {} subagent) read in {:.1}s",
        sessions.len(),
        sessions.len() - subagents,
        subagents,
        elapsed.as_secs_f64()
    );
    println!("projects    {}", projects.len());
    println!(
        "prompts     {prompts} human turns, {interrupted} interrupted ({:.1}%)",
        pct(interrupted, prompts)
    );
    println!(
        "tokens      out {} / in {} / cache write {} / cache read {}",
        thousands(usage.output),
        thousands(usage.input),
        thousands(usage.cache_creation),
        thousands(usage.cache_read)
    );
    let total_calls: usize = tools.values().sum();
    println!(
        "tool calls  {} ({} errored, {:.1}%)",
        thousands(total_calls as u64),
        tool_errors,
        pct(tool_errors, total_calls)
    );

    // The per-harness split is the point of reading them together: the same
    // questions, answered in numbers that are finally comparable.
    println!(
        "\n{:<8}  {:>8}  {:>8}  {:>12}  {:>12}  span",
        "harness", "sessions", "prompts", "out tokens", "in tokens"
    );
    let mut split: Vec<(&Harness, &Corpus)> = per_harness.iter().collect();
    split.sort_by_key(|(h, _)| **h);
    for (harness, corpus) in split {
        println!(
            "{:<8}  {:>8}  {:>8}  {:>12}  {:>12}  {} .. {}",
            harness.to_string(),
            corpus.sessions,
            corpus.prompts,
            thousands(corpus.usage.output),
            thousands(corpus.usage.input),
            when(corpus.first).trim(),
            when(corpus.last).trim(),
        );
    }

    println!("\ntop tools");
    for (name, count) in top(&tools, 8) {
        println!("  {count:>6}  {name}");
    }
    if !models.is_empty() {
        println!("\nmodels");
        for (name, count) in top(&models, 6) {
            println!("  {count:>6}  {name}");
        }
    }
    println!("\nmost-edited files");
    for (path, count) in top(&files_touched, 8) {
        println!("  {count:>6}  {path}");
    }
    println!("\nbusiest projects");
    for (path, count) in top(&projects, 8) {
        println!("  {count:>6}  {path}");
    }
    Ok(())
}

/// One harness's slice of the corpus.
#[derive(Default)]
struct Corpus {
    sessions: usize,
    prompts: usize,
    usage: UsageTotals,
    first: Option<chrono::DateTime<chrono::Utc>>,
    last: Option<chrono::DateTime<chrono::Utc>>,
}

impl Corpus {
    fn absorb(&mut self, s: &Session) {
        self.sessions += 1;
        self.prompts += s.prompts().count();
        self.usage.add(s.usage);
        if let Some(start) = s.started_at {
            self.first = Some(self.first.map_or(start, |f| f.min(start)));
        }
        if let Some(end) = s.ended_at {
            self.last = Some(self.last.map_or(end, |l| l.max(end)));
        }
    }
}
