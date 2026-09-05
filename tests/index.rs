//! The index and the searcher, over a corpus small enough to reason about.
//!
//! These transcripts are bilingual on purpose: what has to hold is that a
//! document and a query are cut the same way whichever script either of them is
//! written in, because a term the index and the query spell differently is a
//! term that can never be found.

use tantivy::tokenizer::TokenStream;
use whence::index::SearchIndex;
use whence::model::Harness;
use whence::search::{Found, Fuzzy, Query};
use whence::source::{Root, Transcript};

const SESSION: &str = "bbbbbbbb-2222-4222-8222-222222222222";

/// One indexed transcript, one prompt per line given, kept alive by the
/// returned directory.
fn corpus(prompts: &[&str]) -> (tempfile::TempDir, SearchIndex) {
    titled(None, prompts)
}

/// The same, with the auto-generated conversation title a harness would have
/// written — which is searched alongside the body, and boosted.
fn titled(title: Option<&str>, prompts: &[&str]) -> (tempfile::TempDir, SearchIndex) {
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
    let mut lines = lines;
    if let Some(title) = title {
        lines.push(format!(
            r#"{{"type":"ai-title","sessionId":"{SESSION}","aiTitle":{}}}"#,
            serde_json::to_string(title).expect("json")
        ));
    }
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

/// The first half of "why did this come back": what is actually being searched
/// for. Your input is not the query — the analyzer cuts a path into three words
/// and every one of them has to be present.
#[test]
fn the_query_is_reported_as_the_words_it_was_cut_into() {
    let (_dir, index) = corpus(&["look at src/model.rs again"]);

    let results = index.search(&exact("src/model.rs")).expect("search");
    assert_eq!(results.terms, ["src", "model", "rs"]);
    assert_eq!(results.hits.len(), 1);
}

/// A relaxed pass is the case that most needs explaining: nothing you typed is
/// in the text, so an unhighlighted excerpt leaves you guessing. The hit has to
/// name the word it actually found.
#[test]
fn a_relaxed_hit_names_the_word_it_actually_found() {
    let (_dir, index) = corpus(&["normalize the transcript first"]);

    let results = index
        .search(&Query {
            text: "normlize".to_string(),
            limit: 20,
            ..Query::default()
        })
        .expect("search");
    assert!(results.relaxed, "the typo matches nothing exactly");

    let hit = results.hits.first().expect("a hit");
    assert_eq!(hit.found_in, Found::Body);
    assert_eq!(hit.matched.len(), 1);
    assert_eq!(hit.matched[0].token, "normlize");
    assert_eq!(hit.matched[0].word, "normalize");
    assert_eq!(hit.why().as_deref(), Some("matched normlize → normalize"));

    // And the excerpt points at it, rather than falling back to the opening
    // words with nothing marked at all.
    let marked: Vec<&str> = hit
        .excerpt
        .highlights
        .iter()
        .map(|range| &hit.excerpt.text[range.clone()])
        .collect();
    assert_eq!(marked, ["normalize"]);
    assert_eq!(
        whence::search::relaxation(&results.hits).as_deref(),
        Some("normlize → normalize")
    );
}

/// The other way an excerpt can look unrelated: the match was in the title,
/// which is searched and boosted but is not the text underneath.
#[test]
fn a_match_in_the_title_alone_says_so() {
    let (_dir, index) = titled(
        Some("Polonius borrow checker"),
        &["how do i fix this lifetime error"],
    );

    let hit = index
        .search(&exact("polonius"))
        .expect("search")
        .hits
        .into_iter()
        .next()
        .expect("a hit");

    assert_eq!(hit.found_in, Found::Title);
    assert_eq!(
        hit.why().as_deref(),
        Some("matched in the title: Polonius"),
        "an excerpt with nothing marked in it has to explain itself"
    );
}

/// `src/model.rs` cuts to three words, one of which is `rs` — and `rs` is
/// inside `first` and `parse` too. A mark that lands there is noise, and in the
/// reader, where the whole conversation is marked, it is a lot of noise.
#[test]
fn a_latin_word_is_marked_only_where_it_is_a_word() {
    let (_dir, index) = corpus(&["the first parse of src/model.rs"]);

    let hit = index
        .search(&exact("src/model.rs"))
        .expect("search")
        .hits
        .into_iter()
        .next()
        .expect("a hit");
    let marked: Vec<&str> = hit
        .excerpt
        .highlights
        .iter()
        .map(|range| &hit.excerpt.text[range.clone()])
        .collect();

    assert_eq!(marked, ["src", "model", "rs"]);
}
