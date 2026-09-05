//! The browser: how text is broken to fit, and what the keys do to the state.
//!
//! Nothing here opens a terminal. `App` is driven with the events crossterm
//! would have produced, and the drawing is checked against a `TestBackend`, so
//! a regression shows up as a failing assertion rather than as a screen someone
//! has to look at.

use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Modifier;
use ratatui::Terminal;
use unicode_width::UnicodeWidthStr;
use whence::index::SearchIndex;
use whence::model::Harness;
use whence::render::renderer;
use whence::search::Query;
use whence::source::{Root, Transcript};
use whence::tui::{ui, App, Show, View};

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

/// Markdown is read, not shown. A reply full of `**` and `|` is what most of
/// this corpus looks like, and the pipes are not the part you came to read.
#[test]
fn markdown_is_rendered_rather_than_printed() {
    let lines = lines_of(
        "## What changed\n\nThe **folding** is in `ui.rs`.\n\n- one\n- two\n",
        40,
    );
    let screen = lines.join("\n");
    assert!(screen.contains("# What changed"), "{screen}");
    assert!(
        !screen.contains("**") && screen.contains("The folding is in ui.rs."),
        "markup is emphasis, not characters:\n{screen}"
    );
    assert!(screen.contains("• one"), "{screen}");
}

/// A table shown as its pipes is the ugliest thing in the reader, and a table
/// wider than the pane still has to stay a table.
#[test]
fn a_table_is_laid_out_in_columns() {
    let source = "| harness | sessions |\n| --- | --- |\n| claude | 812 |\n";
    let lines = lines_of(source, 30);
    assert!(lines[0].starts_with("harness │ sessions"), "{lines:?}");
    assert!(lines[1].contains('┼'), "a rule under the head: {lines:?}");
    assert!(lines[2].starts_with("claude  │ 812"), "{lines:?}");
    for line in &lines {
        assert!(line.width() <= 30, "{line:?}");
    }

    // Squeezed into a pane that cannot hold it, every column still gets some.
    let narrow = lines_of(source, 14);
    for line in &narrow {
        assert!(line.width() <= 14, "{line:?}");
        assert!(!line.contains('|'), "still a table, not pipes: {line:?}");
    }
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

/// Why *this* conversation came back. A transcript is thousands of lines and
/// the reason you opened it is one sentence inside it, so the words that
/// matched are picked out wherever they appear — the results list can only say
/// that a hit matched, not where.
#[test]
fn the_reader_marks_the_words_that_matched() {
    let (_dir, mut app) = app();
    type_in(&mut app, "tantivy");
    press(&mut app, KeyCode::Enter);
    let _ = screen(&mut app, 100, 24);

    let laid = app
        .reading
        .as_ref()
        .expect("a reader")
        .laid()
        .expect("laid");
    let marked: Vec<String> = laid
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .filter(|span| span.style.add_modifier.contains(Modifier::REVERSED))
        .map(|span| span.content.to_string())
        .collect();

    assert_eq!(marked, ["tantivy"], "the matched word, and only it");
}

/// The other half: a query is not what you typed once the analyzer has cut it
/// up, and a screen that does not say so cannot explain its own results.
#[test]
fn the_query_box_shows_the_words_the_input_was_cut_into() {
    let (_dir, mut app) = app();
    type_in(&mut app, "重建索引");
    let screen = screen(&mut app, 100, 24);

    assert!(
        screen.contains("重建 + 索引"),
        "one word typed, two looked for:\n{screen}"
    );
}

// ---- tool calls ----------------------------------------------------------

/// Forty calls between two sentences is a normal turn, and printed one per line
/// the sentences are what you scroll past.
#[test]
fn a_long_run_of_tool_calls_folds_to_one_line() {
    let mut calls: Vec<String> = (0..5)
        .map(|i| read_call(&format!("r{i}"), &format!("src/a{i}.rs")))
        .collect();
    calls.extend((0..3).map(|i| bash_call(&format!("b{i}"), &format!("ls {i}"))));
    let turn = turn_with(&calls, false);

    let folded = shown(&turn, Show::default());
    assert!(
        folded.contains("8 tool calls") && folded.contains("Read ×5") && folded.contains("Bash ×3"),
        "a run says how many and of what:\n{folded}"
    );
    assert!(
        !folded.contains("src/a0.rs"),
        "and not each one of them:\n{folded}"
    );

    // Which is a fold, not a loss: every call is one key away.
    let open = shown(
        &turn,
        Show {
            tools: true,
            ..Show::default()
        },
    );
    assert!(open.contains("Read(src/a0.rs)"), "{open}");
    assert!(open.contains("Bash(ls 2)"), "{open}");
    assert!(!open.contains("8 tool calls"), "{open}");
}

/// A run short enough to read is left alone: two reads in a row are part of the
/// sentence around them.
#[test]
fn a_short_run_is_not_folded() {
    let calls = vec![
        read_call("s0", "src/short0.rs"),
        read_call("s1", "src/short1.rs"),
    ];
    let screen = shown(&turn_with(&calls, false), Show::default());
    assert!(screen.contains("Read(src/short0.rs)"), "{screen}");
    assert!(screen.contains("Read(src/short1.rs)"), "{screen}");
    assert!(!screen.contains("tool calls"), "{screen}");
}

/// A denied or failed call must not disappear into a count. It is the thing you
/// went looking for.
#[test]
fn a_folded_run_still_says_something_went_wrong() {
    let calls: Vec<String> = (0..6)
        .map(|i| bash_call(&format!("e{i}"), &format!("ls {i}")))
        .collect();
    let turn = turn_with(&calls, true);
    let line = ui::turn_lines(&turn, Show::default(), 80)
        .iter()
        .map(|l| l.to_string())
        .find(|l| l.contains("tool calls"))
        .expect("the run folded");
    assert!(
        line.contains("6 tool calls") && line.contains("Bash ×6"),
        "{line}"
    );
    assert!(line.contains("✗ 1"), "a denial survives the fold: {line}");
}

/// Prose splits a run: calls that answered one sentence do not get counted in
/// with the ones that answered the next.
#[test]
fn text_between_two_runs_keeps_them_apart() {
    let head = format!(r#""sessionId":"{SESSION}","cwd":"/code/demo""#);
    let step = |req: &str, body: String| {
        format!(
            r#"{{"type":"assistant",{head},"requestId":"{req}","timestamp":"2026-08-08T05:20:30Z","message":{{"id":"m_{req}","content":[{body}],"usage":{{"output_tokens":1}}}}}}"#
        )
    };
    let four = |tag: &str| {
        (0..4)
            .map(|i| read_call(&format!("{tag}{i}"), &format!("src/{tag}{i}.rs")))
            .collect::<Vec<_>>()
            .join(",")
    };
    let lines = vec![
        format!(
            r#"{{"type":"user",{head},"timestamp":"2026-08-08T05:20:00Z","message":{{"content":"go"}}}}"#
        ),
        step("req_1", four("a")),
        step(
            "req_2",
            r#"{"type":"text","text":"now the other half"}"#.to_string(),
        ),
        step("req_3", four("b")),
    ];
    let turn = parse_turn(&lines);
    let screen = shown(&turn, Show::default());
    assert_eq!(
        screen.matches("4 tool calls").count(),
        2,
        "two runs of four, not one of eight:\n{screen}"
    );
}

#[test]
fn o_toggles_the_tool_calls_in_the_reader() {
    let (_dir, mut app) = app();
    press(&mut app, KeyCode::Enter);
    assert!(!app.tools, "folded until asked");
    press(&mut app, KeyCode::Char('o'));
    assert!(app.tools);
    press(&mut app, KeyCode::Char('o'));
    assert!(!app.tools);
}

fn read_call(id: &str, path: &str) -> String {
    format!(r#"{{"type":"tool_use","id":"{id}","name":"Read","input":{{"file_path":"{path}"}}}}"#)
}

fn bash_call(id: &str, command: &str) -> String {
    format!(r#"{{"type":"tool_use","id":"{id}","name":"Bash","input":{{"command":"{command}"}}}}"#)
}

fn shown(turn: &whence::model::Turn, show: Show) -> String {
    ui::turn_lines(turn, show, 80)
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// One turn built from raw tool-use blocks, so a test can say exactly how long
/// a run is rather than inventing a `Turn` by hand — and so the folding is
/// checked against what the adapter really produces.
fn turn_with(calls: &[String], deny_last: bool) -> whence::model::Turn {
    let head = format!(r#""sessionId":"{SESSION}","cwd":"/code/demo""#);
    let mut lines = vec![
        format!(
            r#"{{"type":"user",{head},"timestamp":"2026-08-08T05:20:00Z","message":{{"content":"go"}}}}"#
        ),
        format!(
            r#"{{"type":"assistant",{head},"requestId":"req_1","timestamp":"2026-08-08T05:20:30Z","message":{{"id":"msg_1","content":[{}],"usage":{{"output_tokens":1}}}}}}"#,
            calls.join(",")
        ),
    ];
    if deny_last {
        let id = calls.len() - 1;
        lines.push(format!(
            r#"{{"type":"user",{head},"toolDenialKind":"user_reject","message":{{"content":[{{"type":"tool_result","tool_use_id":"e{id}","is_error":true,"content":"denied"}}]}}}}"#
        ));
    }
    parse_turn(&lines)
}

fn parse_turn(lines: &[String]) -> whence::model::Turn {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join(format!("{SESSION}.jsonl"));
    std::fs::write(&file, lines.join("\n")).expect("write");
    let (session, _) = whence::source::normalize_with(Harness::Claude, &file).expect("parse");
    session.turns.into_iter().next().expect("one turn")
}
