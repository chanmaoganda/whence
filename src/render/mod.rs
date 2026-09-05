//! Turning agent-authored text into something displayable.
//!
//! Nearly everything in a transcript is markdown: replies are written as
//! markdown by every harness, and prompts are typed as markdown by most people.
//! Today we show it as-is. This module exists so that showing it *properly*
//! later is a new [`Render`] implementation rather than an edit to every
//! surface that prints text.
//!
//! The seam is deliberately placed here, between the model and the display:
//!
//! ```text
//!   Step::text (markdown source)
//!        │
//!        ├─▶ index  ── indexed as source, never rendered ──▶ search
//!        │
//!        └─▶ Render::parse ──▶ Doc ──▶ wrap(width) ──▶ CLI / TUI
//! ```
//!
//! Two rules that a markdown implementation must not break:
//!
//! 1. **The index reads the source, not the rendering.** You search for what was
//!    written — backticks, asterisks and all — and a hit's offsets have to point
//!    into the stored text. Rendering on the way into the index would make
//!    excerpt highlighting point at the wrong characters.
//! 2. **Wrapping stays downstream of rendering.** A [`Doc`] carries unwrapped
//!    spans; the display layer wraps them at the width it is actually drawing
//!    at, measuring display width and breaking mid-run. Chinese prose has no
//!    spaces, so a renderer that pre-wraps on whitespace hides most of this
//!    corpus. See `tui::ui::wrap`.
//!
//! When the time comes, the implementation to add is `Markdown`, parsing with
//! `pulldown-cmark` into the same [`Doc`]. Nothing above this module changes.

/// A parsed body of text, ready to be laid out at a width not yet known.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Doc {
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// Flowing text. Wrappable.
    Paragraph(Vec<Span>),
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    /// A fenced code block. **Never wrapped and never reflowed** — it is shown
    /// verbatim and scrolled sideways if it does not fit.
    Code {
        lang: Option<String>,
        lines: Vec<String>,
    },
    Bullet {
        depth: u8,
        spans: Vec<Span>,
    },
    Quote(Vec<Span>),
    Rule,
}

/// A run of text with one emphasis. `Plain` produces exactly one span per
/// block; a markdown renderer will produce many.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub emphasis: Emphasis,
}

impl Span {
    pub fn plain(text: impl Into<String>) -> Self {
        Span {
            text: text.into(),
            emphasis: Emphasis::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Emphasis {
    #[default]
    None,
    Strong,
    Emph,
    Code,
    Link,
}

/// Parsing agent text into displayable blocks.
pub trait Render: Send + Sync {
    fn parse(&self, source: &str) -> Doc;
}

/// What we do today: no inline markup, but fenced code is still recognised.
///
/// Recognising fences is not an early start on markdown — it is a layout
/// correctness fix. A code block that gets reflowed to the terminal width is
/// unreadable and, worse, is no longer the command you could copy and run. Every
/// other construct falls through as plain text.
pub struct Plain;

const FENCE: &str = "```";

impl Render for Plain {
    fn parse(&self, source: &str) -> Doc {
        let mut blocks = Vec::new();
        let mut paragraph: Vec<String> = Vec::new();
        let mut fence: Option<(Option<String>, Vec<String>)> = None;

        let flush = |paragraph: &mut Vec<String>, blocks: &mut Vec<Block>| {
            if !paragraph.is_empty() {
                blocks.push(Block::Paragraph(vec![Span::plain(paragraph.join("\n"))]));
                paragraph.clear();
            }
        };

        for line in source.lines() {
            match &mut fence {
                // Inside a fence everything is content until the closing fence,
                // including blank lines and anything that looks like markup.
                Some((lang, lines)) => {
                    if line.trim_start().starts_with(FENCE) {
                        blocks.push(Block::Code {
                            lang: lang.take(),
                            lines: std::mem::take(lines),
                        });
                        fence = None;
                    } else {
                        lines.push(line.to_string());
                    }
                }
                None if line.trim_start().starts_with(FENCE) => {
                    flush(&mut paragraph, &mut blocks);
                    let lang = line.trim_start().trim_start_matches(FENCE).trim();
                    fence = Some(((!lang.is_empty()).then(|| lang.to_string()), Vec::new()));
                }
                None if line.trim().is_empty() => flush(&mut paragraph, &mut blocks),
                None => paragraph.push(line.to_string()),
            }
        }
        // An unterminated fence is common in a truncated or interrupted reply;
        // keep what we have rather than dropping it.
        if let Some((lang, lines)) = fence {
            blocks.push(Block::Code { lang, lines });
        }
        flush(&mut paragraph, &mut blocks);
        Doc { blocks }
    }
}

static PLAIN: Plain = Plain;

/// The renderer every surface uses. One place to switch over.
pub fn renderer() -> &'static dyn Render {
    &PLAIN
}

impl Doc {
    /// The text back as a flat string — what you print when you have no styling
    /// to apply, and what a test asserts on.
    pub fn to_plain_text(&self) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            if !out.is_empty() {
                out.push('\n');
            }
            match block {
                Block::Code { lines, .. } => out.push_str(&lines.join("\n")),
                Block::Rule => out.push_str("---"),
                Block::Paragraph(spans)
                | Block::Heading { spans, .. }
                | Block::Bullet { spans, .. }
                | Block::Quote(spans) => {
                    for span in spans {
                        out.push_str(&span.text);
                    }
                }
            }
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_fences_survive_as_verbatim_blocks() {
        let doc = Plain
            .parse("here you go:\n\n```rust\nfn main() {}\n\n// blank line inside\n```\n\ndone");
        assert_eq!(
            doc.blocks,
            vec![
                Block::Paragraph(vec![Span::plain("here you go:")]),
                Block::Code {
                    lang: Some("rust".into()),
                    lines: vec![
                        "fn main() {}".into(),
                        String::new(),
                        "// blank line inside".into()
                    ],
                },
                Block::Paragraph(vec![Span::plain("done")]),
            ]
        );
    }

    #[test]
    fn an_unterminated_fence_keeps_its_content() {
        let doc = Plain.parse("output:\n\n```\ncargo build\n");
        assert!(matches!(doc.blocks[1], Block::Code { .. }));
        assert!(doc.to_plain_text().contains("cargo build"));
    }

    #[test]
    fn prose_is_untouched_including_inline_markup() {
        // Plain must not eat markup it cannot render, or search excerpts and
        // the displayed text would disagree about what was written.
        let doc = Plain.parse("use **bold** and `code` here");
        assert_eq!(doc.to_plain_text().trim(), "use **bold** and `code` here");
    }
}
