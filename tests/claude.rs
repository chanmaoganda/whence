//! The Claude Code format traps, each pinned by a fixture.
//!
//! Each case encodes a mistake a naive reader would make. If one fails, Claude
//! Code changed its format — read `src/source/claude/normalize.rs` first.

use std::path::Path;
use whence::model::{Harness, SessionKind};
use whence::source::{self, Resumable};

fn run(lines: &[&str]) -> whence::model::Session {
    let path = Path::new("/tmp/projects/-code-demo/abc123.jsonl");
    let mut iter = lines.iter().map(|s| s.to_string());
    let (session, _) = source::get(Harness::Claude).normalize(path, &mut iter);
    session
}

/// One API response is written as several lines sharing a `requestId`, each
/// repeating the same `usage`. Counting per line inflates tokens badly.
#[test]
fn folds_one_response_spanning_many_lines() {
    let session = run(&[
        r#"{"type":"user","sessionId":"s","timestamp":"2026-08-08T05:20:00Z","cwd":"/code/demo","message":{"content":"重构这个模块"}}"#,
        r#"{"type":"assistant","requestId":"req_1","message":{"id":"msg_1","model":"claude-opus-5","content":[{"type":"text","text":"先看代码"}],"usage":{"output_tokens":100,"input_tokens":5}}}"#,
        r#"{"type":"assistant","requestId":"req_1","message":{"id":"msg_1","model":"claude-opus-5","content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"a.rs"}}],"usage":{"output_tokens":100,"input_tokens":5}}}"#,
        r#"{"type":"assistant","requestId":"req_1","message":{"id":"msg_1","model":"claude-opus-5","content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"ls"}}],"usage":{"output_tokens":100,"input_tokens":5}}}"#,
    ]);

    assert_eq!(session.steps().count(), 1, "three lines are one response");
    assert_eq!(
        session.usage.output, 100,
        "usage counted once, not three times"
    );
    assert_eq!(
        session.tool_calls().count(),
        2,
        "blocks from every line are kept"
    );
    assert_eq!(session.steps().next().unwrap().text, "先看代码");
}

/// Most `type: "user"` records are tool results, not something a human typed.
#[test]
fn tool_results_are_not_human_prompts() {
    let session = run(&[
        r#"{"type":"user","message":{"content":"跑一下测试"}}"#,
        r#"{"type":"assistant","requestId":"r1","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}],"usage":{"output_tokens":10}}}"#,
        r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"test result: ok","is_error":false}]}}"#,
    ]);

    assert_eq!(session.prompts().count(), 1, "only the real prompt counts");
    let call = session.tool_calls().next().unwrap();
    assert_eq!(call.result.as_deref(), Some("test result: ok"));
    assert!(!call.is_error);
}

#[test]
fn tool_result_blocks_are_flattened() {
    let session = run(&[
        r#"{"type":"assistant","requestId":"r1","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}],"usage":{"output_tokens":1}}}"#,
        r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"line one"},{"type":"text","text":"line two"}],"is_error":true}]}}"#,
    ]);

    let call = session.tool_calls().next().unwrap();
    assert_eq!(call.result.as_deref(), Some("line one\nline two"));
    assert!(call.is_error, "errored calls are the interesting ones");
}

/// Interrupts mark where Claude went off the rails — keep them, and do not let
/// the marker masquerade as a new prompt.
#[test]
fn interrupts_attach_to_the_turn_they_broke() {
    let session = run(&[
        r#"{"type":"user","message":{"content":"接着完成B14c"}}"#,
        r#"{"type":"assistant","requestId":"r1","message":{"id":"m1","content":[{"type":"text","text":"好的"}],"usage":{"output_tokens":5}}}"#,
        r#"{"type":"user","message":{"content":"[Request interrupted by user]"}}"#,
        r#"{"type":"user","message":{"content":"换个方向"}}"#,
    ]);

    assert_eq!(session.prompts().count(), 2, "the marker is not a prompt");
    assert!(session.turns[0].interrupted);
    assert!(!session.turns[1].interrupted);
}

/// `file-history-delta` links an edit to the message that caused it. This is
/// the attribution `git blame` cannot provide — but the link is by *line*
/// `uuid`, not by `message.id`, and one response spans many lines. Measured on
/// the local corpus: 154 of 154 deltas resolve through `uuid`, 0 through
/// `message.id`, so folding must keep every line's uuid.
#[test]
fn file_edits_are_attributed_to_the_line_that_caused_them() {
    let session = run(&[
        r#"{"type":"user","message":{"content":"改一下 error.rs"}}"#,
        r#"{"type":"assistant","uuid":"u1","requestId":"r1","message":{"id":"m1","content":[{"type":"text","text":"改好了"}],"usage":{"output_tokens":5}}}"#,
        r#"{"type":"assistant","uuid":"u2","requestId":"r1","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{}}],"usage":{"output_tokens":5}}}"#,
        r#"{"type":"file-history-delta","trackingPath":"src/error.rs","messageId":"u2","timestamp":"2026-08-08T05:21:00Z"}"#,
    ]);

    assert_eq!(session.file_touches.len(), 1);
    let touch = &session.file_touches[0];
    assert_eq!(touch.path, "src/error.rs");
    assert_eq!(
        touch.message_id.as_deref(),
        Some("u2"),
        "the delta names a line uuid, not msg_…"
    );

    let by_uuid = session.steps_by_uuid();
    let (turn, step) = by_uuid[touch.message_id.as_deref().unwrap()];
    assert_eq!(
        step.text, "改好了",
        "the edit traces back to the whole folded response, not just its last line"
    );
    assert_eq!(
        turn.prompt.as_ref().unwrap().text,
        "改一下 error.rs",
        "and further back to what was asked for"
    );
    assert_eq!(
        session.steps().count(),
        1,
        "both lines still fold into one response"
    );
}

/// Harness-injected reminders are not user words and must not reach the index.
#[test]
fn strips_system_reminders_and_meta_records() {
    let session = run(&[
        r#"{"type":"user","message":{"content":"真正的问题<system-reminder>忽略我</system-reminder>"}}"#,
        r#"{"type":"user","isMeta":true,"message":{"content":"hook output"}}"#,
        r#"{"type":"user","message":{"content":"<system-reminder>只有噪音</system-reminder>"}}"#,
    ]);

    assert_eq!(session.prompts().count(), 1);
    assert_eq!(session.prompts().next().unwrap().text, "真正的问题");
}

#[test]
fn reads_session_identity_from_records_and_path() {
    let session = run(&[
        r#"{"type":"user","sessionId":"s1","cwd":"/code/stock/rtrade","gitBranch":"main","version":"2.1.0","timestamp":"2026-08-08T05:20:00Z","message":{"content":"hi"}}"#,
        r#"{"type":"ai-title","aiTitle":"重构分析模块"}"#,
    ]);

    assert_eq!(session.id, "abc123", "id comes from the filename");
    assert_eq!(session.project, "/code/stock/rtrade");
    assert_eq!(session.branch.as_deref(), Some("main"));
    assert_eq!(session.title.as_deref(), Some("重构分析模块"));
    assert_eq!(session.kind, SessionKind::Main);
}

#[test]
fn subagent_transcripts_are_labelled() {
    let path = Path::new("/tmp/projects/-code-demo/parent/subagents/agent-abc.jsonl");
    let (session, _) = source::get(Harness::Claude).normalize(path, &mut std::iter::empty());
    assert_eq!(
        session.kind,
        SessionKind::Subagent {
            agent: "agent-abc".into()
        }
    );
}

/// The way back out of whence: the whole id, in the directory Claude Code
/// keeps that session under. Eight characters name a session here and nowhere
/// else, and `--resume` looks only in the project it is run from.
#[test]
fn resumes_a_session_by_its_whole_id_in_its_own_project() {
    let session = run(&[
        r#"{"type":"user","sessionId":"s1","cwd":"/code/stock/rtrade","message":{"content":"hi"}}"#,
    ]);
    let command = source::resume(Harness::Claude, Resumable::from(&session))
        .expect("resumable")
        .pasteable();
    assert_eq!(
        command, "cd /code/stock/rtrade && claude --resume abc123",
        "the id in full, run where the session ran"
    );
}

/// A subagent was never a session you drove, so `--resume` will not take its
/// id. The conversation that spawned it is the directory the `subagents/`
/// folder sits in — and is what you wanted to reopen anyway.
#[test]
fn a_subagent_resumes_as_the_session_that_spawned_it() {
    let parent = "0750255e-3156-45a3-9670-8501b4421ca0";
    let path = format!("/tmp/projects/-code-demo/{parent}/subagents/agent-abc.jsonl");
    let session = Resumable {
        id: "agent-abc",
        project: "/code/demo",
        path: Path::new(&path),
    };
    assert_eq!(
        source::resume(Harness::Claude, session)
            .expect("resumable")
            .pasteable(),
        format!("cd /code/demo && claude --resume {parent}")
    );

    // A subagent of a subagent: only the outermost id is a session.
    let nested = format!("/tmp/projects/-code-demo/{parent}/subagents/sub/subagents/deep.jsonl");
    let session = Resumable {
        id: "deep",
        project: "/code/demo",
        path: Path::new(&nested),
    };
    assert_eq!(
        source::resume(Harness::Claude, session)
            .expect("resumable")
            .pasteable(),
        format!("cd /code/demo && claude --resume {parent}")
    );
}

/// A directory with a space in it is ordinary, and a command you cannot paste
/// is not an answer.
#[test]
fn quotes_a_project_the_shell_would_cut_up() {
    let session = Resumable {
        id: "abc123",
        project: "/Users/me/My Code/whence",
        path: Path::new("/tmp/projects/x/abc123.jsonl"),
    };
    assert_eq!(
        source::resume(Harness::Claude, session)
            .expect("resumable")
            .pasteable(),
        "cd '/Users/me/My Code/whence' && claude --resume abc123"
    );
}

/// A malformed line must never abort the file.
#[test]
fn survives_corrupt_lines() {
    let session = run(&[
        r#"{"type":"user","message":{"content":"第一句"}}"#,
        r#"{not json at all"#,
        r#"{"type":"totally-new-record-type","whatever":1}"#,
        r#"{"type":"user","message":{"content":"第二句"}}"#,
    ]);
    assert_eq!(session.prompts().count(), 2);
}
