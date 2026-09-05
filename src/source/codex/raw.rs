//! Deserialization types that mirror Codex's on-disk rollout format exactly.
//! Nothing here interprets the data — see [`super::normalize`].
//!
//! A rollout file is an event log, not a message list. Every line is
//! `{timestamp, type, payload}`, and `payload` is a second tagged union whose
//! shape depends on `type`. The outer wrapper is read as a struct with a raw
//! `payload` rather than as an adjacently tagged enum, because serde cannot
//! flatten a `#[serde(tag, content)]` enum next to a sibling field like
//! `timestamp` — and because interpreting the payload separately means a
//! payload we do not recognise degrades to [`Line::Other`] instead of failing
//! the whole line.
//!
//! As with every adapter here, all of this is permissive: unknown types and
//! fields are ignored and almost everything is optional.

use serde::Deserialize;
use serde_json::Value;

/// One line of a rollout file, before its payload is interpreted.
#[derive(Debug, Deserialize)]
pub struct RawLine {
    pub timestamp: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
}

/// A line whose payload has been interpreted.
#[derive(Debug)]
pub enum Line {
    /// Opens the file: identity, cwd, versions.
    Meta(Box<SessionMeta>),
    /// Per-turn settings, and where the model name comes from.
    Context(TurnContext),
    /// A record from the model API's own item stream.
    Item(ResponseItem),
    /// A record from the harness's UI event stream.
    Event(EventMsg),
    /// `world_state` and anything the format grows later.
    Other,
}

impl RawLine {
    /// Interpret the payload according to `kind`. A payload that does not match
    /// its kind becomes [`Line::Other`] rather than an error: one malformed
    /// record must not cost us the rest of the session.
    pub fn into_line(self) -> Line {
        fn get<T: serde::de::DeserializeOwned>(v: Value) -> Option<T> {
            serde_json::from_value(v).ok()
        }
        match self.kind.as_str() {
            "session_meta" => get(self.payload).map(Line::Meta).unwrap_or(Line::Other),
            "turn_context" => get(self.payload).map(Line::Context).unwrap_or(Line::Other),
            "response_item" => get(self.payload).map(Line::Item).unwrap_or(Line::Other),
            "event_msg" => get(self.payload).map(Line::Event).unwrap_or(Line::Other),
            _ => Line::Other,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SessionMeta {
    pub session_id: Option<String>,
    pub id: Option<String>,
    pub timestamp: Option<String>,
    pub cwd: Option<String>,
    /// `codex-tui`, `codex-exec`, … — which front end wrote this.
    pub originator: Option<String>,
    pub cli_version: Option<String>,
    /// Set when this session was branched off another one.
    pub forked_from_id: Option<String>,
    pub git: Option<GitInfo>,
}

#[derive(Debug, Deserialize)]
pub struct GitInfo {
    pub branch: Option<String>,
    pub commit_hash: Option<String>,
    pub repository_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TurnContext {
    pub turn_id: Option<String>,
    pub cwd: Option<String>,
    /// The only place the model name appears in a rollout file.
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// The model API's item stream.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseItem {
    Message(Message),
    Reasoning(Reasoning),
    CustomToolCall(CustomToolCall),
    CustomToolCallOutput(ToolOutput),
    /// The plain Responses-API tool shape. Not produced by the model this
    /// corpus was recorded against, which uses `custom_tool_call`, but it is
    /// what Codex emits for most other models.
    FunctionCall(FunctionCall),
    FunctionCallOutput(ToolOutput),
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub id: Option<String>,
    /// `user`, `assistant` or `developer`. Developer messages are harness
    /// instructions and are never a human turn.
    pub role: Option<String>,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    /// `final_answer` on the reply that ends a turn.
    pub phase: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    InputText {
        #[serde(default)]
        text: String,
    },
    OutputText {
        #[serde(default)]
        text: String,
    },
    /// `input_image` and anything else: carried, not read.
    #[serde(other)]
    Other,
}

impl ContentBlock {
    pub fn text(&self) -> Option<&str> {
        match self {
            ContentBlock::InputText { text } | ContentBlock::OutputText { text } => Some(text),
            ContentBlock::Other => None,
        }
    }
}

/// A model reasoning item. In practice it carries only `encrypted_content`:
/// all 89 in the reference corpus have an empty `summary` and no `content`.
#[derive(Debug, Deserialize)]
pub struct Reasoning {
    pub id: Option<String>,
    #[serde(default)]
    pub summary: Vec<SummaryBlock>,
    #[serde(default)]
    pub content: Option<Vec<SummaryBlock>>,
    pub encrypted_content: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SummaryBlock {
    #[serde(default)]
    pub text: String,
}

/// Codex's freeform tool call: `input` is the script itself, as a string.
#[derive(Debug, Deserialize)]
pub struct CustomToolCall {
    pub id: Option<String>,
    /// What the output is matched back on. Not the same field as `id`.
    pub call_id: Option<String>,
    pub name: Option<String>,
    pub input: Option<Value>,
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FunctionCall {
    pub id: Option<String>,
    pub call_id: Option<String>,
    pub name: Option<String>,
    /// A JSON *string* holding the arguments object.
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ToolOutput {
    pub call_id: Option<String>,
    /// A list of content blocks for a custom tool, a bare string for a function.
    pub output: Option<Value>,
}

/// The harness's UI event stream.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMsg {
    /// What the human typed. Duplicates a `message:user` response item.
    UserMessage {
        #[serde(default)]
        message: String,
    },
    /// Duplicates a `message:assistant` response item; ignored on that account.
    AgentMessage {
        #[serde(default)]
        message: String,
    },
    /// Closes a model response and carries its token usage.
    TokenCount {
        info: Option<TokenInfo>,
    },
    TaskStarted {
        turn_id: Option<String>,
    },
    TaskComplete {
        turn_id: Option<String>,
    },
    /// The user interrupted.
    TurnAborted {
        turn_id: Option<String>,
        reason: Option<String>,
    },
    /// Where a file edit is recorded, with its full new content.
    PatchApplyEnd {
        call_id: Option<String>,
        turn_id: Option<String>,
        #[serde(default)]
        success: bool,
        /// Absolute path -> change. The change carries whole file contents, so
        /// nothing here may reach the index.
        changes: Option<serde_json::Map<String, Value>>,
    },
    /// The conversation was rewound; earlier turns were un-spent.
    ThreadRolledBack {
        num_turns: Option<u32>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct TokenInfo {
    /// Running total for the whole session so far — **cumulative**, not a delta.
    pub total_token_usage: Option<Tokens>,
    /// This response's own usage.
    pub last_token_usage: Option<Tokens>,
}

/// Codex's token counts.
///
/// Note the arithmetic differs from Claude's: here `total_tokens ==
/// input_tokens + output_tokens`, which means `input_tokens` *includes*
/// `cached_input_tokens`. Claude reports cache reads as a separate bucket
/// outside `input_tokens`. [`super::normalize`] converts to the latter.
#[derive(Debug, Deserialize, Default, Clone, Copy)]
pub struct Tokens {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub cache_write_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}
