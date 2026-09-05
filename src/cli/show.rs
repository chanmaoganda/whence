//! `whence show` — read the conversation a search result points at.
//!
//! Goes through `source` rather than the index, so reading a session back never
//! depends on the index being built or current. Text goes through `render`, so
//! when markdown rendering arrives this surface gets it for free.

use super::report::when;
use anyhow::{Context, Result};
use whence::model::{first_line, SessionKind, Turn};
use whence::render::{self, Block};
use whence::source::{self, Transcript};

pub fn run(transcripts: &[Transcript], target: &str, after: usize, thinking: bool) -> Result<()> {
    let (prefix, wanted) = match target.split_once('#') {
        Some((prefix, turn)) => (
            prefix,
            Some(
                turn.parse::<usize>()
                    .with_context(|| format!("{turn:?} is not a turn number — try `641a2ec6#2`"))?,
            ),
        ),
        None => (target, None),
    };
    anyhow::ensure!(!prefix.is_empty(), "give a session id, e.g. `641a2ec6#2`");

    let found = source::by_session_prefix(transcripts, prefix);
    match found.len() {
        0 => anyhow::bail!("no session matching {prefix:?}"),
        1 => {}
        n => {
            eprintln!("{n} sessions match {prefix:?} — use more of the id:");
            for t in found.iter().take(10) {
                eprintln!("  {}  {}", t.harness, t.path.display());
            }
            anyhow::bail!("ambiguous session {prefix:?}");
        }
    }
    let transcript = &found[0];
    let (session, _) = source::normalize_with(transcript.harness, &transcript.path)
        .with_context(|| format!("reading {}", transcript.path.display()))?;

    println!("harness  {}", session.harness);
    println!("session  {}", session.id);
    println!("project  {}", session.project);
    if let Some(title) = &session.title {
        println!("title    {title}");
    }
    if let Some(model) = &session.model {
        println!("model    {model}");
    }
    if let SessionKind::Subagent { agent } = &session.kind {
        println!("kind     subagent ({agent})");
    }
    println!("source   {}", transcript.path.display());
    println!();

    let Some(wanted) = wanted else {
        // No turn asked for: the map, so you can pick one.
        for turn in &session.turns {
            let prompt = turn
                .prompt
                .as_ref()
                .map(|p| first_line(&p.text, 76))
                .unwrap_or_else(|| "(no prompt — resumed or hook-driven)".into());
            println!(
                "  #{:<3} {:<16} {}{}",
                turn.index,
                when(turn.prompt.as_ref().and_then(|p| p.timestamp)),
                prompt,
                if turn.interrupted {
                    "  [interrupted]"
                } else {
                    ""
                }
            );
        }
        println!(
            "\nread one with:  whence show {}#<turn>",
            session.short_id()
        );
        return Ok(());
    };

    let last = wanted + after;
    anyhow::ensure!(
        session.turns.iter().any(|t| t.index == wanted),
        "session {} has no turn #{wanted} (it has {})",
        session.short_id(),
        session.turns.len()
    );
    for turn in session
        .turns
        .iter()
        .filter(|t| (wanted..=last).contains(&t.index))
    {
        print_turn(turn, thinking);
    }
    Ok(())
}

fn print_turn(turn: &Turn, thinking: bool) {
    println!(
        "─── #{} {} {}",
        turn.index,
        when(turn.prompt.as_ref().and_then(|p| p.timestamp)).trim(),
        if turn.interrupted {
            "[interrupted]"
        } else {
            ""
        }
    );
    if let Some(prompt) = &turn.prompt {
        for line in prompt.text.lines() {
            println!("> {line}");
        }
        println!();
    }
    for step in &turn.steps {
        if thinking && !step.thinking.trim().is_empty() {
            for line in step.thinking.lines() {
                println!("  ~ {line}");
            }
        }
        if !step.text.trim().is_empty() {
            print_body(&step.text);
        }
        for call in &step.tool_calls {
            let mark = match (call.denied, call.is_error) {
                (true, _) => "  ✗ denied",
                (_, true) => "  ✗ error",
                _ => "",
            };
            println!("  · {}{}", call.summary(80), mark);
        }
        println!();
    }
}

/// Print a reply through the renderer. Today that only distinguishes fenced
/// code, which is marked rather than reflowed; when a markdown renderer lands,
/// this is where it takes effect.
fn print_body(text: &str) {
    for block in render::renderer().parse(text).blocks {
        match block {
            Block::Code { lang, lines } => {
                println!("  ┌─ {}", lang.unwrap_or_else(|| "code".into()));
                for line in lines {
                    println!("  │ {line}");
                }
                println!("  └─");
            }
            other => {
                for span in block_spans(&other) {
                    for line in span.lines() {
                        println!("  {line}");
                    }
                }
            }
        }
    }
}

fn block_spans(block: &Block) -> Vec<String> {
    match block {
        Block::Paragraph(spans)
        | Block::Heading { spans, .. }
        | Block::Bullet { spans, .. }
        | Block::Quote(spans) => spans.iter().map(|s| s.text.clone()).collect(),
        Block::Rule => vec!["---".into()],
        Block::Code { lines, .. } => lines.clone(),
    }
}
