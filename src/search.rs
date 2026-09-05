//! Querying the index.
//!
//! Two questions, one index. `whence search` is free-text over what you typed,
//! what the agent replied and what it was thinking; `whence file` asks which
//! sessions changed a path and why — ranked by recency, because for a file
//! history "when" beats "how well it matched".
//!
//! Both questions span every harness at once. That is the point of a single
//! index: you rarely remember which agent you were using when you saw the thing
//! you are now looking for.

use crate::index::{stored_text, SearchIndex, KIND_EDIT, KIND_PROMPT, KIND_REPLY, KIND_THINK};
use crate::model::Harness;
use crate::tokenize::{is_cjk, TOK_PATH, TOK_TEXT};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use std::ops::Range;
use tantivy::collector::TopDocs;
use tantivy::query::{
    AllQuery, BooleanQuery, FuzzyTermQuery, Occur, PhraseQuery, Query as TantivyQuery, QueryParser,
    RangeQuery, RegexQuery, TermQuery,
};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{DateTime as TantivyDate, DocAddress, Order, Score, TantivyDocument, Term};

/// When to fall back from exact matching to fuzzy matching.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Fuzzy {
    /// Exact first; relax only when it found nothing. Keeps precise results
    /// precise and only bends when you would otherwise get an empty screen.
    #[default]
    Auto,
    Always,
    Never,
}

/// A free-text query plus the filters that narrow it.
#[derive(Debug, Default, Clone)]
pub struct Query {
    pub text: String,
    /// Restrict to these harnesses. Empty means all of them.
    pub harness: Vec<Harness>,
    /// Matched against path segments, so `rtrade` finds `/code/stock/rtrade`.
    pub project: Option<String>,
    pub kind: Option<String>,
    pub tool: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub limit: usize,
    pub fuzzy: Fuzzy,
}

/// Hits plus whether they came from a relaxed pass, so the caller can say so.
#[derive(Debug, Clone, Default)]
pub struct Results {
    pub hits: Vec<Hit>,
    pub relaxed: bool,
    /// The query as the analyzer cut it — the words actually looked for, which
    /// is not always the words you typed: `src/model.rs` is three of them, and
    /// a Chinese phrase is however many jieba decided.
    pub terms: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Hit {
    /// Which agent produced this. Always set: it is stamped on every document.
    pub harness: String,
    pub kind: String,
    pub session: String,
    pub project: String,
    pub source: String,
    pub title: String,
    pub turn: u64,
    pub timestamp: Option<DateTime<Utc>>,
    /// Only set on `edit` hits.
    pub file: Option<String>,
    pub score: Score,
    pub excerpt: Excerpt,
    /// Each query word that this hit answered, paired with the word actually
    /// found. Empty when nothing in the text matched — see [`Hit::found_in`].
    pub matched: Vec<Matched>,
    /// Which part of the hit the words were found in.
    pub found_in: Found,
}

/// One word of the query and the word in the hit that answered it.
///
/// The two differ exactly when the query was relaxed: you typed `normlize` and
/// the transcript says `normalize`, and being told so is the difference between
/// a result you can trust and one you have to squint at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched {
    /// The query word, as the analyzer cut it.
    pub token: String,
    /// The word found in the hit, in the spelling the transcript used.
    pub word: String,
}

/// Where the words were found. Only [`Found::Body`] is visible in the excerpt,
/// so the other two have to be said out loud.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Found {
    /// In the excerpt below, where the highlights point at them.
    #[default]
    Body,
    /// Only in the conversation title, which is why the excerpt reads as though
    /// it has nothing to do with the query.
    Title,
    /// Nowhere in the text: this came back on the filters alone, or on a field
    /// the excerpt never shows.
    Elsewhere,
}

impl Hit {
    /// Why this hit came back, when the excerpt does not already show it. `None`
    /// means the highlighted words in the excerpt are the whole story.
    pub fn why(&self) -> Option<String> {
        let words = || -> Vec<&str> { self.matched.iter().map(|m| m.word.as_str()).collect() };
        match self.found_in {
            Found::Body => {
                let relaxed: Vec<String> = self
                    .matched
                    .iter()
                    .filter(|m| !m.token.eq_ignore_ascii_case(&m.word))
                    .map(|m| format!("{} → {}", m.token, m.word))
                    .collect();
                (!relaxed.is_empty()).then(|| format!("matched {}", relaxed.join(", ")))
            }
            Found::Title => Some(format!("matched in the title: {}", words().join(", "))),
            Found::Elsewhere => None,
        }
    }
}

/// A passage of matching text, with the byte ranges worth highlighting. Keeping
/// ranges rather than pre-rendered markup lets the caller decide how to show it.
#[derive(Debug, Clone, Default)]
pub struct Excerpt {
    pub text: String,
    pub highlights: Vec<Range<usize>>,
}

impl Excerpt {
    /// Wrap each highlighted range, e.g. in ANSI bold.
    pub fn render(&self, open: &str, close: &str) -> String {
        let mut out = String::with_capacity(self.text.len());
        let mut at = 0;
        for range in &self.highlights {
            if range.start < at || range.end > self.text.len() {
                continue;
            }
            out.push_str(&self.text[at..range.start]);
            out.push_str(open);
            out.push_str(&self.text[range.clone()]);
            out.push_str(close);
            at = range.end;
        }
        out.push_str(&self.text[at..]);
        out
    }
}

impl SearchIndex {
    /// Free-text search, ranked by relevance.
    pub fn search(&self, query: &Query) -> Result<Results> {
        let searchable = !query.text.trim().is_empty();
        let terms = self.text_tokens(&query.text)?;
        let cut = &terms;
        let results = |hits, relaxed| Results {
            hits,
            relaxed,
            terms: terms.clone(),
        };
        match query.fuzzy {
            Fuzzy::Never => Ok(results(self.run(query, false, cut)?, false)),
            Fuzzy::Always if searchable => Ok(results(self.run(query, true, cut)?, true)),
            Fuzzy::Always => Ok(results(self.run(query, false, cut)?, false)),
            Fuzzy::Auto => {
                let exact = self.run(query, false, cut)?;
                if !exact.is_empty() || !searchable {
                    return Ok(results(exact, false));
                }
                // Nothing matched exactly, so the alternative to relaxing is an
                // empty screen. Only then is the extra recall worth the noise.
                let hits = self.run(query, true, cut)?;
                let relaxed = !hits.is_empty();
                Ok(results(hits, relaxed))
            }
        }
    }

    /// One pass over the index. `tokens` is the query as the analyzer cut it,
    /// passed in because the two passes of [`Fuzzy::Auto`] share it — and
    /// cutting a Chinese query is what loads jieba's dictionary.
    fn run(&self, query: &Query, fuzzy: bool, tokens: &[String]) -> Result<Vec<Hit>> {
        let f = self.fields;
        let reader = self.index.reader()?;
        let searcher = reader.searcher();

        let text: Box<dyn TantivyQuery> = if query.text.trim().is_empty() {
            Box::new(AllQuery)
        } else if fuzzy {
            self.fuzzy_query(&query.text, tokens)?
        } else {
            let mut parser = QueryParser::for_index(&self.index, vec![f.body, f.title]);
            // A hit in the conversation title is a strong signal that the whole
            // session is about the topic, but the body is what you searched.
            parser.set_field_boost(f.title, 2.0);
            parser.set_conjunction_by_default();
            parser
                .parse_query(&query.text)
                .with_context(|| format!("could not parse query {:?}", query.text))?
        };

        let mut clauses: Vec<(Occur, Box<dyn TantivyQuery>)> = vec![(Occur::Must, text)];
        if let Some(kind) = &query.kind {
            clauses.push((Occur::Must, exact(self.fields.kind, kind)));
        }
        // Several harnesses are an OR nested inside the outer AND: any of the
        // named ones, but still subject to every other filter.
        if !query.harness.is_empty() {
            let any: Vec<(Occur, Box<dyn TantivyQuery>)> = query
                .harness
                .iter()
                .map(|h| (Occur::Should, exact(self.fields.harness, h.as_str())))
                .collect();
            clauses.push((Occur::Must, Box::new(BooleanQuery::new(any))));
        }
        if let Some(project) = &query.project {
            clauses.push((Occur::Must, self.path_query(self.fields.project, project)?));
        }
        if let Some(tool) = &query.tool {
            clauses.push((Occur::Must, self.path_query(self.fields.tools, tool)?));
        }
        if let Some(since) = query.since {
            clauses.push((Occur::Must, since_query(f.ts, since)));
        }

        let query_all = BooleanQuery::new(clauses);
        let limit = query.limit.max(1);
        // With no words to rank on, every document scores the same and "the top
        // by score" is whatever order the segments happened to be in. Recency is
        // then the only ordering that means anything — and it turns a bare
        // `--project x --since 7d`, or the TUI's opening screen, into "what was
        // I doing", which is the question you were asking.
        let top: Vec<(Score, DocAddress)> = if query.text.trim().is_empty() {
            let collector =
                TopDocs::with_limit(limit).order_by_fast_field::<TantivyDate>("ts", Order::Desc);
            let recent: Vec<(Option<TantivyDate>, DocAddress)> =
                searcher.search(&query_all, &collector)?;
            recent.into_iter().map(|(_, addr)| (0.0, addr)).collect()
        } else {
            searcher.search(&query_all, &TopDocs::with_limit(limit).order_by_score())?
        };

        // Excerpts are found by looking for the query's own tokens in the
        // stored body. tantivy's `SnippetGenerator` would pick a better window,
        // but it re-tokenizes every hit it is shown: at the TUI's limit of 200
        // that was 47 ms of jieba per keystroke on a Chinese query against 4 ms
        // here, and it could not highlight a regex or fuzzy match at all,
        // because those report no terms.
        top.into_iter()
            .map(|(score, addr)| {
                let doc: TantivyDocument = searcher.doc(addr)?;
                let body = stored_text(&doc, f.body).unwrap_or_default();
                let (excerpt, matched) = locate(&body, tokens, fuzzy);
                // A hit whose words are nowhere in the body matched on the
                // title, which is boosted and searched alongside it. Saying so
                // is the difference between an excerpt that looks wrong and one
                // that is merely not where the match was.
                let (matched, found_in) = if !matched.is_empty() {
                    (matched, Found::Body)
                } else {
                    let title = stored_text(&doc, f.title).unwrap_or_default();
                    match locate(&title, tokens, fuzzy).1 {
                        found if !found.is_empty() => (found, Found::Title),
                        _ => (Vec::new(), Found::Elsewhere),
                    }
                };
                Ok(self.hit(&doc, score, excerpt, matched, found_in))
            })
            .collect()
    }

    /// Which sessions changed this path, most recent first. The path is matched
    /// as a sequence of segments, so `normalize.rs`, `src/normalize.rs` and the
    /// full absolute path all find the same edits.
    pub fn file_history(&self, path: &str, limit: usize) -> Result<Vec<Hit>> {
        let f = self.fields;
        let reader = self.index.reader()?;
        let searcher = reader.searcher();

        let query = BooleanQuery::new(vec![
            (Occur::Must, exact(f.kind, KIND_EDIT)),
            (Occur::Must, self.path_query(f.file, path)?),
        ]);

        let collector =
            TopDocs::with_limit(limit.max(1)).order_by_fast_field::<TantivyDate>("ts", Order::Desc);
        let top: Vec<(Option<TantivyDate>, DocAddress)> = searcher.search(&query, &collector)?;

        top.into_iter()
            .map(|(_, addr)| {
                let doc: TantivyDocument = searcher.doc(addr)?;
                let excerpt = Excerpt {
                    text: opening(&stored_text(&doc, f.body).unwrap_or_default(), EXCERPT),
                    highlights: Vec::new(),
                };
                // What matched is the path, and the path is printed above the
                // excerpt — there is nothing to explain.
                Ok(self.hit(&doc, 0.0, excerpt, Vec::new(), Found::Elsewhere))
            })
            .collect()
    }

    fn hit(
        &self,
        doc: &TantivyDocument,
        score: Score,
        excerpt: Excerpt,
        matched: Vec<Matched>,
        found_in: Found,
    ) -> Hit {
        let f = self.fields;
        Hit {
            harness: stored_text(doc, f.harness).unwrap_or_default(),
            kind: stored_text(doc, f.kind).unwrap_or_default(),
            session: stored_text(doc, f.session).unwrap_or_default(),
            project: stored_text(doc, f.project).unwrap_or_default(),
            source: stored_text(doc, f.source).unwrap_or_default(),
            title: stored_text(doc, f.title).unwrap_or_default(),
            turn: doc.get_first(f.turn).and_then(|v| v.as_u64()).unwrap_or(0),
            timestamp: doc
                .get_first(f.ts)
                .and_then(|v| v.as_datetime())
                .and_then(|d| Utc.timestamp_opt(d.into_timestamp_secs(), 0).single()),
            file: stored_text(doc, f.file),
            score,
            excerpt,
            matched,
            found_in,
        }
    }

    /// The relaxed pass. Each query token is matched by whichever kind of
    /// fuzziness suits the script it is written in — measured on this corpus,
    /// the two are not interchangeable:
    ///
    /// | token | exact | substring | edit distance 1 |
    /// | --- | --- | --- | --- |
    /// | `一化` | 0 | 31 | 2,423 |
    /// | `索` | 0 | 250 | 11,917 |
    /// | `tantivvy` | 0 | 0 | 5 |
    ///
    /// A two-character Chinese word has ~110 neighbours at edit distance 1
    /// (`重构` reaches `架构`, `结构`, `机构`), so edit distance there is a
    /// noise generator. What Chinese actually needs is substring matching,
    /// because jieba cuts on word boundaries and you rarely remember them. The
    /// reverse holds for Latin script: a typo has no substring in common, and
    /// `tantivy` has *zero* neighbours at distance 1, so the correction is
    /// unambiguous.
    fn fuzzy_query(&self, text: &str, tokens: &[String]) -> Result<Box<dyn TantivyQuery>> {
        let mut clauses: Vec<(Occur, Box<dyn TantivyQuery>)> = Vec::new();
        for token in tokens {
            clauses.push((Occur::Must, self.token_query(token)?));
        }
        if clauses.is_empty() {
            bail!("{text:?} has nothing to match on");
        }
        Ok(Box::new(BooleanQuery::new(clauses)))
    }

    /// The query cut the way the index was cut, deduplicated: jieba's search
    /// mode emits overlapping tokens, and requiring one twice only narrows the
    /// result for no gain.
    ///
    /// These are both what a relaxed pass matches on and what an excerpt
    /// highlights, which is the same list for the same reason: they are the
    /// words the index would have looked for.
    fn text_tokens(&self, text: &str) -> Result<Vec<String>> {
        let mut analyzer = self
            .index
            .tokenizers()
            .get(TOK_TEXT)
            .context("text tokenizer is not registered")?;
        let mut tokens: Vec<String> = Vec::new();
        let mut stream = analyzer.token_stream(text);
        while let Some(token) = stream.next() {
            if !tokens.iter().any(|t| t == &token.text) {
                tokens.push(token.text.clone());
            }
        }
        Ok(tokens)
    }

    fn token_query(&self, token: &str) -> Result<Box<dyn TantivyQuery>> {
        let field = self.fields.body;
        if token.chars().any(is_cjk) {
            let pattern = format!(".*{}.*", escape_regex(token));
            return Ok(Box::new(RegexQuery::from_pattern(&pattern, field)?));
        }
        // Below four characters a typo and a different word are the same edit,
        // so only extend the prefix rather than inventing corrections.
        let distance = u8::from(token.chars().count() >= 4);
        Ok(Box::new(FuzzyTermQuery::new_prefix(
            Term::from_field_text(field, token),
            distance,
            true,
        )))
    }

    /// Build a query over a path-tokenized field, running the input through the
    /// same analyzer the field was indexed with.
    fn path_query(
        &self,
        field: tantivy::schema::Field,
        input: &str,
    ) -> Result<Box<dyn TantivyQuery>> {
        let mut analyzer = self
            .index
            .tokenizers()
            .get(TOK_PATH)
            .context("path tokenizer is not registered")?;
        let mut terms: Vec<(usize, Term)> = Vec::new();
        let mut stream = analyzer.token_stream(input);
        while let Some(token) = stream.next() {
            terms.push((terms.len(), Term::from_field_text(field, &token.text)));
        }
        match terms.len() {
            0 => bail!("{input:?} has nothing to match on"),
            1 => Ok(Box::new(TermQuery::new(
                terms.pop().expect("checked len").1,
                IndexRecordOption::Basic,
            ))),
            // Consecutive segments: `src/normalize.rs` must not match
            // `normalize/other/src.rs`.
            _ => Ok(Box::new(PhraseQuery::new_with_offset(terms))),
        }
    }
}

/// The words a relaxed pass actually reached, as `typed → found` pairs.
///
/// `None` when nothing was reached by relaxing — then the honest thing to say
/// is only that the query was relaxed, because the words are the ones you typed.
pub fn relaxation(hits: &[Hit]) -> Option<String> {
    let mut pairs: Vec<String> = Vec::new();
    for matched in hits.iter().flat_map(|hit| &hit.matched) {
        if matched.token.eq_ignore_ascii_case(&matched.word) {
            continue;
        }
        let pair = format!("{} → {}", matched.token, matched.word);
        if !pairs.contains(&pair) {
            pairs.push(pair);
        }
    }
    (!pairs.is_empty()).then(|| pairs.join(", "))
}

fn exact(field: tantivy::schema::Field, value: &str) -> Box<dyn TantivyQuery> {
    Box::new(TermQuery::new(
        Term::from_field_text(field, value),
        IndexRecordOption::Basic,
    ))
}

fn since_query(ts: tantivy::schema::Field, since: DateTime<Utc>) -> Box<dyn TantivyQuery> {
    use std::ops::Bound;
    let lower = Term::from_field_date(ts, TantivyDate::from_timestamp_secs(since.timestamp()));
    Box::new(RangeQuery::new(Bound::Included(lower), Bound::Unbounded))
}

/// Characters of context to show around a match.
const EXCERPT: usize = 220;

/// A window of `body` around the first query word found in it, every occurrence
/// inside the window highlighted, and which words those were.
///
/// Every word of an exact query is in the body verbatim, because the analyzer
/// cut it out of text like this one. A relaxed query's word is not — `normlize`
/// never appears, `normalize` does — so `relaxed` says to look for whatever the
/// relaxed query would have accepted, and report the spelling actually found.
/// Without that, the one search that most needs explaining is the one that
/// comes back with nothing highlighted at all.
fn locate(body: &str, tokens: &[String], relaxed: bool) -> (Excerpt, Vec<Matched>) {
    let mut found: Vec<(usize, Matched)> = tokens
        .iter()
        .filter_map(|token| {
            let (at, word) = first_match(body, token, relaxed)?;
            Some((
                at,
                Matched {
                    token: token.clone(),
                    word,
                },
            ))
        })
        .collect();
    found.sort_by_key(|(at, _)| *at);

    let Some(&(at, _)) = found.first() else {
        return (
            Excerpt {
                text: opening(body, EXCERPT),
                highlights: Vec::new(),
            },
            Vec::new(),
        );
    };

    // Back off a little so the match is not flush against the left edge, and
    // keep both ends on character boundaries.
    let start = floor_char_boundary(body, at.saturating_sub(EXCERPT / 4));
    let end = ceil_char_boundary(body, (start + EXCERPT).min(body.len()));
    let window = body[start..end].replace('\n', " ");

    let mut highlights: Vec<Range<usize>> = Vec::new();
    for (_, matched) in &found {
        let mut from = 0;
        while let Some(hit) = find_word(&window, &matched.word, from) {
            highlights.push(hit..hit + matched.word.len());
            from = hit + matched.word.len().max(1);
        }
    }
    highlights.sort_by_key(|r| r.start);

    let mut matched: Vec<Matched> = Vec::new();
    for (_, one) in found {
        if !matched.contains(&one) {
            matched.push(one);
        }
    }
    (
        Excerpt {
            text: window,
            highlights,
        },
        matched,
    )
}

/// Where a query word occurs in `body`, and how it is spelled there.
///
/// Literal first, because that is what an exact query means. A relaxed query
/// then gets the same treatment its *query* got, since the two must agree about
/// what counts as a match: CJK is substring matching, which the literal pass
/// has already covered, and Latin script is a prefix within one edit — so
/// `tantiv` and `tantivvy` both point at the `tantivy` in the text.
fn first_match(body: &str, token: &str, relaxed: bool) -> Option<(usize, String)> {
    if let Some(at) = find_word(body, token, 0) {
        return Some((at, body[at..at + token.len()].to_string()));
    }
    if !relaxed || token.chars().any(is_cjk) {
        return None;
    }
    let allowed = usize::from(token.chars().count() >= 4);
    words(body).find_map(|(at, word)| {
        (prefix_distance(token, word) <= allowed).then(|| (at, word.to_string()))
    })
}

/// Runs of Latin-ish word characters, the way the analyzer cut them. CJK is
/// excluded because it is never what a Latin token is looking for, and letting
/// a whole Chinese sentence in as one "word" makes the edit distance below
/// expensive for no possible gain.
fn words(body: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut start: Option<usize> = None;
    let mut chars = body
        .char_indices()
        .chain(std::iter::once((body.len(), ' ')));
    std::iter::from_fn(move || {
        for (at, ch) in chars.by_ref() {
            if ch.is_alphanumeric() && !is_cjk(ch) {
                start.get_or_insert(at);
            } else if let Some(from) = start.take() {
                return Some((from, &body[from..at]));
            }
        }
        None
    })
}

/// How many edits turn `token` into some prefix of `word` — the distance a
/// prefix fuzzy query measures, so a longer word costs nothing extra.
///
/// Only the first `token.len() + 1` characters of `word` can matter, so the
/// table stays small however long the word is.
fn prefix_distance(token: &str, word: &str) -> usize {
    let a: Vec<char> = token.chars().flat_map(char::to_lowercase).collect();
    let b: Vec<char> = word
        .chars()
        .flat_map(char::to_lowercase)
        .take(a.len() + 1)
        .collect();
    if b.len() + 1 < a.len() {
        return usize::MAX;
    }
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut next = vec![i + 1; b.len() + 1];
        for (j, y) in b.iter().enumerate() {
            next[j + 1] = if x == y {
                row[j]
            } else {
                1 + row[j].min(row[j + 1]).min(next[j])
            };
        }
        row = next;
    }
    // The word is free to carry on past the token, so any column of the last
    // row is an answer.
    row.into_iter().min().unwrap_or(usize::MAX)
}

/// Where a query word occurs in `haystack`, as the analyzer would count it.
///
/// Latin script has to land on a word boundary: the query `src/model.rs` cuts
/// to `rs`, and marking the `rs` inside `first` and `parse` turns a page of a
/// conversation into noise. CJK has no boundaries to land on — jieba cuts finer
/// than you type — so there it stays a substring, which is the same rule the
/// relaxed pass matches CJK with.
pub(crate) fn find_word(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    if needle.chars().any(is_cjk) {
        return find_ci(haystack, needle, from);
    }
    let mut at = from;
    while let Some(found) = find_ci(haystack, needle, at) {
        let before = haystack[..found].chars().next_back();
        let after = haystack[found + needle.len()..].chars().next();
        if [before, after].into_iter().flatten().all(is_boundary) {
            return Some(found);
        }
        at = found + needle.len().max(1);
    }
    None
}

/// Anything the analyzer would have cut a Latin word at.
fn is_boundary(c: char) -> bool {
    !c.is_alphanumeric() || is_cjk(c)
}

/// Case-insensitive substring search that reports a byte offset into
/// `haystack`. Lowercasing the haystack first would be simpler but can change
/// its length, which then misplaces every offset taken from it.
fn find_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    (from..=h.len() - n.len())
        .find(|&i| haystack.is_char_boundary(i) && h[i..i + n.len()].eq_ignore_ascii_case(n))
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn escape_regex(token: &str) -> String {
    let mut out = String::with_capacity(token.len());
    for c in token.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The first non-empty lines of a body, for hits with nothing to highlight.
fn opening(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    let mut out = String::new();
    for line in trimmed.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line.trim());
        if out.chars().count() >= max {
            break;
        }
    }
    match out.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &out[..end]),
        None => out,
    }
}

/// Accepts the kind names the CLI exposes.
pub fn parse_kind(value: &str) -> Result<String> {
    let lowered = value.to_ascii_lowercase();
    match lowered.as_str() {
        KIND_PROMPT | KIND_REPLY | KIND_THINK | KIND_EDIT => Ok(lowered),
        other => bail!("unknown kind {other:?}; expected prompt, reply, think or edit"),
    }
}

/// `2026-08-01`, or a plain day count like `30d`.
pub fn parse_since(value: &str) -> Result<DateTime<Utc>> {
    if let Some(days) = value.strip_suffix('d').and_then(|n| n.parse::<i64>().ok()) {
        return Ok(Utc::now() - chrono::Duration::days(days));
    }
    let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .with_context(|| format!("could not read {value:?} as a date (YYYY-MM-DD) or age (30d)"))?;
    Ok(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("midnight always exists")))
}
