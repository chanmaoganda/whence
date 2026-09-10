//! The Oh My Pi format traps, each pinned by a fixture.
//!
//! Every constant here was measured against the reference corpus in
//! `~/.omp/agent/sessions`; the fixtures reproduce the shapes in miniature so
//! the tests run anywhere. If one of these fails, omp changed its format — read
//! `src/source/omp/normalize.rs` before "fixing" anything.

use whence::model::Harness;
use whence::source::{self, Resumable};

/// A transcript exercising every trap at once:
///
/// * a response whose `reasoningTokens` are counted inside `output`,
/// * a tool call written down twice, once as a block and once as a `custom`
///   record announcing that it started,
/// * a `toolResult` role that is neither the human nor the assistant,
/// * a harness notice wearing `attribution: "agent"`,
/// * a title that is superseded by a later `title_change`,
/// * an edit recorded only by its tool result's `resolvedPath`.
fn fixture() -> Vec<String> {
    [
        r##"{"type":"title","v":1,"title":"Old name","source":"auto","updatedAt":"2026-09-10T15:52:31.091Z","pad":"      "}"##,
        r##"{"type":"session","version":3,"id":"01a08c02-ffe5-76fd-9184-4470d76bd10b","timestamp":"2026-09-10T15:50:01.957Z","cwd":"/code/stock/rtrade-rewrite","title":"Old name","titleSource":"auto"}"##,
        r##"{"type":"thinking_level_change","id":"95776efc","parentId":null,"timestamp":"2026-09-10T15:50:14.322Z","thinkingLevel":"high","configured":null}"##,
        r##"{"type":"model_change","id":"17f198b4","parentId":"95776efc","timestamp":"2026-09-10T15:52:17.600Z","model":"deepseek/deepseek-v4-flash","role":"default","resolvedModelIsFallback":false}"##,
        r##"{"type":"model_change","id":"17f198b5","parentId":"17f198b4","timestamp":"2026-09-10T15:52:18.600Z","model":"deepseek/deepseek-smol","role":"smol","resolvedModelIsFallback":false}"##,
        r##"{"type":"message","id":"a1","parentId":"17f198b5","timestamp":"2026-09-10T15:52:20.000Z","message":{"role":"user","content":[{"type":"text","text":"add the config overlay"}],"attribution":"user","timestamp":1789055540000}}"##,
        r##"{"type":"title_change","id":"a2","parentId":"a1","timestamp":"2026-09-10T15:52:31.091Z","title":"Config overlay","source":"auto"}"##,
        r##"{"type":"message","id":"a3","parentId":"a2","timestamp":"2026-09-10T15:52:35.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"They want a control overlay written under /tmp.","thinkingSignature":"reasoning_content"},{"type":"text","text":"Writing it now."},{"type":"toolCall","id":"call_00_abc","name":"write","arguments":{"i":"Writing control overlay","path":"/tmp/omp-control.yml","content":"display:\n  showTurnTime: false\n"},"partialArgs":"{\"path\":","streamIndex":0,"intent":"Writing control overlay"}],"model":"deepseek-v4-flash","provider":"deepseek","api":"openai-completions","responseId":"f8f10feb-4105-4ef3-8446-110225af47a9","stopReason":"toolUse","usage":{"input":237,"output":1047,"cacheRead":19584,"cacheWrite":0,"totalTokens":20868,"reasoningTokens":850},"timestamp":1789055555000}}"##,
        r##"{"type":"custom","customType":"tool_execution_start","id":"a4","parentId":"a3","timestamp":"2026-09-10T15:52:36.000Z","data":{"toolCallId":"call_00_abc","toolName":"write","startedAt":"2026-09-10T15:52:36.000Z","args":{"path":"/tmp/omp-control.yml"},"intent":"Writing control overlay"}}"##,
        r##"{"type":"message","id":"a5","parentId":"a4","timestamp":"2026-09-10T15:52:37.000Z","message":{"role":"toolResult","toolCallId":"call_00_abc","toolName":"write","content":[{"type":"text","text":"Wrote 2 lines to /tmp/omp-control.yml"}],"details":{"resolvedPath":"/tmp/omp-control.yml"},"isError":false,"timestamp":1789055557000}}"##,
        r##"{"type":"custom_message","customType":"async-result","content":"<system-notice>\nBackground job bg_8 has completed. Resume your work using the result below.\n</system-notice>","display":true,"details":{"jobs":[]},"attribution":"agent","id":"a6","parentId":"a5","timestamp":"2026-09-10T15:52:40.000Z"}"##,
        r##"{"type":"reset_boundary","id":"a7","parentId":"a6","timestamp":"2026-09-10T15:52:45.000Z"}"##,
        r##"{"type":"message","id":"a8","parentId":"a7","timestamp":"2026-09-10T15:52:50.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Done."}],"model":"deepseek-v4-flash","provider":"deepseek","api":"openai-completions","responseId":"f88b1ee9-922b-45cf-8c0d-c86455d320fe","stopReason":"stop","usage":{"input":481,"output":370,"cacheRead":35968,"cacheWrite":12,"totalTokens":36819,"reasoningTokens":260},"timestamp":1789055560000}}"##,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

const PATH: &str =
    "/home/x/.omp/agent/sessions/--code-stock-rtrade-rewrite--/2026-09-10T15-50-01-957Z_01a08c02-ffe5-76fd-9184-4470d76bd10b.jsonl";

fn parse_lines(lines: Vec<String>) -> whence::model::Session {
    let mut iter = lines.into_iter();
    let (session, stats) =
        source::get(Harness::Omp).normalize(std::path::Path::new(PATH), &mut iter);
    assert_eq!(stats.parse_errors, 0, "the fixture must parse cleanly");
    session
}

fn parse() -> whence::model::Session {
    parse_lines(fixture())
}

/// Trap 1: `reasoningTokens` is billed inside `output`, not beside it. Adding
/// the two reads 2,527 output tokens here instead of 1,417 — the same mistake
/// costs 59% on the reference session.
#[test]
fn reasoning_tokens_are_counted_inside_output() {
    let session = parse();
    assert_eq!(session.usage.output, 1417);
    let naive = 1047 + 850 + 370 + 260;
    assert_ne!(session.usage.output, naive, "reasoning was added twice");
}

/// The buckets are already disjoint — `totalTokens == input + output +
/// cacheRead` on every response — so unlike Codex nothing has to be subtracted
/// back out of `input`.
#[test]
fn token_buckets_are_disjoint_as_recorded() {
    let session = parse();
    assert_eq!(session.usage.input, 718);
    assert_eq!(session.usage.cache_read, 55552);
    assert_eq!(session.usage.cache_creation, 12);
    // The identity the adapter relies on, spelled out for the first response.
    assert_eq!(237 + 1047 + 19584, 20868);
}

/// Trap 2: the assistant message records the call and a `custom` record of type
/// `tool_execution_start` records it again. Reading both doubles every tool
/// count in the corpus.
#[test]
fn a_tool_call_announced_twice_is_counted_once() {
    let session = parse();
    assert_eq!(session.tool_calls().count(), 1);
    let call = session.tool_calls().next().expect("one call");
    assert_eq!(call.name, "write");
    assert_eq!(call.id, "call_00_abc");
}

/// Trap 3: `toolResult` is a role of its own. It is neither a prompt nor a
/// response — it belongs to the call it answers.
#[test]
fn a_tool_result_is_attached_to_its_call_and_is_not_a_turn() {
    let session = parse();
    let call = session.tool_calls().next().expect("one call");
    assert_eq!(
        call.result.as_deref(),
        Some("Wrote 2 lines to /tmp/omp-control.yml")
    );
    assert!(!call.is_error);
    assert_eq!(session.turns.len(), 1, "a tool result opened a turn");
}

/// The thing omp does that no other harness whence reads does: it writes the
/// model's reasoning out in the clear. Both other adapters leave this empty
/// because the transcript only holds a signature and a ciphertext.
#[test]
fn thinking_survives_in_the_clear() {
    let session = parse();
    let step = session.steps().next().expect("a response");
    assert_eq!(
        step.thinking,
        "They want a control overlay written under /tmp."
    );
}

/// A `custom_message` is the harness talking to the model — a finished
/// background job, most often. It carries `attribution: "agent"` and reads
/// exactly like a turn.
#[test]
fn a_harness_notice_is_not_a_prompt() {
    let session = parse();
    assert_eq!(session.prompts().count(), 1);
    let prompt = session.prompts().next().expect("one prompt");
    assert_eq!(prompt.text, "add the config overlay");
}

/// The header is rewritten in place and a `title_change` is appended after it,
/// so the name at the top of the file is the stale one.
#[test]
fn the_last_title_on_disk_wins() {
    let session = parse();
    assert_eq!(session.title.as_deref(), Some("Config overlay"));
}

/// A response names its model bare and its provider separately; a
/// `model_change` names them together. The two must agree, or one session reads
/// as two models. The `smol` slot is not the model answering the conversation.
#[test]
fn the_model_is_qualified_by_its_provider() {
    let session = parse();
    assert_eq!(session.model.as_deref(), Some("deepseek/deepseek-v4-flash"));
}

/// `patch_apply_end` has no equivalent here: the only record of an edit is the
/// tool result, whose `resolvedPath` names the file the write actually landed
/// on. It has to resolve back to the response that asked for it.
#[test]
fn an_edit_is_attributed_to_the_response_that_made_it() {
    let session = parse();
    assert_eq!(session.file_touches.len(), 1);
    let touch = &session.file_touches[0];
    assert_eq!(touch.path, "/tmp/omp-control.yml");
    let by_uuid = session.steps_by_uuid();
    let id = touch.message_id.as_deref().expect("attributed");
    let (_, step) = by_uuid.get(id).expect("resolves to the response");
    assert_eq!(step.text, "Writing it now.");
}

/// Trap 4: the envelope's clock is RFC 3339 and the one inside a message counts
/// milliseconds. Reading the latter as seconds lands in the year 58699.
#[test]
fn the_clock_inside_a_message_counts_milliseconds() {
    let mut lines = fixture();
    // Same prompt, with the envelope's timestamp taken away so only the
    // millisecond clock is left to read.
    lines[5] = r##"{"type":"message","id":"a1","parentId":"17f198b5","message":{"role":"user","content":[{"type":"text","text":"add the config overlay"}],"attribution":"user","timestamp":1789055540000}}"##.to_string();
    let session = parse_lines(lines);
    let prompt = session.prompts().next().expect("one prompt");
    let ts = prompt.timestamp.expect("the millisecond clock was read");
    assert_eq!(ts.to_rfc3339(), "2026-09-10T15:52:20+00:00");
}

/// The session names itself; the filename is only the fallback.
#[test]
fn the_session_record_names_the_session() {
    let session = parse();
    assert_eq!(session.id, "01a08c02-ffe5-76fd-9184-4470d76bd10b");
    assert_eq!(session.project, "/code/stock/rtrade-rewrite");
    assert_eq!(session.harness, Harness::Omp);
    // A v7 uuid: the leading bits are a timestamp, so the tail identifies it.
    assert_eq!(session.short_id(), "d76bd10b");
}

/// A file truncated past its header still has to yield an id and a project,
/// both of which the path carries.
///
/// The project only approximately: omp builds the directory name by replacing
/// every separator with a dash, so a directory whose name already contains one
/// cannot be told from one more level of nesting. `rtrade-rewrite` comes back
/// as `rtrade/rewrite`, which is why this is the fallback and not the answer.
#[test]
fn a_headless_transcript_falls_back_to_its_path() {
    let session = parse_lines(vec![fixture()[7].clone()]);
    assert_eq!(session.id, "01a08c02-ffe5-76fd-9184-4470d76bd10b");
    assert_eq!(session.project, "/code/stock/rtrade/rewrite");
}

#[test]
fn resume_names_the_session_and_the_directory_it_ran_in() {
    let session = parse();
    let resume = source::resume(Harness::Omp, Resumable::from(&session)).expect("resumable");
    assert_eq!(
        resume.command,
        "omp --resume 01a08c02-ffe5-76fd-9184-4470d76bd10b"
    );
    assert_eq!(
        resume.pasteable(),
        "cd /code/stock/rtrade-rewrite && omp --resume 01a08c02-ffe5-76fd-9184-4470d76bd10b"
    );
}

/// A corrupt line is counted, never fatal: the file is still being appended to
/// while it is read.
#[test]
fn a_corrupt_line_does_not_abort_the_file() {
    let mut lines = fixture();
    lines.insert(6, "{\"type\":\"message\",\"id\":\"tr".to_string());
    let mut iter = lines.into_iter();
    let (session, stats) =
        source::get(Harness::Omp).normalize(std::path::Path::new(PATH), &mut iter);
    assert_eq!(stats.parse_errors, 1);
    assert_eq!(session.prompts().count(), 1);
    assert_eq!(session.usage.output, 1417);
}
