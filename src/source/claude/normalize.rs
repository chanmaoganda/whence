//! Turning Claude Code's raw line stream into a [`Session`].
//!
//! This is the load-bearing module of the Claude adapter. Three properties of
//! the on-disk format make a naive reading wrong:
//!
//! 1. **One API response spans many lines.** Every line of a response repeats
//!    the same `requestId` *and the same `usage` block*. Summing `usage` per
//!    line inflates token counts — on a real 545-line session here, output
//!    tokens go from 270,569 (correct) to 521,960 (93% too high). We fold lines
//!    by `requestId` and count usage once.
//!
//! 2. **`type: "user"` is not the same as "the human said something".** Most
//!    user records are tool results being fed back to the model; others are
//!    hook output or interrupt markers. Only a string `content`, or blocks
//!    containing real text, is a human prompt.
//!
//! 3. **Interrupts are data.** `[Request interrupted by user]` markers and
//!    `interruptedMessageId` tell you where Claude went wrong — worth keeping,
//!    not filtering out.

use super::raw::{Block, Content, RawRecord, Usage};
use crate::model::*;
use crate::source::ParseStats;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::Path;

const INTERRUPT_MARKER: &str = "[Request interrupted by user";

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
    /// `request_id` of the response currently being folded.
    open_step: Option<String>,
    /// `tool_use_id` -> (turn, step, call) so results can be attached in one pass.
    call_index: HashMap<String, (usize, usize, usize)>,
}

impl Builder {
    fn new(path: &Path) -> Self {
        Builder {
            session: Session {
                id: session_id_from_path(path),
                harness: Harness::Claude,
                project: String::new(),
                source_path: path.to_path_buf(),
                kind: session_kind_from_path(path),
                branch: None,
                agent_version: None,
                model: None,
                title: None,
                started_at: None,
                ended_at: None,
                turns: Vec::new(),
                file_touches: Vec::new(),
                usage: UsageTotals::default(),
            },
            open_step: None,
            call_index: HashMap::new(),
        }
    }

    fn push(&mut self, rec: RawRecord) {
        if let Some(env) = rec.envelope() {
            if self.session.project.is_empty() {
                if let Some(cwd) = &env.cwd {
                    self.session.project = cwd.clone();
                }
            }
            if self.session.branch.is_none()
                && env.git_branch.as_deref().is_some_and(|b| !b.is_empty())
            {
                self.session.branch = env.git_branch.clone();
            }
            if self.session.agent_version.is_none() {
                self.session.agent_version = env.version.clone();
            }
            if let Some(ts) = parse_ts(env.timestamp.as_deref()) {
                self.session.started_at.get_or_insert(ts);
                self.session.ended_at = Some(ts);
            }
        }

        match rec {
            RawRecord::AiTitle(t) => {
                if let Some(title) = t.ai_title.filter(|s| !s.is_empty()) {
                    self.session.title = Some(title);
                }
            }
            RawRecord::FileHistoryDelta(d) => {
                self.session.file_touches.push(FileTouch {
                    path: d.tracking_path,
                    message_id: d.message_id,
                    timestamp: parse_ts(d.timestamp.as_deref()),
                });
            }
            RawRecord::User(u) => {
                let ts = parse_ts(u.envelope.timestamp.as_deref());

                if u.interrupted_message_id.is_some() {
                    self.mark_interrupted();
                }
                if u.tool_denial_kind.is_some() {
                    self.mark_last_call_denied();
                }

                let Some(msg) = u.message else { return };
                match msg.content {
                    Content::Text(text) => {
                        self.push_human_text(&text, ts, u.envelope.uuid, u.is_meta)
                    }
                    Content::Blocks(blocks) => {
                        let mut text = String::new();
                        for block in blocks {
                            match block {
                                Block::Text { text: t } => {
                                    if !text.is_empty() {
                                        text.push('\n');
                                    }
                                    text.push_str(&t);
                                }
                                Block::ToolResult {
                                    tool_use_id,
                                    content,
                                    is_error,
                                } => {
                                    self.attach_result(&tool_use_id, content, is_error);
                                }
                                _ => {}
                            }
                        }
                        if !text.is_empty() {
                            self.push_human_text(&text, ts, u.envelope.uuid, u.is_meta);
                        }
                    }
                    Content::Other(_) => {}
                }
            }
            RawRecord::Assistant(a) => {
                let ts = parse_ts(a.envelope.timestamp.as_deref());
                let Some(msg) = a.message else { return };
                // Fall back through requestId -> message id -> uuid so a response
                // is still folded correctly on older transcript versions.
                let key = a
                    .request_id
                    .clone()
                    .or_else(|| msg.id.clone())
                    .or_else(|| a.envelope.uuid.clone())
                    .unwrap_or_default();

                if self.open_step.as_deref() != Some(key.as_str()) {
                    self.begin_step(Step {
                        response_id: a.request_id,
                        message_id: msg.id,
                        uuids: Vec::new(),
                        model: msg.model,
                        timestamp: ts,
                        text: String::new(),
                        thinking: String::new(),
                        tool_calls: Vec::new(),
                        usage: UsageTotals::default(),
                    });
                    self.open_step = Some(key);
                }

                // Keep every line's uuid: `file-history-delta` attributes an edit
                // to the *line* that carried the tool call, not to `message.id`.
                if let Some(uuid) = a.envelope.uuid {
                    if let Some(step) = self.current_step() {
                        step.uuids.push(uuid);
                    }
                }

                // Usage is repeated verbatim on every line of the response; take
                // it once, defensively keeping the largest value seen per field.
                if let Some(usage) = msg.usage {
                    self.merge_usage(usage);
                }
                for block in msg.content {
                    self.push_block(block);
                }
            }
            _ => {}
        }
    }

    fn push_human_text(
        &mut self,
        text: &str,
        ts: Option<DateTime<Utc>>,
        uuid: Option<String>,
        is_meta: bool,
    ) {
        if text.starts_with(INTERRUPT_MARKER) {
            self.mark_interrupted();
            return;
        }
        let cleaned = strip_noise(text);
        // Hook output and command expansions ride in on user records but are not
        // things a human typed.
        if is_meta || cleaned.is_empty() {
            return;
        }
        let index = self.session.turns.len();
        self.session.turns.push(Turn {
            index,
            prompt: Some(Prompt {
                text: cleaned,
                timestamp: ts,
                uuid,
            }),
            steps: Vec::new(),
            interrupted: false,
        });
        self.open_step = None;
    }

    /// Assistant work with no preceding prompt (resumed sessions, hook runs)
    /// still belongs somewhere, so open an anonymous turn for it.
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

    fn merge_usage(&mut self, usage: Usage) {
        let incoming: UsageTotals = usage.into();
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
            Block::Text { text } => {
                if !step.text.is_empty() {
                    step.text.push('\n');
                }
                step.text.push_str(&text);
            }
            Block::Thinking { thinking } => {
                if !step.thinking.is_empty() {
                    step.thinking.push('\n');
                }
                step.thinking.push_str(&thinking);
            }
            Block::ToolUse { id, name, input } => {
                let call_idx = step.tool_calls.len();
                step.tool_calls.push(ToolCall {
                    id: id.clone(),
                    name,
                    input,
                    result: None,
                    is_error: false,
                    denied: false,
                });
                self.call_index.insert(id, (turn_idx, step_idx, call_idx));
            }
            _ => {}
        }
    }

    fn attach_result(
        &mut self,
        tool_use_id: &str,
        content: Option<serde_json::Value>,
        is_error: bool,
    ) {
        let Some(&(t, s, c)) = self.call_index.get(tool_use_id) else {
            return;
        };
        let Some(call) = self
            .session
            .turns
            .get_mut(t)
            .and_then(|turn| turn.steps.get_mut(s))
            .and_then(|step| step.tool_calls.get_mut(c))
        else {
            return;
        };
        call.result = content.as_ref().map(flatten_result);
        call.is_error = is_error;
    }

    fn mark_interrupted(&mut self) {
        if let Some(turn) = self.session.turns.last_mut() {
            turn.interrupted = true;
        }
    }

    fn mark_last_call_denied(&mut self) {
        if let Some(step) = self.current_step() {
            if let Some(call) = step.tool_calls.last_mut() {
                call.denied = true;
            }
        }
    }

    fn finish(mut self) -> Session {
        let mut total = UsageTotals::default();
        for turn in &self.session.turns {
            for step in &turn.steps {
                total.add(step.usage);
            }
        }
        self.session.usage = total;
        self.session.model = dominant_model(&self.session);
        if self.session.project.is_empty() {
            self.session.project = project_from_path(&self.session.source_path);
        }
        self.session
    }
}

/// Tool results are either a string or a list of content blocks.
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

/// Drop `<system-reminder>` payloads injected by the harness — they are not
/// user words and would otherwise dominate search results.
fn strip_noise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<system-reminder>") {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find("</system-reminder>") {
            Some(end) => &rest[start + end + "</system-reminder>".len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out.trim().to_string()
}

fn parse_ts(s: Option<&str>) -> Option<DateTime<Utc>> {
    let s = s?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Subagent transcripts live in a `subagents/` directory beneath their parent.
fn session_kind_from_path(path: &Path) -> SessionKind {
    let is_subagent = path.components().any(|c| c.as_os_str() == "subagents");
    if is_subagent {
        SessionKind::Subagent {
            agent: session_id_from_path(path),
        }
    } else {
        SessionKind::Main
    }
}

/// Last-resort project name when no record carried a `cwd`: decode it from the
/// directory name, which is the path with separators replaced by dashes.
fn project_from_path(path: &Path) -> String {
    let mut dir: &Path = path;
    while let Some(parent) = dir.parent() {
        if parent.file_name().is_some_and(|n| n == "projects") {
            return dir
                .file_name()
                .map(|n| n.to_string_lossy().replace('-', "/"))
                .unwrap_or_default();
        }
        dir = parent;
    }
    String::new()
}

/// The model that did most of the work. A session can span models — a compaction
/// or a `/model` switch — so this is the busiest one, not the first.
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

/// Claude Code repeats the same `usage` block on every line of a response, so
/// this conversion is only ever applied once per folded [`Step`].
impl From<Usage> for UsageTotals {
    fn from(u: Usage) -> Self {
        UsageTotals {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_creation: u.cache_creation_input_tokens,
            cache_read: u.cache_read_input_tokens,
        }
    }
}
