//! Deserialization types that mirror Claude Code's on-disk
//! `~/.claude/projects/**/*.jsonl` format exactly. Nothing here interprets the
//! data — see [`super::normalize`].
//!
//! The format is undocumented and changes between Claude Code releases, so every
//! type here is deliberately permissive: unknown fields are ignored, unknown
//! record types fall through to [`RawRecord::Other`], and almost everything is
//! optional. A parse error on one line must never take down the whole index.

use serde::Deserialize;
use serde_json::Value;

/// One line of a transcript file.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum RawRecord {
    #[serde(rename = "user")]
    User(UserRecord),
    #[serde(rename = "assistant")]
    Assistant(AssistantRecord),
    /// Emitted when a tracked file changes; carries the message that caused it.
    #[serde(rename = "file-history-delta")]
    FileHistoryDelta(FileHistoryDelta),
    #[serde(rename = "ai-title")]
    AiTitle(AiTitle),
    #[serde(rename = "system")]
    System(SystemRecord),
    /// `attachment`, `mode`, `permission-mode`, `last-prompt`, `queue-operation`,
    /// `atis-latch`, `cost-state`, … — carried in the file but not indexed.
    #[serde(other)]
    Other,
}

/// Fields that appear on most conversational records.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub session_id: Option<String>,
    pub timestamp: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    #[serde(default)]
    pub is_sidechain: bool,
    pub version: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRecord {
    #[serde(flatten)]
    pub envelope: Envelope,
    pub message: Option<UserMessage>,
    /// Present when this record carries the result of a tool call.
    pub tool_use_result: Option<Value>,
    /// Set on synthetic records (hook output, command expansion) — not a human turn.
    #[serde(default)]
    pub is_meta: bool,
    /// Set when the user hit Ctrl-C on the assistant message with this id.
    pub interrupted_message_id: Option<String>,
    /// Set when the user rejected a tool call.
    pub tool_denial_kind: Option<String>,
    pub user_feedback: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct UserMessage {
    #[serde(default)]
    pub content: Content,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantRecord {
    #[serde(flatten)]
    pub envelope: Envelope,
    /// Identifies the API response. One response is split across several lines,
    /// all sharing this id — the single most important field in the format.
    pub request_id: Option<String>,
    pub message: Option<AssistantMessage>,
    pub effort: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AssistantMessage {
    pub id: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub content: Vec<Block>,
    pub usage: Option<Usage>,
    pub stop_reason: Option<String>,
}

/// A user message's content is either a bare string (what you typed) or a list
/// of blocks (tool results, images, or text with attachments).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<Block>),
    /// Anything else the format grows later; carried but not interpreted.
    Other(Value),
}

impl Default for Content {
    fn default() -> Self {
        Content::Other(Value::Null)
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        #[serde(default)]
        text: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Option<Value>,
        #[serde(default)]
        is_error: bool,
    },
    #[serde(other)]
    Other,
}

/// Token counts for one API response. Note these are repeated identically on
/// every line of the response, so they must be counted once per `request_id`.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileHistoryDelta {
    /// Path relative to the session cwd.
    pub tracking_path: String,
    /// The assistant message that produced this edit — the "why" link.
    pub message_id: Option<String>,
    pub snapshot_message_id: Option<String>,
    pub timestamp: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiTitle {
    pub ai_title: Option<String>,
    pub leaf_uuid: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemRecord {
    pub subtype: Option<String>,
    pub duration_ms: Option<u64>,
    pub content: Option<Value>,
    #[serde(flatten)]
    pub envelope: Envelope,
}

impl RawRecord {
    pub fn envelope(&self) -> Option<&Envelope> {
        match self {
            RawRecord::User(r) => Some(&r.envelope),
            RawRecord::Assistant(r) => Some(&r.envelope),
            RawRecord::System(r) => Some(&r.envelope),
            _ => None,
        }
    }
}
