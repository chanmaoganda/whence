//! The analyzer every document and every query goes through.
//!
//! Transcripts are bilingual — Chinese prose with paths, identifiers and shell
//! commands embedded in it — and the two halves want different treatment.
//! Chinese has no spaces, so it needs a dictionary; `src/model.rs` does not,
//! and asking a dictionary about it is expensive:
//!
//! **jieba's dictionary costs ~100 ms to load, once per process.** It is a 5 MB
//! deflate blob inflated on first use and built into a trie, and
//! `tantivy-jieba` hangs it off a lazy static. Tokenizing the *query* is what
//! triggers it, so `whence search tantivy` — a query with no Chinese anywhere
//! in it — used to spend 100 ms of its 110 ms building a Chinese dictionary it
//! then made no use of.
//!
//! So [`Script`] splits text into runs of CJK and runs of everything else, and
//! only the CJK runs reach jieba. The split is a property of the text, not of
//! the caller, so a document and a query are cut the same way by construction —
//! which is the only thing that matters here, because a term that the index and
//! the query spell differently is a term that can never be found. A query with
//! no CJK in it never touches the dictionary at all: 110 ms becomes 10 ms.
//!
//! **Punctuation is not a term.** Measured over a 60-transcript sample, 58.7% of
//! everything jieba emitted was whitespace or punctuation — 162,549 of 276,826
//! tokens, from just 113 distinct strings, so a handful of posting lists carried
//! most of the index. Dropping them takes 29% off the searchable index (44% off
//! the positions file alone), and it fixes two things that were never about
//! size: `whence search 'borrow checker'` used to highlight every space in the
//! excerpt, because `" "` really was one of the terms it matched on; and a
//! quoted phrase used to demand the exact separator that was recorded, so
//! `"borrow checker"` missed a transcript that wrote it across a line break.
//!
//! Positions stay where jieba puts them — the character index of the token
//! within the whole text, with `position_length` spanning the characters it
//! covers. That is what makes jieba's search mode work, where a long word and
//! the pieces it decomposes into legitimately overlap, and dropping a
//! punctuation token then leaves a gap rather than shifting its neighbours.

use tantivy::tokenizer::{
    LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer, Token, TokenStream, Tokenizer,
};

/// jieba for the Chinese, simple splitting for everything else. Named in
/// `meta.json`, so it must be re-registered on open.
pub const TOK_TEXT: &str = "text";
/// Splitting on non-alphanumerics, for paths, projects and tool names.
pub const TOK_PATH: &str = "path";

/// The analyzer text fields are indexed and queried with.
pub fn text() -> TextAnalyzer {
    TextAnalyzer::builder(Script::default())
        .filter(RemoveLongFilter::limit(64))
        .filter(LowerCaser)
        .build()
}

/// The analyzer for path-shaped fields: projects, edited files, tool names.
/// Splitting on non-alphanumerics is what lets `src/normalize.rs` and the
/// absolute path it was recorded under match the same documents.
pub fn path() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(64))
        .filter(LowerCaser)
        .build()
}

/// CJK, roughly: everything from the CJK radicals block upward. This decides
/// which half of [`Script`] a character belongs to, and — in
/// [`crate::search`] — which flavour of fuzziness a token gets.
///
/// Deliberately wide. The CJK punctuation block (`，`, `。`) falls inside it and
/// so travels with the text it punctuates into jieba, which is where it is
/// dropped; full-width Latin and the ASCII punctuation fall outside it and are
/// dropped by the other branch. Either way it does not become a term.
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x2E80..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x3FFFF)
}

/// The text analyzer: CJK runs are cut by jieba, everything else on
/// non-alphanumerics.
#[derive(Clone, Default)]
pub struct Script {
    jieba: tantivy_jieba::JiebaTokenizer,
}

impl Script {
    /// Cut one run of CJK with jieba, rebasing its offsets and positions onto
    /// the whole text.
    fn cut(&mut self, run: &str, byte: usize, chars: usize, out: &mut Vec<Token>) {
        let mut stream = self.jieba.token_stream(run);
        while stream.advance() {
            let token = stream.token();
            if !meaningful(&token.text) {
                continue;
            }
            out.push(Token {
                offset_from: byte + token.offset_from,
                offset_to: byte + token.offset_to,
                position: chars + token.position,
                position_length: token.position_length,
                text: token.text.clone(),
            });
        }
    }
}

/// Runs of alphanumerics, which is what the path tokenizer does to a path and
/// close enough to what jieba did to Latin script that a rebuilt index finds
/// the same documents. `x86_64` and `c#` are the visible difference: they were
/// one token each and are now `x86`/`64` and `c`.
fn split(run: &str, byte: usize, chars: usize, out: &mut Vec<Token>) {
    let mut start: Option<(usize, usize)> = None;
    let ends = run
        .char_indices()
        .enumerate()
        .map(|(i, (offset, c))| (i, offset, Some(c)))
        // One past the end, so a token running to the end of the run is closed.
        .chain(std::iter::once((run.chars().count(), run.len(), None)));
    for (index, offset, ch) in ends {
        match ch {
            Some(c) if c.is_alphanumeric() => {
                start.get_or_insert((index, offset));
            }
            _ => {
                if let Some((first, from)) = start.take() {
                    out.push(Token {
                        offset_from: byte + from,
                        offset_to: byte + offset,
                        position: chars + first,
                        position_length: index - first,
                        text: run[from..offset].to_string(),
                    });
                }
            }
        }
    }
}

/// A token worth indexing has something in it you could have searched for.
/// Whitespace and punctuation do not.
fn meaningful(text: &str) -> bool {
    text.chars().any(char::is_alphanumeric)
}

impl Tokenizer for Script {
    type TokenStream<'a> = Tokens;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Tokens {
        let mut tokens = Vec::new();
        let (mut byte, mut chars) = (0, 0);
        while byte < text.len() {
            let rest = &text[byte..];
            let cjk = is_cjk(
                rest.chars()
                    .next()
                    .expect("a non-empty remainder has a char"),
            );
            let run: &str = {
                let len = rest
                    .char_indices()
                    .take_while(|(_, c)| is_cjk(*c) == cjk)
                    .map(|(offset, c)| offset + c.len_utf8())
                    .last()
                    .expect("a non-empty remainder has at least one character");
                &rest[..len]
            };
            if cjk {
                self.cut(run, byte, chars, &mut tokens);
            } else {
                split(run, byte, chars, &mut tokens);
            }
            byte += run.len();
            chars += run.chars().count();
        }
        Tokens { tokens, index: 0 }
    }
}

/// The cut text, held whole because both halves of [`Script`] produce their
/// tokens in one pass.
pub struct Tokens {
    tokens: Vec<Token>,
    index: usize,
}

impl TokenStream for Tokens {
    fn advance(&mut self) -> bool {
        if self.index >= self.tokens.len() {
            return false;
        }
        self.index += 1;
        true
    }

    fn token(&self) -> &Token {
        &self.tokens[self.index - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.index - 1]
    }
}
