//! The full-text index: schema, incremental build, and the tail state that
//! makes re-indexing cheap.
//!
//! Two decisions drive this module.
//!
//! **The tokenizer must handle CJK.** These transcripts are largely Chinese, and
//! tantivy's default tokenizer splits on whitespace and punctuation — which
//! turns a whole Chinese sentence into one unsearchable token. Text fields go
//! through `jieba`; path-shaped fields go through a simple splitter so
//! `src/normalize.rs` and `/code/rust/whence/src/model.rs` match the same
//! documents.
//!
//! **The harness is a field, not a separate index.** Every document records
//! which agent it came from, so one index answers "where did I see this" across
//! all of them and `--harness` narrows it without a second store to keep in step.
//!
//! **Transcripts are append-only, so re-reading them all is waste.** We remember
//! `(inode, offset)` per file, where `offset` is the byte length already
//! indexed. A file whose inode and length are unchanged is skipped outright —
//! the overwhelmingly common case. A file that grew is re-read in full and its
//! old documents are deleted first: a session is folded from its whole line
//! stream (see [`crate::source`]), so its documents cannot be derived from
//! the appended tail alone. The saving is in the files we never open.

use crate::model::{Session, Turn};
use crate::source::{self, Root, Transcript};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tantivy::directory::MmapDirectory;
use tantivy::schema::{
    DateOptions, DateTimePrecision, Field, IndexRecordOption, Schema, TextFieldIndexing,
    TextOptions, Value, INDEXED, STORED, STRING,
};
use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer};
use tantivy::{DateTime as TantivyDate, Index, TantivyDocument, Term};

/// Bumped whenever the schema changes; a stale index is rebuilt, not rejected.
const INDEX_FORMAT: u32 = 1;

/// jieba, for prose. Named in `meta.json`, so it must be re-registered on open.
pub const TOK_TEXT: &str = "jieba";
/// Simple splitting on non-alphanumerics, for paths, projects and tool names.
pub const TOK_PATH: &str = "path";

/// Cap on indexed text per document. Long tool-shaped pastes add index weight
/// without adding anything you would search for.
const MAX_BODY: usize = 20_000;

/// How many transcripts to parse in parallel before handing documents to the
/// single writer. Bounds peak memory on a large corpus.
const BATCH: usize = 64;

pub const KIND_PROMPT: &str = "prompt";
pub const KIND_REPLY: &str = "reply";
pub const KIND_THINK: &str = "think";
pub const KIND_EDIT: &str = "edit";

/// Field handles, resolved once so nothing downstream looks fields up by name.
#[derive(Debug, Clone, Copy)]
pub struct Fields {
    pub harness: Field,
    pub kind: Field,
    pub session: Field,
    pub source: Field,
    pub project: Field,
    pub file: Field,
    pub tools: Field,
    pub title: Field,
    pub body: Field,
    pub turn: Field,
    pub ts: Field,
}

impl Fields {
    fn resolve(schema: &Schema) -> Result<Self> {
        let f = |name: &str| {
            schema
                .get_field(name)
                .with_context(|| format!("index is missing the `{name}` field"))
        };
        Ok(Fields {
            harness: f("harness")?,
            kind: f("kind")?,
            session: f("session")?,
            source: f("source")?,
            project: f("project")?,
            file: f("file")?,
            tools: f("tools")?,
            title: f("title")?,
            body: f("body")?,
            turn: f("turn")?,
            ts: f("ts")?,
        })
    }
}

pub struct SearchIndex {
    pub(crate) dir: PathBuf,
    pub(crate) index: Index,
    pub(crate) fields: Fields,
}

/// `$XDG_CACHE_HOME/whence/index`, falling back to `~/.cache/whence/index`.
pub fn default_index_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".cache"),
    };
    Some(base.join("whence").join("index"))
}

impl SearchIndex {
    /// Open the index at `dir`, creating it if absent. An index written by an
    /// older schema is discarded and rebuilt rather than reported as an error —
    /// it is a derived artifact, and the transcripts are the source of truth.
    pub fn open_or_create(dir: &Path) -> Result<Self> {
        let marker = dir.join("whence-format");
        if dir.join("meta.json").exists() && format_of(dir) != Some(INDEX_FORMAT) {
            std::fs::remove_dir_all(dir)
                .with_context(|| format!("clearing stale index at {}", dir.display()))?;
        }
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating index at {}", dir.display()))?;
        std::fs::write(&marker, INDEX_FORMAT.to_string())?;
        Self::mount(
            dir,
            Index::open_or_create(MmapDirectory::open(dir)?, schema())?,
        )
    }

    /// Open an existing index to query it. Adds nothing to the directory, and
    /// refuses to conjure an empty index out of a typo in `--index`. (tantivy
    /// still takes a meta lockfile to build a reader, so the directory has to be
    /// writable — this is "does not create an index", not "read-only".)
    pub fn open(dir: &Path) -> Result<Self> {
        anyhow::ensure!(
            dir.join("meta.json").exists(),
            "no index at {} — run `whence index` first",
            dir.display()
        );
        anyhow::ensure!(
            format_of(dir) == Some(INDEX_FORMAT),
            "the index at {} was written by a different version of whence — \
             run `whence index` to rebuild it",
            dir.display()
        );
        Self::mount(dir, Index::open_in_dir(dir)?)
    }

    fn mount(dir: &Path, index: Index) -> Result<Self> {
        register_tokenizers(&index);
        let fields = Fields::resolve(&index.schema())?;
        Ok(SearchIndex {
            dir: dir.to_path_buf(),
            index,
            fields,
        })
    }

    /// Index every transcript that has changed since the last run. `force`
    /// re-reads everything.
    ///
    /// Takes the file list rather than a root because discovery is per-harness
    /// and already knows which adapter owns each file: re-deriving that here
    /// would mean either sniffing every file again or teaching the index about
    /// on-disk layouts, and neither belongs in an index.
    pub fn build(
        &self,
        transcripts: &[Transcript],
        roots: &[Root],
        force: bool,
    ) -> Result<BuildReport> {
        let started = Instant::now();
        let mut state = if force {
            TailState::default()
        } else {
            TailState::load(&self.dir)
        };

        let files = transcripts;
        let mut report = BuildReport {
            scanned: files.len(),
            ..Default::default()
        };

        // Decide what to do with each file from a stat alone — no transcript is
        // opened until we know it changed.
        let mut work: Vec<(Transcript, FileTail, bool)> = Vec::new();
        let mut present: HashSet<String> = HashSet::with_capacity(files.len());
        for transcript in files {
            let key = transcript.path.to_string_lossy().into_owned();
            present.insert(key.clone());
            let Ok(tail) = FileTail::stat(&transcript.path) else {
                continue;
            };
            match state.files.get(&key) {
                Some(&known) if known == tail => report.unchanged += 1,
                Some(_) => {
                    report.updated += 1;
                    report.bytes += tail.offset;
                    work.push((transcript.clone(), tail, true));
                }
                None => {
                    report.fresh += 1;
                    report.bytes += tail.offset;
                    work.push((transcript.clone(), tail, false));
                }
            }
        }

        // Transcripts that vanished (a project directory deleted, a session
        // removed) must not linger in the index. Only consider the subtrees we
        // were asked to scan, so `--root` on a subdirectory, or a run narrowed
        // to one harness, is not destructive to the rest.
        let prefixes: Vec<String> = roots
            .iter()
            .map(|r| r.path.to_string_lossy().into_owned())
            .collect();
        let gone: Vec<String> = state
            .files
            .keys()
            .filter(|k| {
                prefixes.iter().any(|prefix| k.starts_with(prefix)) && !present.contains(*k)
            })
            .cloned()
            .collect();
        report.dropped = gone.len();

        if work.is_empty() && gone.is_empty() {
            report.elapsed = started.elapsed();
            return Ok(report);
        }

        let mut writer = self.index.writer(WRITER_HEAP)?;

        // Deletes must land before the replacement documents are added.
        for (transcript, _, stale) in &work {
            if *stale {
                writer.delete_term(term_source(self.fields, &transcript.path.to_string_lossy()));
            }
        }
        for key in &gone {
            writer.delete_term(term_source(self.fields, key));
            state.files.remove(key);
        }

        let mut done: Vec<(&Path, FileTail)> = Vec::with_capacity(work.len());
        for chunk in work.chunks(BATCH) {
            let batches: Vec<Option<Vec<TantivyDocument>>> = chunk
                .par_iter()
                .map(|(transcript, _, _)| {
                    source::normalize_with(transcript.harness, &transcript.path)
                        .ok()
                        .map(|(session, _)| session_docs(&session, &self.fields))
                })
                .collect();
            for ((transcript, tail, _), batch) in chunk.iter().zip(batches) {
                // A transcript we could not open records no tail state, so the
                // next run tries it again rather than pretending it is indexed.
                let Some(docs) = batch else {
                    report.unreadable += 1;
                    continue;
                };
                report.docs += docs.len();
                for doc in docs {
                    writer.add_document(doc)?;
                }
                done.push((transcript.path.as_path(), *tail));
            }
        }

        writer.commit()?;

        for (path, tail) in done {
            state
                .files
                .insert(path.to_string_lossy().into_owned(), tail);
        }
        state.save(&self.dir)?;

        report.elapsed = started.elapsed();
        Ok(report)
    }
}

const WRITER_HEAP: usize = 128 * 1024 * 1024;

fn term_source(fields: Fields, source: &str) -> Term {
    Term::from_field_text(fields.source, source)
}

#[derive(Debug, Default)]
pub struct BuildReport {
    pub scanned: usize,
    /// Transcripts seen for the first time.
    pub fresh: usize,
    /// Transcripts that grew since the last run and were re-read in full.
    pub updated: usize,
    /// Transcripts skipped without being opened.
    pub unchanged: usize,
    /// Transcripts that disappeared; their documents were removed.
    pub dropped: usize,
    /// Transcripts that could not be opened; retried on the next run.
    pub unreadable: usize,
    pub docs: usize,
    /// Bytes of transcript actually re-read.
    pub bytes: u64,
    pub elapsed: Duration,
}

impl BuildReport {
    pub fn touched(&self) -> usize {
        self.fresh + self.updated
    }
}

/// What we remember about a transcript between runs. The inode guards against a
/// path being reused by a different file; the offset is the byte length already
/// indexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTail {
    pub inode: u64,
    pub offset: u64,
}

impl FileTail {
    fn stat(path: &Path) -> std::io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        Ok(FileTail {
            inode: inode_of(&meta),
            offset: meta.len(),
        })
    }
}

#[cfg(unix)]
fn inode_of(meta: &std::fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::ino(meta)
}

#[cfg(not(unix))]
fn inode_of(_meta: &std::fs::Metadata) -> u64 {
    0
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TailState {
    #[serde(default)]
    pub files: BTreeMap<String, FileTail>,
}

impl TailState {
    fn path(dir: &Path) -> PathBuf {
        dir.join("whence-tail.json")
    }

    /// A missing or unreadable state file just means "index everything".
    fn load(dir: &Path) -> Self {
        std::fs::read(Self::path(dir))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, dir: &Path) -> Result<()> {
        if dir.as_os_str().is_empty() {
            return Ok(());
        }
        let tmp = Self::path(dir).with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, Self::path(dir))?;
        Ok(())
    }
}

/// The `INDEX_FORMAT` an index directory was written with, if it says.
fn format_of(dir: &Path) -> Option<u32> {
    std::fs::read_to_string(dir.join("whence-format"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn schema() -> Schema {
    let mut b = Schema::builder();
    // Stored and indexed as an exact token: it is a filter and a label, never
    // something you free-text search.
    b.add_text_field("harness", STRING | STORED);
    b.add_text_field("kind", STRING | STORED);
    b.add_text_field("session", STRING | STORED);
    b.add_text_field("source", STRING | STORED);
    b.add_text_field("project", tokenized(TOK_PATH));
    b.add_text_field("file", tokenized(TOK_PATH));
    b.add_text_field("tools", tokenized(TOK_PATH));
    b.add_text_field("title", tokenized(TOK_TEXT));
    b.add_text_field("body", tokenized(TOK_TEXT));
    b.add_u64_field("turn", STORED | INDEXED);
    b.add_date_field(
        "ts",
        DateOptions::default()
            .set_stored()
            .set_indexed()
            .set_fast()
            .set_precision(DateTimePrecision::Seconds),
    );
    b.build()
}

fn tokenized(tokenizer: &str) -> TextOptions {
    TextOptions::default().set_stored().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            // Positions cost space but buy phrase queries, which is how
            // `whence file` matches a path suffix.
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    )
}

pub fn register_tokenizers(index: &Index) {
    index.tokenizers().register(
        TOK_TEXT,
        TextAnalyzer::builder(tantivy_jieba::JiebaTokenizer::new())
            .filter(RemoveLongFilter::limit(64))
            .filter(LowerCaser)
            .build(),
    );
    index.tokenizers().register(
        TOK_PATH,
        TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(RemoveLongFilter::limit(64))
            .filter(LowerCaser)
            .build(),
    );
}

/// One session becomes several documents: what you asked, what came back, what
/// the agent was thinking, and every file it changed.
///
/// Tool *results* are deliberately absent. They are most of the corpus by
/// volume and they are where pasted secrets and file contents live; keeping
/// them out is a structural protection the MCP server relies on.
pub fn session_docs(session: &Session, f: &Fields) -> Vec<TantivyDocument> {
    let source = session.source_path.to_string_lossy().into_owned();
    let title = session.title.clone().unwrap_or_default();

    let base = |kind: &str, turn: usize, ts: Option<DateTime<Utc>>, body: &str| {
        let mut doc = TantivyDocument::new();
        doc.add_text(f.harness, session.harness.as_str());
        doc.add_text(f.kind, kind);
        doc.add_text(f.session, &session.id);
        doc.add_text(f.source, &source);
        doc.add_text(f.project, &session.project);
        doc.add_text(f.title, &title);
        doc.add_u64(f.turn, turn as u64);
        doc.add_text(f.body, truncate(body, MAX_BODY));
        if let Some(ts) = ts {
            doc.add_date(f.ts, TantivyDate::from_timestamp_secs(ts.timestamp()));
        }
        doc
    };

    let mut docs = Vec::new();
    for turn in &session.turns {
        if let Some(prompt) = &turn.prompt {
            if !prompt.text.trim().is_empty() {
                docs.push(base(
                    KIND_PROMPT,
                    turn.index,
                    prompt.timestamp,
                    &prompt.text,
                ));
            }
        }
        for step in &turn.steps {
            if !step.text.trim().is_empty() {
                let mut doc = base(KIND_REPLY, turn.index, step.timestamp, &step.text);
                for call in &step.tool_calls {
                    doc.add_text(f.tools, &call.name);
                }
                docs.push(doc);
            }
            if !step.thinking.trim().is_empty() {
                docs.push(base(KIND_THINK, turn.index, step.timestamp, &step.thinking));
            }
        }
    }

    // Edits carry the *why*: the prompt that asked for the change and the reply
    // that made it. Several deltas usually share one response — collapse them so
    // one edit to one file by one response is one document.
    let by_uuid = session.steps_by_uuid();
    let mut seen: HashSet<(&str, Option<&str>)> = HashSet::new();
    for touch in &session.file_touches {
        if !seen.insert((touch.path.as_str(), touch.message_id.as_deref())) {
            continue;
        }
        let linked = touch
            .message_id
            .as_deref()
            .and_then(|uuid| by_uuid.get(uuid).copied());
        let (turn_index, why) = match linked {
            Some((turn, step)) => (turn.index, why_text(turn, &step.text)),
            None => (0, String::new()),
        };
        let ts = touch
            .timestamp
            .or_else(|| linked.and_then(|(_, step)| step.timestamp));
        let mut doc = base(KIND_EDIT, turn_index, ts, &why);
        doc.add_text(f.file, absolute_path(&touch.path, &session.project));
        docs.push(doc);
    }
    docs
}

/// The searchable explanation for an edit: the ask, then the answer.
fn why_text(turn: &Turn, reply: &str) -> String {
    let prompt = turn.prompt.as_ref().map(|p| p.text.as_str()).unwrap_or("");
    match (prompt.trim().is_empty(), reply.trim().is_empty()) {
        (true, _) => reply.to_string(),
        (false, true) => prompt.to_string(),
        (false, false) => format!("{prompt}\n{reply}"),
    }
}

/// Recorded edit paths are relative to the session cwd about half the time
/// under Claude Code, and absolute under Codex. Normalize to absolute so a path
/// matches whichever way it was recorded, by whichever harness.
fn absolute_path(path: &str, project: &str) -> String {
    if path.starts_with('/') || project.is_empty() {
        path.to_string()
    } else {
        format!("{}/{}", project.trim_end_matches('/'), path)
    }
}

fn truncate(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// Read a stored string field back out of a hit.
pub(crate) fn stored_text(doc: &TantivyDocument, field: Field) -> Option<String> {
    doc.get_first(field)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}
