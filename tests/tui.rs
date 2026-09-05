//! The browser: how text is broken to fit, and what the keys do to the state.
//!
//! Nothing here opens a terminal. `App` is driven with the events crossterm
//! would have produced, and the drawing is checked against a `TestBackend`, so
//! a regression shows up as a failing assertion rather than as a screen someone
//! has to look at.

use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use unicode_width::UnicodeWidthStr;
use whence::index::SearchIndex;
use whence::model::Harness;
use whence::render::renderer;
use whence::search::Query;
use whence::source::{Root, Transcript};
use whence::tui::{ui, App, View};

// ---- line breaking -------------------------------------------------------

fn lines_of(source: &str, width: u16) -> Vec<String> {
    ui::wrap(&renderer().parse(source), width)
        .iter()
        .map(|line| line.to_string())
        .collect()
}

/// Chinese prose has no spaces in it. A wrapper that only breaks on whitespace
/// emits one enormous line and shows you the first screenful of it.
#[test]
fn chinese_breaks_mid_run_and_measures_columns() {
    // 20 characters, so 40 columns — more than three times the width given.
    let text = "重构索引以后再看看这个模块的实现细节和边界";
    let lines = lines_of(text, 12);

    assert!(
        lines.len() >= 4,
        "one line per 12 columns, not per 12 chars"
    );
    for line in &lines {
        assert!(
            line.width() <= 12,
            "{line:?} is {} columns wide",
            line.width()
        );
    }
    assert_eq!(
        lines.concat(),
        text,
        "every character survives the break, in order"
    );
}

/// Latin script is the other half of the same rule: a typo-free word is worth
/// more than a full line, so the break goes at the space.
#[test]
fn latin_breaks_at_spaces() {
    let lines = lines_of("the searcher folds one index across every harness", 20);
    for line in &lines {
        assert!(line.width() <= 20, "{line:?}");
        assert!(!line.starts_with(' ') && !line.ends_with(' '), "{line:?}");
    }
    assert!(
        lines.iter().all(|l| !l.contains("harnes\n")),
        "words are not split"
    );
    assert_eq!(
        lines.join(" "),
        "the searcher folds one index across every harness"
    );
}

/// A reflowed shell command is no longer a command you can copy and run.
#[test]
fn code_blocks_are_never_reflowed() {
    let source = "run this:\n\n```sh\ncargo test --all-targets -- --nocapture\nls\n```\n";
    let lines = lines_of(source, 24);

    let code: Vec<&String> = lines.iter().filter(|l| l.starts_with('│')).collect();
    assert_eq!(code.len(), 2, "two source lines stay two display lines");
    assert!(
        code[0].ends_with('…'),
        "an over-wide code line is clipped, not wrapped: {:?}",
        code[0]
    );
    assert_eq!(code[1], "│ ls");
}

#[test]
fn a_tab_gets_a_width_before_anything_measures_it() {
    let lines = lines_of("```\n\tindented\n```", 40);
    assert_eq!(lines[0], "│     indented");
}

// ---- the application -----------------------------------------------------

const SESSION: &str = "aaaaaaaa-1111-4111-8111-111111111111";

fn transcript() -> Vec<String> {
    let head = format!(r#""sessionId":"{SESSION}","cwd":"/code/demo""#);
    vec![
        format!(
            r#"{{"type":"user",{head},"timestamp":"2026-08-08T05:20:00Z","message":{{"content":"用 tantivy 重建索引"}}}}"#
        ),
        format!(
            r#"{{"type":"assistant",{head},"requestId":"req_1","timestamp":"2026-08-08T05:20:30Z","message":{{"id":"msg_1","model":"claude-opus-5","content":[{{"type":"text","text":"好的，先读一遍 normalize.rs"}}],"usage":{{"output_tokens":100,"input_tokens":5}}}}}}"#
        ),
        format!(
            r#"{{"type":"assistant",{head},"requestId":"req_1","timestamp":"2026-08-08T05:20:31Z","message":{{"id":"msg_1","content":[{{"type":"tool_use","id":"t1","name":"Read","input":{{"file_path":"src/normalize.rs"}}}}],"usage":{{"output_tokens":100,"input_tokens":5}}}}}}"#
        ),
        format!(
            r#"{{"type":"user",{head},"timestamp":"2026-08-08T05:25:00Z","message":{{"content":"再跑一次测试"}}}}"#
        ),
        format!(
            r#"{{"type":"assistant",{head},"requestId":"req_2","timestamp":"2026-08-08T05:25:20Z","message":{{"id":"msg_2","content":[{{"type":"text","text":"tests pass"}}],"usage":{{"output_tokens":20,"input_tokens":5}}}}}}"#
        ),
    ]
}

/// One indexed transcript, kept alive by the returned directory.
fn corpus() -> (tempfile::TempDir, SearchIndex) {
    let dir = tempfile::tempdir().expect("tempdir");
    let project = dir.path().join("projects").join("-code-demo");
    std::fs::create_dir_all(&project).expect("mkdir");
    let file = project.join(format!("{SESSION}.jsonl"));
    std::fs::write(&file, transcript().join("\n")).expect("write");

    let index = SearchIndex::open_or_create(&dir.path().join("index")).expect("index");
    index
        .build(
            &[Transcript {
                harness: Harness::Claude,
                path: file,
            }],
            &[Root {
                harness: Harness::Claude,
                path: dir.path().to_path_buf(),
            }],
            false,
        )
        .expect("build");
    (dir, index)
}

fn app() -> (tempfile::TempDir, App) {
    let (dir, index) = corpus();
    let mut app = App::new(
        index,
        Query {
            limit: 20,
            ..Query::default()
        },
    );
    app.settle();
    (dir, app)
}

fn press(app: &mut App, code: KeyCode) {
    app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn type_in(app: &mut App, text: &str) {
    for ch in text.chars() {
        press(app, KeyCode::Char(ch));
    }
    app.settle();
}

/// With nothing typed, the opening screen is the most recent thing recorded —
/// which is a better answer to "what was I doing" than an empty pane.
#[test]
fn the_opening_screen_is_recent_activity() {
    let (_dir, app) = app();
    assert!(!app.hits.is_empty());
    assert!(app.selected().is_some());
}

#[test]
fn typing_narrows_and_enter_opens_the_conversation() {
    let (_dir, mut app) = app();
    type_in(&mut app, "tantivy");

    assert!(!app.hits.is_empty(), "the prompt mentions tantivy");
    assert!(app.hits.iter().all(|h| h.session == SESSION));

    press(&mut app, KeyCode::Enter);
    assert_eq!(app.view, View::Read);

    // Reading goes through `source`, so the conversation on screen has things
    // the index never stored — tool calls above all.
    let reading = app.reading.as_ref().expect("a session was opened");
    assert!(
        reading.session.tool_calls().count() > 0,
        "tool calls are in the transcript and never in the index"
    );

    press(&mut app, KeyCode::Esc);
    assert_eq!(app.view, View::Search);
}

#[test]
fn a_half_typed_query_is_reported_not_fatal() {
    let (_dir, mut app) = app();
    type_in(&mut app, "\"unbalanced");
    assert!(app.error.is_some(), "a parse error reaches the status line");

    // And typing on gets you back to results rather than leaving you stuck.
    type_in(&mut app, "\"");
    assert!(app.error.is_none());
}

#[test]
fn tab_cycles_the_kind_filter() {
    let (_dir, mut app) = app();
    assert_eq!(app.kind, None);

    press(&mut app, KeyCode::Tab);
    app.settle();
    assert_eq!(app.kind.as_deref(), Some("prompt"));
    assert!(app.hits.iter().all(|h| h.kind == "prompt"));

    for _ in 0..4 {
        press(&mut app, KeyCode::Tab);
    }
    app.settle();
    assert_eq!(app.kind, None, "the cycle comes back round to every kind");
}

#[test]
fn the_caret_moves_by_characters_not_bytes() {
    let (_dir, mut app) = app();
    type_in(&mut app, "重构");
    assert_eq!(app.cursor, 6);

    press(&mut app, KeyCode::Left);
    assert_eq!(app.cursor, 3, "one character back, not one byte");
    press(&mut app, KeyCode::Backspace);
    app.settle();
    assert_eq!(app.query, "构");

    // Ctrl-U cuts back to the start of the line, so it has to know where the
    // caret is in bytes even though it was moved in characters.
    press(&mut app, KeyCode::End);
    app.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    )));
    app.settle();
    assert!(app.query.is_empty());
    assert_eq!(app.cursor, 0);
}

#[test]
fn ctrl_c_quits_from_anywhere() {
    let (_dir, mut app) = app();
    press(&mut app, KeyCode::Enter);
    app.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    assert!(app.quit);
    assert!(
        app.exit_hint().is_some(),
        "the shell is left with something to type"
    );
}

// ---- what actually lands on the screen -----------------------------------

fn screen(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal.draw(|frame| ui::draw(frame, app)).expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            let mut row = String::new();
            let mut skip = false;
            for x in 0..buffer.area.width {
                // A wide character occupies two cells; the second is a blank
                // the terminal never draws, and reading it back would put a
                // space inside every Chinese word on screen.
                if std::mem::take(&mut skip) {
                    continue;
                }
                let symbol = buffer[(x, y)].symbol();
                skip = symbol.width() > 1;
                row.push_str(symbol);
            }
            row
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_search_screen_shows_the_query_the_hit_and_a_preview() {
    let (_dir, mut app) = app();
    type_in(&mut app, "tantivy");
    let screen = screen(&mut app, 120, 30);

    assert!(screen.contains("tantivy"), "the query is echoed:\n{screen}");
    assert!(screen.contains("aaaaaaaa#"), "a hit names its session");
    assert!(
        screen.contains("Read(src/normalize.rs)"),
        "the preview reads the transcript, tool calls and all:\n{screen}"
    );
}

/// Narrow terminals give the whole width to the results rather than two
/// unreadable columns.
#[test]
fn a_narrow_terminal_drops_the_preview() {
    let (_dir, mut app) = app();
    type_in(&mut app, "tantivy");
    let screen = screen(&mut app, 70, 20);
    assert!(screen.contains("aaaaaaaa#"));
    assert!(!screen.contains("Read(src/normalize.rs)"));
}

#[test]
fn the_reader_opens_on_the_turn_the_hit_came_from() {
    let (_dir, mut app) = app();
    type_in(&mut app, "测试");
    press(&mut app, KeyCode::Enter);
    let screen = screen(&mut app, 100, 24);

    assert!(
        screen.contains("再跑一次测试"),
        "the reader lands on the matching turn:\n{screen}"
    );
    assert!(screen.contains("aaaaaaaa"), "the header names the session");
}
