//! whence — a local search engine and time machine for coding-agent transcripts.
//!
//! Your agents leave a detailed record of what you asked, what they did and why,
//! and then it is never read again. whence makes it searchable, replayable and
//! attributable, entirely on your own machine.
//!
//! ```text
//!   source/{claude,codex} ──▶ model ──┬──▶ index + search (tantivy, jieba)
//!    per-harness parsing              ├──▶ insights (corpus aggregates)
//!                                     ├──▶ render ──▶ CLI + TUI
//!                                     └──▶ MCP server (redacted)
//! ```
//!
//! The one architectural rule: [`source`] is the only place that knows what a
//! transcript looks like on disk, and [`model`] is what every other surface
//! reads. A harness that changes its format touches one directory; a new
//! harness *adds* one.

pub mod home;
pub mod index;
pub mod model;
pub mod render;
pub mod search;
pub mod source;
pub mod tokenize;
pub mod tui;
