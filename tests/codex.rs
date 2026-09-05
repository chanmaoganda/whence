//! The Codex format traps, each pinned by a fixture.
//!
//! Every constant here was measured against the reference corpus in
//! `~/.codex/sessions`; the fixtures reproduce the shapes in miniature so the
//! tests run anywhere. If one of these fails, Codex changed its format — read
//! `src/source/codex/normalize.rs` before "fixing" anything.

use whence::model::Harness;
use whence::source;

/// A rollout file exercising all five traps at once:
///
/// * two `token_count` events whose `total_token_usage` accumulates,
/// * a prompt recorded as both a response item and an event,
/// * an `AGENTS.md` preamble arriving as a user message,
/// * a `patch_apply_end` whose `call_id` matches no tool call,
/// * a reasoning item with nothing readable in it.
fn fixture() -> Vec<String> {
    [
        r##"{"timestamp":"2026-07-11T01:28:09.083Z","type":"session_meta","payload":{"session_id":"019f4eca-901b-7d91-9f65-cda91498aa04","cwd":"/code/stock/rtrade","cli_version":"0.144.1","git":{"branch":"master"}}}"##,
        r##"{"timestamp":"2026-07-11T01:28:09.084Z","type":"turn_context","payload":{"turn_id":"t1","model":"gpt-5.6-sol","cwd":"/code/stock/rtrade"}}"##,
        r##"{"timestamp":"2026-07-11T01:28:09.085Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions instructions>\nno sandbox\n</permissions instructions>"}]}}"##,
        r##"{"timestamp":"2026-07-11T01:28:09.086Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /code/stock/rtrade\n\n<INSTRUCTIONS>\nread the docs first\n</INSTRUCTIONS>"}]}}"##,
        r##"{"timestamp":"2026-07-11T01:28:10.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix the table layout"}]}}"##,
        r##"{"timestamp":"2026-07-11T01:28:10.001Z","type":"event_msg","payload":{"type":"user_message","message":"fix the table layout","images":[]}}"##,
        r##"{"timestamp":"2026-07-11T01:28:11.000Z","type":"response_item","payload":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"gAAAAABqUZxTzXsoXzZj"}}"##,
        r##"{"timestamp":"2026-07-11T01:28:12.000Z","type":"event_msg","payload":{"type":"agent_message","message":"On it."}}"##,
        r##"{"timestamp":"2026-07-11T01:28:12.001Z","type":"response_item","payload":{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"On it."}],"phase":"final_answer"}}"##,
        r##"{"timestamp":"2026-07-11T01:28:13.000Z","type":"response_item","payload":{"type":"custom_tool_call","id":"ctc_1","call_id":"call_abc","name":"exec","input":"apply_patch << 'EOF'"}}"##,
        r##"{"timestamp":"2026-07-11T01:28:14.000Z","type":"event_msg","payload":{"type":"patch_apply_end","call_id":"exec-ebd11c93-6992-4d0f","turn_id":"t1","success":true,"changes":{"/code/stock/rtrade/src/app.rs":{"type":"add","content":"fn main() {}"}}}}"##,
        r##"{"timestamp":"2026-07-11T01:28:15.000Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_abc","output":[{"type":"input_text","text":"Success. Updated the following files:\nA src/app.rs"}]}}"##,
        r##"{"timestamp":"2026-07-11T01:28:16.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":15129,"cached_input_tokens":4352,"cache_write_input_tokens":0,"output_tokens":324,"total_tokens":15453},"last_token_usage":{"input_tokens":15129,"cached_input_tokens":4352,"cache_write_input_tokens":0,"output_tokens":324,"total_tokens":15453}}}}"##,
        r##"{"timestamp":"2026-07-11T01:28:17.000Z","type":"response_item","payload":{"type":"reasoning","id":"rs_2","summary":[],"encrypted_content":"gAAAAABqUZxTzXsoXzZk"}}"##,
        r##"{"timestamp":"2026-07-11T01:28:18.000Z","type":"response_item","payload":{"type":"message","id":"msg_2","role":"assistant","content":[{"type":"output_text","text":"Done."}],"phase":"final_answer"}}"##,
        r##"{"timestamp":"2026-07-11T01:28:19.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":30000,"cached_input_tokens":10000,"cache_write_input_tokens":0,"output_tokens":583,"total_tokens":30583},"last_token_usage":{"input_tokens":14871,"cached_input_tokens":5648,"cache_write_input_tokens":0,"output_tokens":259,"total_tokens":15130}}}}"##,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn parse() -> whence::model::Session {
    let lines = fixture();
    let mut iter = lines.into_iter();
    let (session, stats) = source::get(Harness::Codex).normalize(
        std::path::Path::new(
            "rollout-2026-07-11T09-28-45-019f4eca-901b-7d91-9f65-cda91498aa04.jsonl",
        ),
        &mut iter,
    );
    assert_eq!(stats.parse_errors, 0, "the fixture must parse cleanly");
    session
}

/// Trap 1: `total_token_usage` accumulates, so summing it double-counts every
/// response that came before. Here that reads 907 output tokens instead of 583.
#[test]
fn session_tokens_come_from_the_last_cumulative_total() {
    let session = parse();
    assert_eq!(session.usage.output, 583);

    // What a reader who summed the cumulative field would get.
    let naive = 324 + 583;
    assert_ne!(session.usage.output, naive);
}

/// Trap 2: summing the *per-response* counts is nearly right, and wrong exactly
/// where a turn was rolled back — its tokens were spent, then un-spent, so they
/// appear in a `last_token_usage` but never reach the running total.
///
/// On the reference corpus this is the single session containing
/// `thread_rolled_back`: 37,280 summed against 36,451 actually charged.
#[test]
fn a_rolled_back_turn_is_not_charged_to_the_session() {
    let mut lines = fixture();
    lines.push(
        r##"{"timestamp":"2026-07-11T01:28:20.000Z","type":"response_item","payload":{"type":"reasoning","id":"rs_3","summary":[],"encrypted_content":"x"}}"##
            .to_string(),
    );
    // A response is charged...
    lines.push(
        r##"{"timestamp":"2026-07-11T01:28:21.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":30000,"cached_input_tokens":10000,"output_tokens":583,"total_tokens":30583},"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":200,"total_tokens":300}}}}"##
            .to_string(),
    );
    // ...and then rolled back, leaving the running total where it was.
    lines.push(
        r##"{"timestamp":"2026-07-11T01:28:22.000Z","type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}"##
            .to_string(),
    );
    let mut iter = lines.into_iter();
    let (session, _) = source::get(Harness::Codex)
        .normalize(std::path::Path::new("rollout-x-abc.jsonl"), &mut iter);

    let summed: u64 = session.steps().map(|s| s.usage.output).sum();
    assert_eq!(
        summed,
        324 + 259 + 200,
        "the spent tokens are still recorded"
    );
    assert_eq!(
        session.usage.output, 583,
        "but the session is charged only what the running total says"
    );
}

/// Trap 2: per-response counts stay per-response, for attribution only.
#[test]
fn each_response_keeps_its_own_usage() {
    let session = parse();
    let per_step: Vec<u64> = session.steps().map(|s| s.usage.output).collect();
    assert_eq!(per_step, vec![324, 259]);
}

/// Codex counts cached tokens inside `input_tokens`; the model keeps the
/// buckets disjoint, the way Claude reports them.
#[test]
fn cached_tokens_are_taken_out_of_input() {
    let session = parse();
    assert_eq!(session.usage.input, 30000 - 10000);
    assert_eq!(session.usage.cache_read, 10000);
}

/// Trap 3: the prompt is recorded twice. It must be counted once.
#[test]
fn a_prompt_recorded_twice_becomes_one_turn() {
    let session = parse();
    let prompts: Vec<&str> = session.prompts().map(|p| p.text.as_str()).collect();
    assert_eq!(prompts, vec!["fix the table layout"]);
}

/// Trap 3, the other direction: `agent_message` duplicates the response item.
#[test]
fn a_reply_recorded_twice_is_not_doubled() {
    let session = parse();
    let text: Vec<&str> = session.steps().map(|s| s.text.as_str()).collect();
    assert_eq!(text, vec!["On it.", "Done."]);
}

/// Trap 4: harness preambles arrive as user messages and are not prompts.
#[test]
fn injected_preambles_are_not_prompts() {
    let session = parse();
    assert_eq!(session.prompts().count(), 1);
    let all: String = session.prompts().map(|p| p.text.clone()).collect();
    assert!(!all.contains("AGENTS.md"));
    assert!(!all.contains("read the docs first"));
}

/// Trap 5: a patch's `call_id` matches no tool call, so attribution is
/// positional. Matching on `call_id` would attribute nothing.
#[test]
fn a_file_edit_resolves_back_to_the_response_that_made_it() {
    let session = parse();
    assert_eq!(session.file_touches.len(), 1);
    let touch = &session.file_touches[0];
    assert_eq!(touch.path, "/code/stock/rtrade/src/app.rs");

    let by_uuid = session.steps_by_uuid();
    let id = touch
        .message_id
        .as_deref()
        .expect("edit must be attributed");
    let (turn, step) = by_uuid.get(id).expect("attribution must resolve to a step");
    assert_eq!(turn.index, 0);
    assert_eq!(step.text, "On it.");
    assert_eq!(step.tool_calls[0].name, "exec");
}

/// Reasoning is encrypted on disk. That is the format, not a parser bug.
#[test]
fn encrypted_reasoning_yields_no_text() {
    let session = parse();
    assert!(session.steps().all(|s| s.thinking.is_empty()));
}

#[test]
fn session_metadata_is_read() {
    let session = parse();
    assert_eq!(session.harness, Harness::Codex);
    assert_eq!(session.id, "019f4eca-901b-7d91-9f65-cda91498aa04");
    assert_eq!(session.project, "/code/stock/rtrade");
    assert_eq!(session.branch.as_deref(), Some("master"));
    assert_eq!(session.agent_version.as_deref(), Some("0.144.1"));
    assert_eq!(session.model.as_deref(), Some("gpt-5.6-sol"));
}

/// A tool call and its output are matched on `call_id`, which is not `id`.
#[test]
fn tool_output_is_matched_back_to_its_call() {
    let session = parse();
    let call = session.tool_calls().next().expect("one call");
    assert_eq!(call.id, "call_abc");
    assert!(call.result.as_deref().unwrap().contains("Success."));
    assert!(!call.is_error);
}

/// A corrupt line is counted, never fatal.
#[test]
fn a_corrupt_line_does_not_lose_the_session() {
    let mut lines = fixture();
    lines.insert(5, "{ this is not json".to_string());
    let mut iter = lines.into_iter();
    let (session, stats) = source::get(Harness::Codex)
        .normalize(std::path::Path::new("rollout-x-abc.jsonl"), &mut iter);
    assert_eq!(stats.parse_errors, 1);
    assert_eq!(session.prompts().count(), 1, "the rest still parses");
}
