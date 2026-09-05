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
        match query.fuzzy {
            Fuzzy::Never => Ok(Results {
                hits: self.run(query, false)?,
                relaxed: false,
            }),
            Fuzzy::Always if searchable => Ok(Results {
                hits: self.run(query, true)?,
                relaxed: true,
            }),
            Fuzzy::Always => Ok(Results {
                hits: self.run(query, false)?,
                relaxed: false,
            }),
            Fuzzy::Auto => {
                let exact = self.run(query, false)?;
                if !exact.is_empty() || !searchable {
                    return Ok(Results {
                        hits: exact,
                        relaxed: false,
                    });
                }
                // Nothing matched exactly, so the alternative to relaxing is an
                // empty screen. Only then is the extra recall worth the noise.
                let hits = self.run(query, true)?;
                let relaxed = !hits.is_empty();
                Ok(Results { hits, relaxed })
            }
        }
    }

    fn run(&self, query: &Query, fuzzy: bool) -> Result<Vec<Hit>> {
        let f = self.fields;
        let reader = self.index.reader()?;
        let searcher = reader.searcher();

        let text: Box<dyn TantivyQuery> = if query.text.trim().is_empty() {
            Box::new(AllQuery)
        } else if fuzzy {
            self.fuzzy_query(&query.text)?
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
        let tokens = self.text_tokens(&query.text)?;

        top.into_iter()
            .map(|(score, addr)| {
                let doc: TantivyDocument = searcher.doc(addr)?;
                let body = stored_text(&doc, f.body).unwrap_or_default();
                Ok(self.hit(&doc, score, locate(&body, &tokens)))
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
                Ok(self.hit(&doc, 0.0, excerpt))
            })
            .collect()
    }

    fn hit(&self, doc: &TantivyDocument, score: Score, excerpt: Excerpt) -> Hit {
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
    fn fuzzy_query(&self, text: &str) -> Result<Box<dyn TantivyQuery>> {
        let mut clauses: Vec<(Occur, Box<dyn TantivyQuery>)> = Vec::new();
        for token in self.text_tokens(text)? {
            clauses.push((Occur::Must, self.token_query(&token)?));
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

/// A window of `body` around the first token that literally occurs in it, with
/// every occurrence inside the window highlighted.
///
/// Every token of an exact query is in the body verbatim, because the analyzer
/// cut it out of text like this one. A token matched by edit distance is not
/// (`normlize` never appears; `normalize` does), so nothing is found and the
/// opening words are the honest fallback. Substring matches — which is every
/// CJK token — always locate.
fn locate(body: &str, tokens: &[String]) -> Excerpt {
    let Some(at) = tokens.iter().filter_map(|t| find_ci(body, t, 0)).min() else {
        return Excerpt {
            text: opening(body, EXCERPT),
            highlights: Vec::new(),
        };
    };

    // Back off a little so the match is not flush against the left edge, and
    // keep both ends on character boundaries.
    let start = floor_char_boundary(body, at.saturating_sub(EXCERPT / 4));
    let end = ceil_char_boundary(body, (start + EXCERPT).min(body.len()));
    let window = body[start..end].replace('\n', " ");

    let mut highlights: Vec<Range<usize>> = Vec::new();
    for token in tokens {
        let mut from = 0;
        while let Some(hit) = find_ci(&window, token, from) {
            highlights.push(hit..hit + token.len());
            from = hit + token.len().max(1);
        }
    }
    highlights.sort_by_key(|r| r.start);
    Excerpt {
        text: window,
        highlights,
    }
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
