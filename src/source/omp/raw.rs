//! Deserialization types that mirror Oh My Pi's on-disk
//! `~/.omp/agent/sessions/**/*.jsonl` format exactly. Nothing here interprets
//! the data — see [`super::normalize`].
//!
//! The shape is a flat record log with a parent pointer: every line carries an
//! `id` and the `parentId` of the line before it, so the file is a chain rather
//! than a tree. Records are appended while the session runs and the header is
//! rewritten in place, which is what the `pad` on [`TitleHeader`] is for.
//!
//! Permissive in the same way as the other adapters: unknown record types fall
//! through to [`RawRecord::Other`], unknown fields are ignored, and nothing is
//! required that a future release might stop writing.

use serde::Deserialize;
use serde_json::Value;

/// One line of a transcript file.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RawRecord {
    /// The header written when the session is created — and rewritten in place
    /// afterwards, which is why it is padded.
    Title(TitleHeader),
    /// The session envelope: id, cwd, and the title as of the last rewrite.
    Session(SessionRecord),
    /// A retitling. Later than the header on disk, so it wins.
    TitleChange(TitleChange),
    ModelChange(ModelChange),
    /// A prompt, a response, or a tool result — see [`Message`].
    Message(MessageRecord),
    /// A notice the harness injected into the conversation (a finished
    /// background job, most often). Carries `attribution: "agent"`: it reads
    /// like something a user said and is not.
    CustomMessage(CustomMessage),
    /// Where the context was reset. The conversation continues in the same
    /// file, but the model stopped being able to see what came before.
    ResetBoundary(Envelope),
    /// `tool_execution_start` — the same tool call the assistant message
    /// already recorded, announced a second time as it begins to run. Named
    /// here so that ignoring it is a decision rather than an oversight; see
    /// trap 2 in [`super::normalize`].
    Custom,
    /// `thinking_level_change` and whatever the format grows next.
    #[serde(other)]
    Other,
}

/// Fields every conversational record carries.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    pub id: Option<String>,
    pub parent_id: Option<String>,
    /// RFC 3339. Not to be confused with the epoch-millisecond `timestamp`
    /// inside a [`Message`].
    pub timestamp: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TitleHeader {
    pub title: Option<String>,
    pub source: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    pub id: Option<String>,
    /// Format version, not the version of omp itself. The transcript never
    /// names the agent's own version.
    pub version: Option<u32>,
    pub timestamp: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TitleChange {
    #[serde(flatten)]
    pub envelope: Envelope,
    pub title: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChange {
    #[serde(flatten)]
    pub envelope: Envelope,
    pub model: Option<String>,
    /// `default`, `smol`, `slow`, `plan` — which of the session's model slots
    /// changed. Only the default one is the model doing the work.
    pub role: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRecord {
    #[serde(flatten)]
    pub envelope: Envelope,
    pub message: Option<Message>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    pub custom_type: Option<String>,
    pub content: Option<String>,
    /// `agent` for harness notices. Never `user`.
    pub attribution: Option<String>,
}

/// A conversational message, keyed on a `role` that has three values, not two:
/// tool results are their own role rather than a block inside a user turn.
#[derive(Debug, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    Assistant(AssistantMessage),
    User(UserMessage),
    ToolResult(ToolResultMessage),
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    #[serde(default)]
    pub content: Vec<Block>,
    /// Both often `null`: the model that answered is named by the preceding
    /// `model_change` record rather than by the response itself.
    pub model: Option<String>,
    pub provider: Option<String>,
    pub response_id: Option<String>,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
    /// Epoch milliseconds.
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    #[serde(default)]
    pub content: Vec<Block>,
    /// `user` for something a human typed. Present so that a harness-injected
    /// message can be told from a real one without guessing at its text.
    pub attribution: Option<String>,
    /// Typed while the agent was still working, to redirect it mid-flight.
    #[serde(default)]
    pub steering: bool,
    /// Epoch milliseconds.
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    /// Blocks, as for a message, but images and other payloads appear here too.
    pub content: Option<Value>,
    /// Tool-specific metadata. `resolvedPath` is the one field this adapter
    /// reads: it is the only record of which file an edit landed on.
    pub details: Option<Value>,
    #[serde(default)]
    pub is_error: bool,
    /// The harness judged the result not worth keeping in context. Recorded on
    /// disk, not modelled here.
    #[serde(default)]
    pub useless: bool,
    /// Epoch milliseconds.
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Block {
    Text {
        #[serde(default)]
        text: String,
    },
    /// Unlike every other harness whence reads, this arrives in the clear.
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    ToolCall {
        id: String,
        name: String,
        #[serde(default)]
        arguments: Value,
    },
    #[serde(other)]
    Other,
}

/// Token counts for one response, and only that response — omp neither repeats
/// them across lines the way Claude Code does nor accumulates them the way
/// Codex does.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    /// Reasoning tokens are billed *inside* `output`, not beside it —
    /// `totalTokens == input + output + cacheRead` holds on every response in
    /// the corpus. Deserialized so that the trap is visible in the type, and
    /// deliberately never added to anything.
    #[serde(default)]
    pub reasoning_tokens: u64,
}

impl RawRecord {
    pub fn envelope(&self) -> Option<&Envelope> {
        match self {
            RawRecord::Message(r) => Some(&r.envelope),
            RawRecord::CustomMessage(r) => Some(&r.envelope),
            RawRecord::TitleChange(r) => Some(&r.envelope),
            RawRecord::ModelChange(r) => Some(&r.envelope),
            RawRecord::ResetBoundary(e) => Some(e),
            _ => None,
        }
    }
}
