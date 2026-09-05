//! What the analyzer does to text that has no Chinese in it.
//!
//! **Nothing in this file may tokenize CJK.** The point of the first test is
//! that jieba's dictionary was never loaded, and the dictionary is a process-
//! wide lazy static: one Chinese string tokenized anywhere in this binary, in
//! any test, and the guard measures a warm dictionary and passes for the wrong
//! reason. Chinese belongs in `tests/index.rs`, which is a separate process.

use std::time::Instant;
use tantivy::tokenizer::TokenStream;

fn cut(text: &str) -> Vec<String> {
    let mut analyzer = whence::tokenize::text();
    let mut stream = analyzer.token_stream(text);
    let mut out = Vec::new();
    while stream.advance() {
        out.push(stream.token().text.clone());
    }
    out
}

fn positions(text: &str) -> Vec<(String, usize)> {
    let mut analyzer = whence::tokenize::text();
    let mut stream = analyzer.token_stream(text);
    let mut out = Vec::new();
    while stream.advance() {
        let token = stream.token();
        out.push((token.text.clone(), token.position));
    }
    out
}

/// The reason [`whence::tokenize`] exists. jieba's dictionary is a 5 MB blob
/// inflated and turned into a trie on first use — ~100 ms, once per process,
/// and tokenizing the query is what used to trigger it. A query with no Chinese
/// in it must never ask.
///
/// The margin here is wide on purpose: the work being measured is microseconds
/// and the load it must not do is ~100 ms even on a fast machine, so a slow CI
/// box moves both numbers without bringing them anywhere near each other.
#[test]
fn latin_never_pays_for_the_chinese_dictionary() {
    let text = "the searcher folds one index across every harness, so a hit \
                says which agent it came from and --harness narrows it";
    let started = Instant::now();
    let tokens = cut(text);
    let elapsed = started.elapsed();

    assert!(tokens.contains(&"harness".to_string()));
    assert!(
        elapsed.as_millis() < 30,
        "cutting Latin script took {elapsed:?} — that is a dictionary being loaded"
    );
}

/// 58.7% of what jieba emitted over a sample of the real corpus was whitespace
/// or punctuation. None of it is a word anyone searches for, and `" "` as a
/// term meant every multi-word query silently intersected a posting list that
/// held every document.
#[test]
fn punctuation_is_not_a_term() {
    let tokens = cut("run `cargo clippy --all-targets`, then src/model.rs");
    assert_eq!(
        tokens,
        ["run", "cargo", "clippy", "all", "targets", "then", "src", "model", "rs"]
    );
}

/// Positions are character offsets into the text, which is what jieba's search
/// mode needs — a compound word and the pieces it decomposes into overlap, and
/// only a real offset can say so. Dropping a punctuation token therefore leaves
/// a gap rather than shifting everything after it.
#[test]
fn positions_are_character_offsets() {
    assert_eq!(
        positions("src/model.rs"),
        [
            ("src".to_string(), 0),
            ("model".to_string(), 4),
            ("rs".to_string(), 10)
        ]
    );
}

/// The whitespace between two words is not part of the phrase you remember. It
/// used to be: `"borrow checker"` matched a transcript that wrote it with one
/// space and missed the one that wrapped it across a line.
#[test]
fn a_phrase_does_not_depend_on_which_whitespace_separated_it() {
    assert_eq!(positions("borrow checker"), positions("borrow\nchecker"));
}

/// Identifiers are cut the way paths are, so `SearchIndex::open_or_create`
/// finds the same documents whether you remember the `::` or not.
#[test]
fn identifiers_split_on_their_punctuation() {
    assert_eq!(
        cut("SearchIndex::open_or_create"),
        ["searchindex", "open", "or", "create"]
    );
}
