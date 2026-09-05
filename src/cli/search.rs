//! `whence <words>` and `whence file` — asking the index a question.

use super::report::when;
use anyhow::Result;
use std::collections::HashSet;
use whence::index::SearchIndex;
use whence::model::{first_line, short_id};
use whence::search::{Excerpt, Hit, Query, Results};

pub fn search(index: &SearchIndex, query: &Query) -> Result<()> {
    let Results { hits, relaxed } = index.search(query)?;
    if hits.is_empty() {
        println!("no matches.");
        return Ok(());
    }
    if relaxed {
        println!("nothing matched exactly — relaxed to fuzzy matching\n");
    }
    let sessions: HashSet<&str> = hits.iter().map(|h| h.session.as_str()).collect();
    let harnesses: HashSet<&str> = hits.iter().map(|h| h.harness.as_str()).collect();
    let mut names: Vec<&str> = harnesses.into_iter().collect();
    names.sort_unstable();
    println!(
        "{} matches across {} sessions ({})\n",
        hits.len(),
        sessions.len(),
        names.join(", ")
    );
    for hit in &hits {
        print_hit(hit);
    }
    println!("read one with:  whence show {}", show_target(&hits[0]));
    Ok(())
}

pub fn file_history(index: &SearchIndex, path: &str, limit: usize) -> Result<()> {
    let hits = index.file_history(path, limit)?;
    if hits.is_empty() {
        println!("no indexed edits to {path} — try a shorter suffix, like just the filename.");
        return Ok(());
    }
    let sessions: HashSet<&str> = hits.iter().map(|h| h.session.as_str()).collect();
    println!("{} edits across {} sessions\n", hits.len(), sessions.len());
    for hit in &hits {
        if let Some(file) = &hit.file {
            println!("{}  {}", when(hit.timestamp), file);
        }
        println!("  {}", hit_location(hit));
        println!("  {}\n", indent(&excerpt(&hit.excerpt)));
    }
    println!("read one with:  whence show {}", show_target(&hits[0]));
    Ok(())
}

fn print_hit(hit: &Hit) {
    println!(
        "{}  {:<6}  {:<6}  {}",
        when(hit.timestamp),
        hit.harness,
        hit.kind,
        hit_location(hit)
    );
    println!("  {}\n", indent(&excerpt(&hit.excerpt)));
}

pub fn show_target(hit: &Hit) -> String {
    format!("{}#{}", short_id(&hit.session), hit.turn)
}

/// Where a hit came from, in the form you would use to go read it.
fn hit_location(hit: &Hit) -> String {
    let mut out = format!("{}  {}", hit.project, show_target(hit));
    if !hit.title.is_empty() {
        out.push_str(&format!("  — {}", first_line(&hit.title, 60)));
    }
    out
}

fn excerpt(excerpt: &Excerpt) -> String {
    excerpt
        .render("\u{1b}[1m", "\u{1b}[0m")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn indent(text: &str) -> String {
    text.replace('\n', "\n  ")
}
