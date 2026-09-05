//! `whence inspect` — read one transcript and report what came out of it.
//!
//! This is the debugging surface for the adapters: it prints the numbers a
//! format change would move first, and it names the harness it decided on, so a
//! mis-sniffed file is obvious rather than mysterious.

use super::report::{preview_list, thousands};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use whence::model::{first_line, SessionKind};
use whence::source;

pub fn run(path: &Path, show_turns: bool) -> Result<()> {
    anyhow::ensure!(path.is_file(), "{} is not a file", path.display());
    let harness = source::sniff_file(path).with_context(|| {
        format!(
            "{} does not look like a transcript from any harness whence knows",
            path.display()
        )
    })?;
    let (session, stats) = source::normalize_with(harness, path)
        .with_context(|| format!("reading {}", path.display()))?;

    println!("harness   {}", session.harness);
    println!("session   {}", session.id);
    println!("project   {}", session.project);
    if let Some(title) = &session.title {
        println!("title     {title}");
    }
    if let Some(model) = &session.model {
        println!("model     {model}");
    }
    if let Some(branch) = &session.branch {
        println!("branch    {branch}");
    }
    if let Some(version) = &session.agent_version {
        println!("version   {version}");
    }
    if let SessionKind::Subagent { agent } = &session.kind {
        println!("kind      subagent ({agent})");
    }
    if let (Some(a), Some(b)) = (session.started_at, session.ended_at) {
        println!(
            "span      {} -> {}",
            a.format("%Y-%m-%d %H:%M"),
            b.format("%H:%M")
        );
    }
    println!(
        "lines     {} parsed, {} failed",
        stats.lines, stats.parse_errors
    );
    println!(
        "turns     {} ({} human prompts, {} interrupted)",
        session.turns.len(),
        session.prompts().count(),
        session.turns.iter().filter(|t| t.interrupted).count()
    );
    println!(
        "responses {} (folded from raw lines)",
        session.steps().count()
    );

    // The headline check: what the token numbers look like if you trust the
    // transcript line by line, against what folding says. Each harness lies in
    // its own way, so the naive reading is computed per harness.
    let naive = naive_output_tokens(harness, path)?;
    println!(
        "tokens    out {} folded / {} naive ({})",
        thousands(session.usage.output),
        thousands(naive),
        error_note(session.usage.output, naive),
    );
    println!(
        "          in {} / cache write {} / cache read {}",
        thousands(session.usage.input),
        thousands(session.usage.cache_creation),
        thousands(session.usage.cache_read)
    );

    let mut tools: HashMap<&str, usize> = HashMap::new();
    let mut errors = 0usize;
    for call in session.tool_calls() {
        *tools.entry(call.name.as_str()).or_default() += 1;
        if call.is_error {
            errors += 1;
        }
    }
    let mut tools: Vec<_> = tools.into_iter().collect();
    tools.sort_by_key(|&(name, count)| (std::cmp::Reverse(count), name));
    println!(
        "tools     {} calls, {errors} errored — {}",
        session.tool_calls().count(),
        tools
            .iter()
            .map(|(n, c)| format!("{n}:{c}"))
            .collect::<Vec<_>>()
            .join(" ")
    );

    if !session.file_touches.is_empty() {
        let mut paths: Vec<&str> = session
            .file_touches
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        paths.sort_unstable();
        paths.dedup();
        // Attribution is the point of recording these: how many resolve back to
        // the response that made them.
        let by_uuid = session.steps_by_uuid();
        let attributed = session
            .file_touches
            .iter()
            .filter(|t| {
                t.message_id
                    .as_deref()
                    .is_some_and(|id| by_uuid.contains_key(id))
            })
            .count();
        println!(
            "files     {} touched ({}/{} edits attributed): {}",
            paths.len(),
            attributed,
            session.file_touches.len(),
            preview_list(&paths, 5)
        );
    }

    if show_turns {
        println!();
        for turn in &session.turns {
            let Some(prompt) = &turn.prompt else { continue };
            println!(
                "  #{:<3} {}{}",
                turn.index,
                first_line(&prompt.text, 90),
                if turn.interrupted {
                    " [interrupted]"
                } else {
                    ""
                }
            );
        }
    }
    Ok(())
}

fn error_note(folded: u64, naive: u64) -> String {
    if folded == 0 {
        return "no usage recorded".into();
    }
    let error = (naive as f64 / folded as f64 - 1.0) * 100.0;
    if error.abs() < 0.5 {
        "the naive reading happens to agree here".into()
    } else {
        format!("{error:+.0}% error if read naively")
    }
}

/// What a reader who trusted every line would get for output tokens.
///
/// Claude Code repeats one response's `usage` on each of its lines, so the naive
/// error is summing a repeated value. Codex accumulates a running total, so the
/// naive error is summing a cumulative one — far larger. Both are worth showing
/// because both are the mistake you would make writing this from scratch.
fn naive_output_tokens(harness: whence::model::Harness, path: &Path) -> Result<u64> {
    use std::io::BufRead;
    use whence::model::Harness;

    let reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut total = 0u64;
    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        total += match harness {
            Harness::Claude => v
                .pointer("/message/usage/output_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0),
            Harness::Codex => v
                .pointer("/payload/info/total_token_usage/output_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0),
        };
    }
    Ok(total)
}
