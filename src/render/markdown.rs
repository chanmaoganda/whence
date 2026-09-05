//! Markdown into a [`Doc`], with `pulldown-cmark` doing the parsing.
//!
//! This is the implementation the module docs next door promised. It changes
//! nothing above it: the same [`Block`]s come out, so the same `wrap` breaks
//! them and the same index keeps reading the *source* rather than this.
//!
//! Three things this has to get right for a transcript specifically:
//!
//! * **A fenced block is content, not markup.** It arrives as one
//!   [`Block::Code`] and is never touched again — the layout clips it rather
//!   than reflowing it, because a rewrapped shell command is no longer a
//!   command you can run. Same rule as `Plain`, now via the parser.
//! * **A soft break is a space, not a line.** Agents hard-wrap their prose at
//!   80 columns; keeping those breaks would mean a column of ragged text in a
//!   200-column pane. Only an explicit hard break survives as a newline.
//! * **Tables are structure worth keeping.** Replies are full of them, and a
//!   table shown as its pipes is the single ugliest thing in the reader.
//!
//! Inline markup becomes [`Emphasis`] on the spans, never characters: the
//! display layer decides what bold looks like, and a line may break in the
//! middle of a bold run because the wrapper works on characters.

use super::{Block, Doc, Emphasis, Render, Span};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// Markdown, as every harness writes it.
pub struct Markdown;

impl Render for Markdown {
    fn parse(&self, source: &str) -> Doc {
        let mut options = Options::empty();
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TASKLISTS);
        options.insert(Options::ENABLE_FOOTNOTES);

        let mut builder = Builder::default();
        for event in Parser::new_ext(source, options) {
            builder.event(event);
        }
        builder.finish()
    }
}

/// Accumulating state: the blocks so far, the inline run being collected, and
/// the stacks saying what that run is inside of.
#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    /// The inline run of the block currently being read.
    spans: Vec<Span>,
    /// Nested inline markup — `**a *b* **` is two entries deep.
    emphasis: Vec<Emphasis>,
    /// One entry per open list; `Some(n)` counts an ordered one.
    lists: Vec<Option<u64>>,
    /// The bullet or number for the item being opened, taken by the first
    /// block inside it. A second paragraph in the same item gets none, and so
    /// lines up under the first rather than growing a second bullet.
    marker: Option<String>,
    heading: Option<u8>,
    code: Option<(Option<String>, String)>,
    quote: usize,
    /// Where the text of the open link started, so the URL can be dropped when
    /// it would only repeat the text.
    links: Vec<(usize, String)>,
    table: Option<TableRows>,
}

/// A table under construction. Cells are flattened to strings: a table is laid
/// out in columns, and a column that has to be clipped cannot also carry
/// emphasis ranges through the clip.
#[derive(Default)]
struct TableRows {
    head: Vec<String>,
    rows: Vec<Vec<String>>,
    row: Vec<String>,
}

impl Builder {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(text) => self.styled(&text, Emphasis::Code),
            // Raw HTML is rare in a reply and never renderable here. `<br>` is
            // the one that carries meaning; everything else goes through as the
            // characters that were written.
            Event::Html(text) | Event::InlineHtml(text) => match text.trim() {
                "<br>" | "<br/>" | "<br />" => self.text("\n"),
                _ => self.text(&text),
            },
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.text("\n"),
            Event::Rule => {
                self.flush();
                self.blocks.push(Block::Rule);
            }
            Event::TaskListMarker(done) => self.text(if done { "[x] " } else { "[ ] " }),
            Event::FootnoteReference(name) => self.text(&format!("[^{name}]")),
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.styled(&text, Emphasis::Code)
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Heading { level, .. } => self.heading = Some(level as u8),
            Tag::BlockQuote(_) => self.quote += 1,
            Tag::CodeBlock(kind) => {
                self.flush();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        let lang = info.split_whitespace().next().unwrap_or("");
                        (!lang.is_empty()).then(|| lang.to_string())
                    }
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((lang, String::new()));
            }
            // A nested list interrupts the item it is inside: `- a` followed by
            // an indented `- b` must close `a` first, or the two run together
            // into one bullet.
            Tag::List(first) => {
                self.flush();
                self.lists.push(first);
            }
            Tag::Item => {
                self.marker = Some(match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let marker = format!("{n}.");
                        *n += 1;
                        marker
                    }
                    _ => "•".to_string(),
                });
            }
            Tag::Emphasis => self.emphasis.push(Emphasis::Emph),
            Tag::Strong => self.emphasis.push(Emphasis::Strong),
            Tag::Strikethrough => self.emphasis.push(Emphasis::Strike),
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.links.push((self.spans.len(), dest_url.to_string()));
                self.emphasis.push(Emphasis::Link);
            }
            Tag::Table(_) => {
                self.flush();
                self.table = Some(TableRows::default());
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.row.push(String::new());
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item => self.flush(),
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if let Some((lang, body)) = self.code.take() {
                    let body = body.strip_suffix('\n').unwrap_or(&body);
                    self.blocks.push(Block::Code {
                        lang,
                        lines: body.lines().map(str::to_string).collect(),
                    });
                }
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                self.marker = None;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.emphasis.pop();
            }
            TagEnd::Link | TagEnd::Image => {
                self.emphasis.pop();
                if let Some((at, url)) = self.links.pop() {
                    let text: String = self.spans[at..].iter().map(|s| s.text.as_str()).collect();
                    // `[the docs](https://…)` keeps its address; a bare
                    // `<https://…>`, where the text *is* the address, does not
                    // get to say it twice.
                    if !url.is_empty() && text.trim() != url {
                        self.spans.push(Span {
                            text: format!(" ({url})"),
                            emphasis: Emphasis::Link,
                        });
                    }
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.blocks.push(Block::Table {
                        head: table.head,
                        rows: table.rows,
                    });
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.head = std::mem::take(&mut table.row);
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        let emphasis = self.emphasis.last().copied().unwrap_or_default();
        self.styled(text, emphasis);
    }

    fn styled(&mut self, text: &str, emphasis: Emphasis) {
        if let Some((_, body)) = &mut self.code {
            body.push_str(text);
        } else if let Some(cell) = self.table.as_mut().and_then(|t| t.row.last_mut()) {
            cell.push_str(text);
        } else {
            self.spans.push(Span {
                text: text.to_string(),
                emphasis,
            });
        }
    }

    /// Close the inline run into whichever block it turned out to be in.
    fn flush(&mut self) {
        let spans = std::mem::take(&mut self.spans);
        let heading = self.heading.take();
        let marker = self.marker.take();
        if spans.iter().all(|s| s.text.trim().is_empty()) {
            return;
        }
        let spans = trim(spans);
        self.blocks.push(match heading {
            Some(level) => Block::Heading { level, spans },
            // A list wins over a quote: a quoted list reads as a list, and the
            // bullet is the part that carries the structure.
            None if !self.lists.is_empty() => Block::Bullet {
                depth: self.lists.len().saturating_sub(1) as u8,
                marker: marker.unwrap_or_default(),
                spans,
            },
            None if self.quote > 0 => Block::Quote(spans),
            None => Block::Paragraph(spans),
        });
    }

    fn finish(mut self) -> Doc {
        // A document that ends mid-block — a truncated or interrupted reply is
        // the common case — keeps what it had.
        if let Some((lang, body)) = self.code.take() {
            self.blocks.push(Block::Code {
                lang,
                lines: body.lines().map(str::to_string).collect(),
            });
        }
        self.flush();
        Doc {
            blocks: self.blocks,
        }
    }
}

/// Drop the whitespace at either end of a run, which is where a soft break
/// turned into a space that no longer has anything to separate.
fn trim(mut spans: Vec<Span>) -> Vec<Span> {
    if let Some(first) = spans.first_mut() {
        first.text = first.text.trim_start().to_string();
    }
    if let Some(last) = spans.last_mut() {
        last.text = last.text.trim_end().to_string();
    }
    spans.retain(|s| !s.text.is_empty());
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(source: &str) -> Vec<Block> {
        Markdown.parse(source).blocks
    }

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// Agents hard-wrap their prose at eighty columns. Keeping those breaks
    /// would mean a narrow column of text in a wide pane, so a soft break is a
    /// space and the display layer decides where the lines go.
    #[test]
    fn a_soft_break_is_a_space_and_a_hard_break_is_a_line() {
        assert_eq!(
            doc("one\ntwo"),
            vec![Block::Paragraph(vec![
                Span::plain("one"),
                Span::plain(" "),
                Span::plain("two"),
            ])]
        );
        let hard = doc("one  \ntwo");
        assert_eq!(
            text(match &hard[0] {
                Block::Paragraph(spans) => spans,
                other => panic!("{other:?}"),
            }),
            "one\ntwo"
        );
    }

    /// Markup becomes emphasis on a span, never characters. The wrapper works
    /// on characters, so a line stays free to break inside a bold run.
    #[test]
    fn inline_markup_becomes_emphasis_not_asterisks() {
        let blocks = doc("use **bold** and `code` and *soft*");
        let Block::Paragraph(spans) = &blocks[0] else {
            panic!("{blocks:?}");
        };
        assert_eq!(text(spans), "use bold and code and soft");
        let found: Vec<Emphasis> = spans
            .iter()
            .filter(|s| s.emphasis != Emphasis::None)
            .map(|s| s.emphasis)
            .collect();
        assert_eq!(
            found,
            vec![Emphasis::Strong, Emphasis::Code, Emphasis::Emph]
        );
    }

    /// The rule `Plain` existed to protect: a fence is content, and it comes
    /// out as lines nobody is allowed to reflow.
    #[test]
    fn a_fence_is_verbatim_and_survives_being_left_open() {
        assert_eq!(
            doc("here:\n\n```rust\nfn main() {}\n\n// blank line inside\n```"),
            vec![
                Block::Paragraph(vec![Span::plain("here:")]),
                Block::Code {
                    lang: Some("rust".into()),
                    lines: vec![
                        "fn main() {}".into(),
                        String::new(),
                        "// blank line inside".into(),
                    ],
                },
            ]
        );

        // A truncated or interrupted reply is the common case, not the odd one.
        let open = doc("output:\n\n```\ncargo build\n");
        assert!(
            matches!(&open[1], Block::Code { lines, .. } if lines == &["cargo build"]),
            "{open:?}"
        );
    }

    #[test]
    fn lists_keep_their_marker_and_their_depth() {
        let blocks = doc("1. first\n2. second\n   - nested\n");
        let markers: Vec<(u8, &str, String)> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::Bullet {
                    depth,
                    marker,
                    spans,
                } => Some((*depth, marker.as_str(), text(spans))),
                _ => None,
            })
            .collect();
        assert_eq!(
            markers,
            vec![
                (0, "1.", "first".to_string()),
                (0, "2.", "second".to_string()),
                (1, "•", "nested".to_string()),
            ]
        );
    }

    /// Replies are full of tables, and a table shown as its pipes is the
    /// ugliest thing in the reader.
    #[test]
    fn a_table_comes_out_as_rows() {
        let blocks =
            doc("| harness | sessions |\n| --- | --- |\n| claude | 812 |\n| codex | 162 |");
        assert_eq!(
            blocks,
            vec![Block::Table {
                head: vec!["harness".into(), "sessions".into()],
                rows: vec![
                    vec!["claude".into(), "812".into()],
                    vec!["codex".into(), "162".into()],
                ],
            }]
        );
    }

    /// A link's address is worth showing once. A bare URL already is its own
    /// text and does not get to say it twice.
    #[test]
    fn a_link_keeps_its_address_exactly_once() {
        let named = doc("see [the docs](https://docs.rs/whence)");
        let Block::Paragraph(spans) = &named[0] else {
            panic!("{named:?}");
        };
        assert_eq!(text(spans), "see the docs (https://docs.rs/whence)");

        let bare = doc("see <https://docs.rs/whence>");
        let Block::Paragraph(spans) = &bare[0] else {
            panic!("{bare:?}");
        };
        assert_eq!(text(spans), "see https://docs.rs/whence");
    }

    /// Prose that is not markdown must survive being read as markdown — most
    /// of this corpus is Chinese, where a `*` is rare and a line break is not.
    #[test]
    fn ordinary_prose_is_unharmed() {
        let blocks = doc("重构索引以后再看看这个模块的实现细节\n和边界");
        assert_eq!(
            blocks,
            vec![Block::Paragraph(vec![
                Span::plain("重构索引以后再看看这个模块的实现细节"),
                Span::plain(" "),
                Span::plain("和边界"),
            ])]
        );
    }
}
