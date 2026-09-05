//! Turning Codex's rollout event log into a [`Session`].
//!
//! Codex records a session as an event log rather than a message list, and four
//! properties of it make a naive reading wrong. Each is measured against the
//! reference corpus in `~/.codex/sessions` and has a test in
//! `tests/codex.rs`.
//!
//! 1. **`total_token_usage` is cumulative, not a delta.** Every `token_count`
//!    event repeats the running total for the whole session. Summing them
//!    inflates token counts without limit — on a 73-response session here, the
//!    naive sum reaches 1.3M output tokens against a true 36,451. Session totals
//!    are taken from the *last* such event, never summed.
//!
//! 2. **Summing the per-response `last_token_usage` is also wrong, but subtly.**
//!    It reconciles exactly with the cumulative total on 6 of 7 sessions here.
//!    The one that disagrees is the one containing `thread_rolled_back` events:
//!    a rolled-back turn's tokens were spent and then un-spent, so they appear
//!    in a `last_token_usage` but never in the running total. Trust the
//!    cumulative figure; keep the per-response figures for attribution only.
//!
//! 3. **Everything the human said is recorded twice.** A prompt appears as a
//!    `message:user` response item *and* as a `user_message` event; a reply
//!    appears as a `message:assistant` item *and* as an `agent_message` event.
//!    We read the response items, which survive a fork or resume, and treat the
//!    events as confirmation. Counting both doubles every prompt in the index.
//!
//! 4. **`message:user` is usually not the human either.** The harness injects
//!    `AGENTS.md`, `<environment_context>` and permission preambles as user
//!    messages. In the reference corpus every session carries exactly one such
//!    injection ahead of the first real prompt.
//!
//! 5. **A patch's `call_id` matches nothing.** `patch_apply_end` — the only
//!    record of a file edit — identifies itself with an internal `exec-<uuid>`
//!    sandbox id that appears nowhere else in the file, not with the `call_...`
//!    id of the tool call that applied it. Attribution is therefore positional:
//!    a patch event always falls between its tool call and that call's output,
//!    so it belongs to whichever response is open. Matching on `call_id`
//!    attributes nothing at all — 0 of 32 edits on the reference session.
//!
//! One more thing that is the format and not a bug: reasoning is encrypted.
//! All 89 reasoning items here carry an `encrypted_content` blob, an empty
//! `summary` and no `content`. Do not go looking for the missing text.

use super::raw::{ContentBlock, EventMsg, Line, RawLine, ResponseItem, Tokens};
use crate::model::*;
use crate::source::ParseStats;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::Path;

/// Wrappers the harness injects into user messages. Anything left after these
/// are removed is what a person actually typed.
const INJECTED_TAGS: [&str; 6] = [
    "INSTRUCTIONS",
    "environment_context",
    "user_instructions",
    "permissions instructions",
    "collaboration_mode",
    "system-reminder",
];

/// An `AGENTS.md` injection survives tag-stripping as its own heading, so it is
/// matched separately.
const AGENTS_HEADING: &str = "# AGENTS.md instructions for";

/// Rebuild one session from the lines of one rollout file.
pub fn normalize(path: &Path, lines: &mut dyn Iterator<Item = String>) -> (Session, ParseStats) {
    let mut stats = ParseStats::default();
    let mut b = Builder::new(path);

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        stats.lines += 1;
        match serde_json::from_str::<RawLine>(&line) {
            Ok(raw) => b.push(raw),
            Err(_) => stats.parse_errors += 1,
        }
    }
    (b.finish(), stats)
}

struct Builder {
    session: Session,
    /// Is a response currently open? Closed by the `token_count` that ends it.
    step_open: bool,
    /// `call_id` -> (turn, step, call), so an output can be attached in one pass.
    call_index: HashMap<String, (usize, usize, usize)>,
    /// The last cumulative total seen. This, not a sum, is the session total.
    running_total: Option<Tokens>,
    /// Models named by `turn_context`, most recent last.
    models: Vec<String>,
}

impl Builder {
    fn new(path: &Path) -> Self {
        Builder {
            session: Session {
                id: session_id_from_path(path),
                harness: Harness::Codex,
                project: String::new(),
                source_path: path.to_path_buf(),
                kind: SessionKind::Main,
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
            step_open: false,
            call_index: HashMap::new(),
            running_total: None,
            models: Vec::new(),
        }
    }

    fn push(&mut self, raw: RawLine) {
        let ts = parse_ts(raw.timestamp.as_deref());
        if let Some(ts) = ts {
            self.session.started_at.get_or_insert(ts);
            self.session.ended_at = Some(ts);
        }

        match raw.into_line() {
            Line::Meta(meta) => {
                if let Some(id) = meta.session_id.or(meta.id) {
                    self.session.id = id;
                }
                if let Some(cwd) = meta.cwd {
                    self.session.project = cwd;
                }
                if let Some(version) = meta.cli_version {
                    self.session.agent_version = Some(version);
                }
                if let Some(branch) = meta.git.and_then(|g| g.branch) {
                    self.session.branch = Some(branch);
                }
            }
            Line::Context(ctx) => {
                if self.session.project.is_empty() {
                    if let Some(cwd) = ctx.cwd {
                        self.session.project = cwd;
                    }
                }
                if let Some(model) = ctx.model {
                    self.models.push(model);
                }
            }
            Line::Item(item) => self.push_item(item, ts),
            Line::Event(event) => self.push_event(event, ts),
            Line::Other => {}
        }
    }

    fn push_item(&mut self, item: ResponseItem, ts: Option<DateTime<Utc>>) {
        match item {
            ResponseItem::Message(msg) => {
                let text: String = msg
                    .content
                    .iter()
                    .filter_map(ContentBlock::text)
                    .collect::<Vec<_>>()
                    .join("\n");
                match msg.role.as_deref() {
                    // Developer messages are harness instructions, never a turn.
                    Some("developer") | None => {}
                    Some("user") => self.push_human_text(&text, ts),
                    // Anything else is the model talking.
                    Some(_) => {
                        if !text.trim().is_empty() {
                            let step = self.open_step(None, ts);
                            if !step.text.is_empty() {
                                step.text.push('\n');
                            }
                            step.text.push_str(&text);
                            if let Some(id) = msg.id {
                                step.uuids.push(id.clone());
                                step.message_id.get_or_insert(id);
                            }
                        }
                    }
                }
            }
            // A reasoning item opens a response: it is the first thing the model
            // emits, ahead of any text or tool call. This is Codex's equivalent
            // of Claude's `requestId` — the boundary everything else folds into.
            ResponseItem::Reasoning(reasoning) => {
                self.close_step();
                let readable: String = reasoning
                    .summary
                    .iter()
                    .chain(reasoning.content.iter().flatten())
                    .map(|s| s.text.as_str())
                    .filter(|t| !t.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                let id = reasoning.id.clone();
                let step = self.open_step(id.clone(), ts);
                step.thinking = readable;
                if let Some(id) = id {
                    step.uuids.push(id);
                }
            }
            ResponseItem::CustomToolCall(call) => {
                let Some(call_id) = call.call_id.or(call.id) else {
                    return;
                };
                self.push_call(
                    call_id,
                    call.name.unwrap_or_else(|| "exec".into()),
                    call.input.unwrap_or(serde_json::Value::Null),
                    ts,
                );
            }
            ResponseItem::FunctionCall(call) => {
                let Some(call_id) = call.call_id.or(call.id) else {
                    return;
                };
                // `arguments` is a JSON object encoded as a string; keep the
                // decoded object where it decodes, the raw string where it does not.
                let input = call
                    .arguments
                    .as_deref()
                    .map(|a| {
                        serde_json::from_str(a)
                            .unwrap_or_else(|_| serde_json::Value::String(a.into()))
                    })
                    .unwrap_or(serde_json::Value::Null);
                self.push_call(
                    call_id,
                    call.name.unwrap_or_else(|| "call".into()),
                    input,
                    ts,
                );
            }
            ResponseItem::CustomToolCallOutput(out) | ResponseItem::FunctionCallOutput(out) => {
                if let Some(call_id) = out.call_id {
                    let text = out.output.as_ref().map(flatten_output);
                    self.attach_result(&call_id, text);
                }
            }
            ResponseItem::Other => {}
        }
    }

    fn push_event(&mut self, event: EventMsg, ts: Option<DateTime<Utc>>) {
        match event {
            // The response item carrying this same text is what we read; see
            // trap 3. Kept as a fallback for transcripts that have the event but
            // no item, which is what a `codex exec` run without history looks like.
            EventMsg::UserMessage { message } => {
                let already = self
                    .session
                    .turns
                    .last()
                    .and_then(|t| t.prompt.as_ref())
                    .is_some_and(|p| p.text.trim() == message.trim());
                if !already {
                    self.push_human_text(&message, ts);
                }
            }
            // Always a duplicate of a `message:assistant` item. Never read.
            EventMsg::AgentMessage { .. } => {}
            EventMsg::TokenCount { info } => {
                let Some(info) = info else { return };
                if let Some(total) = info.total_token_usage {
                    self.running_total = Some(total);
                }
                if let Some(last) = info.last_token_usage {
                    if let Some(step) = self.current_step() {
                        step.usage = convert(last);
                    }
                }
                // The count closes the response it belongs to.
                self.close_step();
            }
            EventMsg::TurnAborted { reason, .. } => {
                if reason.as_deref() != Some("replaced") {
                    if let Some(turn) = self.session.turns.last_mut() {
                        turn.interrupted = true;
                    }
                }
                self.close_step();
            }
            // The edit record. `changes` carries whole file contents, so only
            // the paths are taken — nothing here may reach the index.
            EventMsg::PatchApplyEnd {
                success, changes, ..
            } => {
                if !success {
                    return;
                }
                // Not `call_id`: see trap 5. The response that is open when the
                // patch lands is the one that applied it.
                let attribution = self.current_attribution();
                for path in changes.iter().flat_map(|c| c.keys()) {
                    self.session.file_touches.push(FileTouch {
                        path: path.clone(),
                        message_id: attribution.clone(),
                        timestamp: ts,
                    });
                }
            }
            EventMsg::TaskStarted { .. }
            | EventMsg::TaskComplete { .. }
            | EventMsg::ThreadRolledBack { .. }
            | EventMsg::Other => {}
        }
    }

    fn push_human_text(&mut self, text: &str, ts: Option<DateTime<Utc>>) {
        let cleaned = strip_injections(text);
        if cleaned.is_empty() {
            return;
        }
        let index = self.session.turns.len();
        self.session.turns.push(Turn {
            index,
            prompt: Some(Prompt {
                text: cleaned,
                timestamp: ts,
                uuid: None,
            }),
            steps: Vec::new(),
            interrupted: false,
        });
        self.step_open = false;
    }

    fn push_call(
        &mut self,
        call_id: String,
        name: String,
        input: serde_json::Value,
        ts: Option<DateTime<Utc>>,
    ) {
        let step = self.open_step(None, ts);
        // The call id is how `patch_apply_end` attributes a file edit back to
        // this response, so it has to be one of the step's ids.
        step.uuids.push(call_id.clone());
        let call_idx = step.tool_calls.len();
        step.tool_calls.push(ToolCall {
            id: call_id.clone(),
            name,
            input,
            result: None,
            is_error: false,
            denied: false,
        });
        let turn_idx = self.session.turns.len() - 1;
        let step_idx = self.session.turns[turn_idx].steps.len() - 1;
        self.call_index
            .insert(call_id, (turn_idx, step_idx, call_idx));
    }

    fn attach_result(&mut self, call_id: &str, text: Option<String>) {
        let Some(&(t, s, c)) = self.call_index.get(call_id) else {
            return;
        };
        if let Some(call) = self
            .session
            .turns
            .get_mut(t)
            .and_then(|turn| turn.steps.get_mut(s))
            .and_then(|step| step.tool_calls.get_mut(c))
        {
            call.is_error = text.as_deref().is_some_and(looks_like_failure);
            call.result = text;
        }
    }

    /// Work with no preceding prompt — replayed history, a resumed session —
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

    /// The response currently being folded, opening one if none is.
    fn open_step(&mut self, id: Option<String>, ts: Option<DateTime<Utc>>) -> &mut Step {
        if !self.step_open {
            let step = Step {
                response_id: id,
                message_id: None,
                uuids: Vec::new(),
                model: None,
                timestamp: ts,
                text: String::new(),
                thinking: String::new(),
                tool_calls: Vec::new(),
                usage: UsageTotals::default(),
            };
            self.current_turn().steps.push(step);
            self.step_open = true;
        }
        self.session
            .turns
            .last_mut()
            .expect("current_turn ensured one")
            .steps
            .last_mut()
            .expect("just pushed or already open")
    }

    fn current_step(&mut self) -> Option<&mut Step> {
        if !self.step_open {
            return None;
        }
        self.session.turns.last_mut()?.steps.last_mut()
    }

    fn close_step(&mut self) {
        self.step_open = false;
    }

    /// An id belonging to the response currently being folded, chosen so that
    /// [`Session::steps_by_uuid`] can resolve it: the tool call that is running,
    /// else any id the step already carries.
    fn current_attribution(&mut self) -> Option<String> {
        let step = self.session.turns.last()?.steps.last()?;
        step.tool_calls
            .last()
            .map(|call| call.id.clone())
            .or_else(|| step.uuids.first().cloned())
            .or_else(|| step.response_id.clone())
    }

    fn finish(mut self) -> Session {
        // The session total is the last cumulative figure, not a sum of the
        // per-response ones — see traps 1 and 2. Only when no `token_count`
        // event was recorded at all do we fall back to summing.
        self.session.usage = match self.running_total {
            Some(total) => convert(total),
            None => {
                let mut sum = UsageTotals::default();
                for step in self.session.turns.iter().flat_map(|t| t.steps.iter()) {
                    sum.add(step.usage);
                }
                sum
            }
        };
        // The model is named per turn; the busiest one describes the session.
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for model in &self.models {
            *counts.entry(model.as_str()).or_default() += 1;
        }
        self.session.model = counts
            .into_iter()
            .max_by_key(|&(name, count)| (count, std::cmp::Reverse(name)))
            .map(|(name, _)| name.to_string());
        let model = self.session.model.clone();
        for turn in &mut self.session.turns {
            for step in &mut turn.steps {
                step.model.clone_from(&model);
            }
        }
        if self.session.project.is_empty() {
            self.session.project = "(unknown)".to_string();
        }
        self.session
    }
}

/// Claude reports cache reads outside `input_tokens`; Codex reports them inside
/// it. The model uses Claude's arrangement — buckets that do not overlap — so
/// the cached portion is taken back out here. Without this, a Codex session's
/// input tokens read as several times a comparable Claude session's.
fn convert(t: Tokens) -> UsageTotals {
    UsageTotals {
        input: t.input_tokens.saturating_sub(t.cached_input_tokens),
        output: t.output_tokens,
        cache_creation: t.cache_write_input_tokens,
        cache_read: t.cached_input_tokens,
    }
}

/// Tool output is a list of content blocks, or a bare string.
fn flatten_output(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                serde_json::Value::String(s) => Some(s.clone()),
                other => other
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(str::to_string),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

/// Codex does not flag a failed tool call in the record, so the only signal is
/// the output itself. Deliberately conservative: a false negative costs a stat,
/// a false positive would make a working session look broken.
fn looks_like_failure(output: &str) -> bool {
    let head: String = output.chars().take(400).collect();
    head.contains("\"exit_code\":1")
        || head.contains("\"exit_code\": 1")
        || head.starts_with("error:")
        || head.starts_with("Error:")
}

/// Remove the harness's injected wrappers. What is left is what a person typed.
fn strip_injections(text: &str) -> String {
    let mut out = text.to_string();
    for tag in INJECTED_TAGS {
        out = strip_tag(&out, tag);
    }
    if out.trim_start().starts_with(AGENTS_HEADING) {
        return String::new();
    }
    out.trim().to_string()
}

/// Drop every `<tag>…</tag>` span, and an unclosed `<tag>` to end of text.
fn strip_tag(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find(&close) {
            Some(end) => &rest[start + end + close.len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn parse_ts(s: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s?)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Rollout files are named `rollout-<iso8601>-<uuid>.jsonl`, so the id is the
/// tail. Only a fallback: `session_meta` carries the real one.
fn session_id_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match stem.rsplit_once('-') {
        // A uuid's last group is 12 hex characters; anything shorter means the
        // name is not in the expected shape and is better used whole.
        Some((head, tail)) if tail.len() == 12 => {
            let uuid_start = head.len().saturating_sub(24);
            stem[uuid_start.min(head.len())..].to_string()
        }
        _ => stem,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_md_injection_is_not_a_prompt() {
        let injected = "# AGENTS.md instructions for /code/stock/rtrade\n\n<INSTRUCTIONS>\nread the docs\n</INSTRUCTIONS>";
        assert_eq!(strip_injections(injected), "");
    }

    #[test]
    fn a_real_prompt_survives_stripping() {
        assert_eq!(
            strip_injections("  fix the table layout  "),
            "fix the table layout"
        );
    }

    #[test]
    fn a_prompt_wrapped_in_a_preamble_keeps_only_what_was_typed() {
        let text = "<environment_context>\ncwd=/x\n</environment_context>\nfix the layout";
        assert_eq!(strip_injections(text), "fix the layout");
    }

    #[test]
    fn cached_input_is_taken_out_of_input() {
        // Codex: total == input + output, with cached inside input.
        let tokens = Tokens {
            input_tokens: 15129,
            cached_input_tokens: 4352,
            output_tokens: 132,
            total_tokens: 15261,
            ..Default::default()
        };
        let usage = convert(tokens);
        assert_eq!(usage.input, 15129 - 4352);
        assert_eq!(usage.cache_read, 4352);
        assert_eq!(usage.output, 132);
    }
}
