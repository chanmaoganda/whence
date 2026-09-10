//! Turning Oh My Pi's raw line stream into a [`Session`].
//!
//! omp is the best-behaved of the three formats — usage is per-response and
//! honest, and one record really is one response — so most of this module is
//! plain assembly. Four things still make a naive reading wrong:
//!
//! 1. **`reasoningTokens` is inside `output`, not beside it.** Every response
//!    satisfies `totalTokens == input + output + cacheRead`, with the reasoning
//!    count a subset of `output`. Adding the two reads 33,024 output tokens on
//!    the reference session against a true 20,713 — 59% too high. This is the
//!    mirror image of the Claude trap: there the sibling buckets are real and
//!    must be summed, here the nested one must not be.
//!
//! 2. **Every tool call is recorded twice.** The assistant message carries a
//!    `toolCall` block, and a separate `custom` record of `customType`
//!    `tool_execution_start` repeats it — same `toolCallId`, same arguments —
//!    when the call begins to run. On the reference session both are 68, over
//!    68 distinct ids. Read the message blocks; the `custom` records are
//!    confirmation, and counting both doubles every tool count in the corpus.
//!
//! 3. **`role` has three values, and the third is not a person.** `toolResult`
//!    is a role of its own rather than a block inside a user turn, so a reader
//!    that splits on user-versus-assistant files tool output under whichever
//!    one it picked. Tool results are attached to their call and never indexed.
//!
//! 4. **There are two clocks.** The envelope's `timestamp` is RFC 3339; the
//!    `timestamp` inside a message is epoch milliseconds. Reading the latter as
//!    seconds puts the conversation in the year 58699.
//!
//! And one thing that is not a trap but changes what the tool can show:
//! **thinking arrives in the clear.** Claude Code and Codex both encrypt it, so
//! [`Step::thinking`] has been empty for every session whence has ever read.
//! omp writes the reasoning text, which means it is searchable.

use super::raw::{Block, Message, RawRecord, Usage};
use crate::model::*;
use crate::source::ParseStats;
use chrono::{DateTime, TimeZone, Utc};
use std::collections::HashMap;
use std::path::Path;

/// Tools whose result names a file they changed. omp records an edit nowhere
/// else — there is no equivalent of Claude's `file-history-delta`.
const EDITING_TOOLS: [&str; 3] = ["write", "edit", "notebook"];

/// Rebuild one session from the lines of one `.jsonl` file.
pub fn normalize(path: &Path, lines: &mut dyn Iterator<Item = String>) -> (Session, ParseStats) {
    let mut stats = ParseStats::default();
    let mut b = Builder::new(path);

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        stats.lines += 1;
        match serde_json::from_str::<RawRecord>(&line) {
            Ok(rec) => b.push(rec),
            Err(_) => stats.parse_errors += 1,
        }
    }
    (b.finish(), stats)
}

struct Builder {
    session: Session,
    /// The model named by the most recent `model_change`. Responses usually
    /// carry a null `model` of their own, so this is what a step inherits.
    current_model: Option<String>,
    /// `responseId` of the response being folded, when the format supplied one.
    open_step: Option<String>,
    /// `toolCallId` -> (turn, step, call), so a result can be attached in one
    /// pass and an edit attributed to the response that made it.
    call_index: HashMap<String, (usize, usize, usize)>,
}

impl Builder {
    fn new(path: &Path) -> Self {
        Builder {
            session: Session {
                id: session_id_from_path(path),
                harness: Harness::Omp,
                project: String::new(),
                source_path: path.to_path_buf(),
                kind: SessionKind::Main,
                branch: None,
                // The transcript records a format version, never omp's own.
                agent_version: None,
                model: None,
                title: None,
                started_at: None,
                ended_at: None,
                turns: Vec::new(),
                file_touches: Vec::new(),
                usage: UsageTotals::default(),
            },
            current_model: None,
            open_step: None,
            call_index: HashMap::new(),
        }
    }

    fn push(&mut self, rec: RawRecord) {
        if let Some(env) = rec.envelope() {
            if let Some(ts) = parse_ts(env.timestamp.as_deref()) {
                self.session.started_at.get_or_insert(ts);
                self.session.ended_at = Some(ts);
            }
        }

        match rec {
            RawRecord::Session(s) => {
                if let Some(id) = s.id.filter(|s| !s.is_empty()) {
                    self.session.id = id;
                }
                if let Some(cwd) = s.cwd.filter(|s| !s.is_empty()) {
                    self.session.project = cwd;
                }
                self.set_title(s.title);
                if let Some(ts) = parse_ts(s.timestamp.as_deref()) {
                    self.session.started_at.get_or_insert(ts);
                    self.session.ended_at.get_or_insert(ts);
                }
            }
            // The header and every retitling in file order, last one winning:
            // the header is rewritten in place and a `title_change` is appended,
            // so whichever came last on disk is the current name.
            RawRecord::Title(t) => self.set_title(t.title),
            RawRecord::TitleChange(t) => self.set_title(t.title),
            RawRecord::ModelChange(m) => {
                // `smol`, `slow` and `plan` are the other model slots; only the
                // default one answers the conversation.
                if m.role.as_deref().unwrap_or("default") == "default" {
                    self.current_model = m.model.filter(|s| !s.is_empty());
                }
            }
            // The context was cleared. The next response cannot see the last
            // one, so it is not a continuation of it.
            RawRecord::ResetBoundary(_) => self.open_step = None,
            RawRecord::Message(r) => {
                let ts = parse_ts(r.envelope.timestamp.as_deref());
                let Some(msg) = r.message else { return };
                match msg {
                    Message::User(u) => {
                        // A message the harness wrote wearing the user's role is
                        // not a prompt. Absent attribution is trusted, so that a
                        // release that stops stamping it does not silently empty
                        // every session of its prompts.
                        if u.attribution.as_deref().is_some_and(|a| a != "user") {
                            return;
                        }
                        let text = join_text(&u.content);
                        let ts = ts.or_else(|| from_millis(u.timestamp));
                        self.push_prompt(text.trim(), ts, r.envelope.id);
                    }
                    Message::Assistant(a) => {
                        let ts = ts.or_else(|| from_millis(a.timestamp));
                        let key = a.response_id.clone().filter(|s| !s.is_empty());
                        // One record is one response here, so a step is opened
                        // per record unless the format explicitly says two
                        // records share a response.
                        let continues = key.is_some() && key == self.open_step;
                        if !continues {
                            let model = qualified(a.model, a.provider);
                            self.begin_step(Step {
                                response_id: a.response_id,
                                message_id: r.envelope.id.clone(),
                                uuids: r.envelope.id.into_iter().collect(),
                                model: model.or_else(|| self.current_model.clone()),
                                timestamp: ts,
                                text: String::new(),
                                thinking: String::new(),
                                tool_calls: Vec::new(),
                                usage: UsageTotals::default(),
                            });
                            self.open_step = key;
                        } else if let Some(id) = r.envelope.id {
                            if let Some(step) = self.current_step() {
                                step.uuids.push(id);
                            }
                        }
                        if let Some(usage) = a.usage {
                            self.merge_usage(usage.into());
                        }
                        for block in a.content {
                            self.push_block(block);
                        }
                    }
                    Message::ToolResult(t) => {
                        let Some(id) = t.tool_call_id else { return };
                        let result = t.content.as_ref().map(flatten_result);
                        self.attach_result(&id, result, t.is_error);
                        if t.tool_name
                            .as_deref()
                            .is_some_and(|n| EDITING_TOOLS.contains(&n))
                        {
                            let ts = ts.or_else(|| from_millis(t.timestamp));
                            self.record_edit(&id, t.details.as_ref(), ts);
                        }
                    }
                    Message::Other => {}
                }
            }
            // A notice omp injected into the conversation — a finished
            // background job, most often. It reads like a turn and is not one.
            RawRecord::CustomMessage(_) => {}
            RawRecord::Custom | RawRecord::Other => {}
        }
    }

    fn set_title(&mut self, title: Option<String>) {
        if let Some(title) = title.filter(|t| !t.trim().is_empty()) {
            self.session.title = Some(title);
        }
    }

    fn push_prompt(&mut self, text: &str, ts: Option<DateTime<Utc>>, id: Option<String>) {
        if text.is_empty() {
            return;
        }
        let index = self.session.turns.len();
        self.session.turns.push(Turn {
            index,
            prompt: Some(Prompt {
                text: text.to_string(),
                timestamp: ts,
                uuid: id,
            }),
            steps: Vec::new(),
            interrupted: false,
        });
        self.open_step = None;
    }

    /// Work with no prompt in front of it — a resumed session, or everything
    /// after a steering message that arrived mid-response — still belongs
    /// somewhere.
    fn current_turn(&mut self) -> &mut Turn {
        if self.session.turns.is_empty() {
            self.session.turns.push(Turn {
                index: 0,
                prompt: None,
                steps: Vec::new(),
                interrupted: false,
            });
        }
        self.session
            .turns
            .last_mut()
            .expect("just ensured non-empty")
    }

    fn begin_step(&mut self, step: Step) {
        self.current_turn().steps.push(step);
    }

    fn current_step(&mut self) -> Option<&mut Step> {
        self.session.turns.last_mut()?.steps.last_mut()
    }

    /// Largest value seen per field rather than a sum.
    ///
    /// Every response in the corpus is one record carrying its counts once, so
    /// this only ever runs on a fresh step. It is insurance for the case where
    /// two records share a `responseId`: if the counts were repeated across
    /// them — which is what Claude Code does — summing would double the
    /// response, and over-counting tokens is the mistake this whole layer
    /// exists to avoid.
    fn merge_usage(&mut self, incoming: UsageTotals) {
        if let Some(step) = self.current_step() {
            step.usage.input = step.usage.input.max(incoming.input);
            step.usage.output = step.usage.output.max(incoming.output);
            step.usage.cache_creation = step.usage.cache_creation.max(incoming.cache_creation);
            step.usage.cache_read = step.usage.cache_read.max(incoming.cache_read);
        }
    }

    fn push_block(&mut self, block: Block) {
        let (turn_idx, step_idx) = match self.session.turns.last() {
            Some(t) if !t.steps.is_empty() => (t.index, t.steps.len() - 1),
            _ => return,
        };
        let Some(step) = self.current_step() else {
            return;
        };
        match block {
            Block::Text { text } => push_para(&mut step.text, &text),
            // In the clear, unlike every other harness — see the module docs.
            Block::Thinking { thinking } => push_para(&mut step.thinking, &thinking),
            Block::ToolCall {
                id,
                name,
                arguments,
            } => {
                let call_idx = step.tool_calls.len();
                step.tool_calls.push(ToolCall {
                    id: id.clone(),
                    name,
                    input: arguments,
                    result: None,
                    is_error: false,
                    denied: false,
                });
                self.call_index.insert(id, (turn_idx, step_idx, call_idx));
            }
            Block::Other => {}
        }
    }

    fn call_mut(&mut self, tool_call_id: &str) -> Option<&mut ToolCall> {
        let &(t, s, c) = self.call_index.get(tool_call_id)?;
        self.session
            .turns
            .get_mut(t)
            .and_then(|turn| turn.steps.get_mut(s))
            .and_then(|step| step.tool_calls.get_mut(c))
    }

    fn attach_result(&mut self, tool_call_id: &str, result: Option<String>, is_error: bool) {
        if let Some(call) = self.call_mut(tool_call_id) {
            call.result = result;
            call.is_error = is_error;
        }
    }

    /// Attribute an edit to the response that asked for it.
    ///
    /// The path comes from the result's `resolvedPath` where there is one —
    /// that is the file omp actually opened, tildes and relative segments
    /// already resolved — and from the call's own `path` argument otherwise.
    fn record_edit(
        &mut self,
        tool_call_id: &str,
        details: Option<&serde_json::Value>,
        ts: Option<DateTime<Utc>>,
    ) {
        let resolved = details
            .and_then(|d| d.get("resolvedPath"))
            .and_then(|p| p.as_str())
            .map(str::to_string);
        let Some(&(t, s, c)) = self.call_index.get(tool_call_id) else {
            return;
        };
        let Some(step) = self
            .session
            .turns
            .get_mut(t)
            .and_then(|turn| turn.steps.get_mut(s))
        else {
            return;
        };
        let path = resolved.or_else(|| {
            step.tool_calls
                .get(c)?
                .input
                .get("path")?
                .as_str()
                .map(str::to_string)
        });
        let Some(path) = path.filter(|p| !p.is_empty()) else {
            return;
        };
        let message_id = step.message_id.clone();
        self.session.file_touches.push(FileTouch {
            path,
            message_id,
            timestamp: ts.or(step.timestamp),
        });
    }

    fn finish(mut self) -> Session {
        let mut total = UsageTotals::default();
        for turn in &self.session.turns {
            for step in &turn.steps {
                total.add(step.usage);
            }
        }
        self.session.usage = total;
        self.session.model = dominant_model(&self.session).or(self.current_model);
        if self.session.project.is_empty() {
            self.session.project = project_from_path(&self.session.source_path);
        }
        self.session
    }
}

fn push_para(buf: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(text);
}

fn join_text(blocks: &[Block]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let Block::Text { text } = block {
            push_para(&mut out, text);
        }
    }
    out
}

/// A tool result is a list of content blocks, or a bare string on older records.
fn flatten_result(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

fn parse_ts(s: Option<&str>) -> Option<DateTime<Utc>> {
    let s = s?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// The clock inside a message, which counts milliseconds and not seconds.
fn from_millis(ms: Option<i64>) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms?).single()
}

/// `<iso-8601>_<session-uuid>.jsonl`. Only a fallback: the `session` record
/// names the session, and this is what is left when the file has been truncated
/// past it.
fn session_id_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match stem.rsplit_once('_') {
        Some((_, id)) => id.to_string(),
        None => stem,
    }
}

/// Last-resort project name when no `session` record survived.
///
/// omp names the directory `--<cwd>--`, having dropped the leading slash and
/// replaced every `/`, `\\` and `:` with a dash. That is not reversible — a
/// directory with a dash in its name encodes exactly like one more level of
/// nesting, so `/code/stock/rtrade-rewrite` comes back as
/// `/code/stock/rtrade/rewrite` — which is why it is the fallback and the
/// `session` record's `cwd` is the answer. Claude Code's directory names have
/// the same ambiguity.
fn project_from_path(path: &Path) -> String {
    let mut dir: &Path = path;
    while let Some(parent) = dir.parent() {
        if parent.file_name().is_some_and(|n| n == "sessions") {
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let inner = name
                .strip_prefix("--")
                .and_then(|n| n.strip_suffix("--"))
                .unwrap_or(&name);
            if inner.is_empty() {
                return String::new();
            }
            return format!("/{}", inner.replace('-', "/"));
        }
        dir = parent;
    }
    String::new()
}

/// A response names its model bare and its provider separately, while a
/// `model_change` names the two together. Spelling both `provider/model` is what
/// keeps one session from reading as two models in `whence stats`.
fn qualified(model: Option<String>, provider: Option<String>) -> Option<String> {
    let model = model.filter(|m| !m.is_empty())?;
    if model.contains('/') {
        return Some(model);
    }
    match provider.filter(|p| !p.is_empty()) {
        Some(provider) => Some(format!("{provider}/{model}")),
        None => Some(model),
    }
}

/// The model that did most of the work — a session can switch models mid-way.
fn dominant_model(session: &Session) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for step in session.turns.iter().flat_map(|t| t.steps.iter()) {
        if let Some(model) = step.model.as_deref() {
            *counts.entry(model).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|&(name, count)| (count, std::cmp::Reverse(name)))
        .map(|(name, _)| name.to_string())
}

/// omp's buckets are already disjoint — `totalTokens == input + output +
/// cacheRead` — which is the arrangement [`UsageTotals`] uses, so nothing has
/// to be subtracted back out the way the Codex adapter does. `reasoningTokens`
/// is deliberately absent: it is counted inside `output` already.
impl From<Usage> for UsageTotals {
    fn from(u: Usage) -> Self {
        UsageTotals {
            input: u.input,
            output: u.output,
            cache_creation: u.cache_write,
            cache_read: u.cache_read,
        }
    }
}
