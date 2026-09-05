//! The index and the searcher, over a corpus small enough to reason about.
//!
//! These transcripts are bilingual on purpose: what has to hold is that a
//! document and a query are cut the same way whichever script either of them is
//! written in, because a term the index and the query spell differently is a
//! term that can never be found.

use tantivy::tokenizer::TokenStream;
use whence::index::SearchIndex;
use whence::model::Harness;
use whence::search::{Fuzzy, Query};
use whence::source::{Root, Transcript};

const SESSION: &str = "bbbbbbbb-2222-4222-8222-222222222222";

/// One indexed transcript, one prompt per line given, kept alive by the
/// returned directory.
fn corpus(prompts: &[&str]) -> (tempfile::TempDir, SearchIndex) {
    let dir = tempfile::tempdir().expect("tempdir");
    let project = dir.path().join("projects").join("-code-demo");
    std::fs::create_dir_all(&project).expect("mkdir");

    let lines: Vec<String> = prompts
        .iter()
        .enumerate()
        .map(|(i, prompt)| {
            format!(
                r#"{{"type":"user","sessionId":"{SESSION}","cwd":"/code/demo",
                   "timestamp":"2026-08-08T05:{:02}:00Z","message":{{"content":{}}}}}"#,
                i,
                serde_json::to_string(prompt).expect("json"),
            )
            .replace('\n', "")
        })
        .collect();
    let file = project.join(format!("{SESSION}.jsonl"));
    std::fs::write(&file, lines.join("\n")).expect("write");

    let index = SearchIndex::open_or_create(&dir.path().join("index")).expect("index");
    index
        .build(
            &[Transcript {
                harness: Harness::Claude,
                path: file,
            }],
            &[Root {
                harness: Harness::Claude,
                path: dir.path().to_path_buf(),
            }],
            false,
        )
        .expect("build");
    (dir, index)
}

fn exact(text: &str) -> Query {
    Query {
        text: text.to_string(),
        limit: 20,
        fuzzy: Fuzzy::Never,
        ..Query::default()
    }
}

fn cut(text: &str) -> Vec<String> {
    let mut analyzer = whence::tokenize::text();
    let mut stream = analyzer.token_stream(text);
    let mut out = Vec::new();
    while stream.advance() {
        out.push(stream.token().text.clone());
    }
    out
}

/// Chinese still goes through jieba, because there is no other way to find the
/// word boundaries: a whitespace tokenizer turns the whole sentence into one
/// token nobody will ever type.
#[test]
fn chinese_is_still_cut_into_words() {
    assert_eq!(cut("重构索引性能"), ["重构", "索引", "性能"]);
}

/// CJK punctuation travels into jieba with the text it punctuates, and is
/// dropped on the way out with everything else that is not a word.
#[test]
fn chinese_punctuation_is_not_a_term() {
    assert_eq!(cut("重构，索引。"), ["重构", "索引"]);
}

/// The seam between the two halves of the analyzer. `tantivy` is embedded in a
/// Chinese sentence, so it was indexed by the CJK path's neighbour — and the
/// query, which has no Chinese in it and never loads a dictionary, still has to
/// find it.
#[test]
fn a_latin_query_finds_a_word_inside_a_chinese_sentence() {
    let (_dir, index) = corpus(&["用 tantivy 重建索引"]);

    let hits = index.search(&exact("tantivy")).expect("search").hits;
    assert_eq!(hits.len(), 1);

    let hits = index.search(&exact("索引")).expect("search").hits;
    assert_eq!(hits.len(), 1, "and the Chinese half still matches too");
}

/// The whitespace that happened to separate two words is not part of the
/// phrase. It used to be a term of its own, so this transcript — which wrapped
/// the phrase across a line — was not a match.
#[test]
fn a_quoted_phrase_survives_a_line_break() {
    let (_dir, index) = corpus(&["how can i use the borrow\nchecker of polonius"]);

    let results = index.search(&exact("\"borrow checker\"")).expect("search");
    assert_eq!(results.hits.len(), 1);
    assert!(!results.relaxed, "matched exactly, not by relaxing");
}

/// An excerpt highlights the words you searched for. When `" "` was a term,
/// every space between them was highlighted too.
#[test]
fn an_excerpt_highlights_words_and_not_the_spaces_between_them() {
    let (_dir, index) = corpus(&["how can i use the borrow checker of polonius"]);

    let hits = index.search(&exact("borrow checker")).expect("search").hits;
    let excerpt = &hits.first().expect("a hit").excerpt;
    let marked: Vec<&str> = excerpt
        .highlights
        .iter()
        .map(|range| &excerpt.text[range.clone()])
        .collect();

    assert_eq!(marked, ["borrow", "checker"]);
}

/// An index written by an older analyzer cannot be queried with this one — the
/// terms in it are not the terms a query would now produce. It is a derived
/// artifact, so it is thrown away and rebuilt rather than reported as an error.
#[test]
fn an_index_from_an_older_format_is_rebuilt() {
    let (dir, _index) = corpus(&["用 tantivy 重建索引"]);
    let path = dir.path().join("index");
    std::fs::write(path.join("whence-format"), "1").expect("write");

    let reopened = SearchIndex::open_or_create(&path).expect("reopen");
    assert!(
        reopened
            .search(&exact("tantivy"))
            .expect("search")
            .hits
            .is_empty(),
        "the stale index was cleared, not queried"
    );
    assert!(
        SearchIndex::open(&path).is_ok(),
        "and what replaced it is this format"
    );
}
