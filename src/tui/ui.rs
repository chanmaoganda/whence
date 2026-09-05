//! Drawing, and the line breaking both views depend on.
//!
//! [`wrap`] is the function [`crate::render`]'s module docs point at. A
//! [`Doc`] deliberately carries *unwrapped* spans so that the display layer can
//! break them at the width it is actually drawing at — and breaking them is not
//! the usual split-on-spaces:
//!
//! * **Width is measured, not counted.** `重构` is two characters and four
//!   columns. Counting characters puts half this corpus past the right edge.
//! * **A line may break mid-run.** Chinese prose contains no spaces at all, so
//!   a wrapper that only breaks on whitespace emits one line per paragraph and
//!   shows you the first screenful of it. Latin text still breaks at spaces,
//!   because splitting a word is worse than a short line.
//! * **A code block is never reflowed.** It is shown verbatim and clipped at
//!   the right edge, because a rewrapped shell command is no longer a command
//!   you can copy and run.

use super::app::{position_of, App, Laid, Reading, View};
use crate::model::{first_line, short_id, when, Session, ToolCall, Turn};
use crate::render::{self, Block as Md, Doc, Emphasis, Span as MdSpan};
use crate::search::{Excerpt, Hit};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Padding, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Below this the preview pane costs the results list more than it gives back,
/// so the results get the whole width.
const PREVIEW_FROM: u16 = 96;
/// Lines of excerpt under each result.
const EXCERPT_LINES: usize = 2;

pub fn draw(frame: &mut Frame, app: &mut App) {
    match app.view {
        View::Search => search_view(frame, app),
        View::Read => read_view(frame, app),
    }
}

// ---- the search view -----------------------------------------------------

fn search_view(frame: &mut Frame, app: &mut App) {
    let [top, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    query_line(frame, app, top);

    let (results, preview) = if body.width >= PREVIEW_FROM {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(body);
        (left, Some(right))
    } else {
        (body, None)
    };

    results_list(frame, app, results);
    if let Some(area) = preview {
        preview_pane(frame, app, area);
    }
    frame.render_widget(status_line(app), foot);
}

fn query_line(frame: &mut Frame, app: &App, area: Rect) {
    let chips = format!(
        " {} · {} · {} ",
        app.harness_label(),
        app.kind.as_deref().unwrap_or("all kinds"),
        match app.hits.len() {
            0 => "no matches".to_string(),
            1 => "1 match".to_string(),
            n => format!("{n} matches"),
        }
    );
    let [left, right] =
        Layout::horizontal([Constraint::Min(8), Constraint::Length(chips.width() as u16)])
            .areas(area);

    let prompt = Span::styled("❯ ", Style::new().fg(Color::Cyan).bold());
    let typed = if app.query.is_empty() {
        Span::styled(
            "search prompts, replies, thinking and edits",
            Style::new().dim(),
        )
    } else {
        Span::raw(app.query.clone())
    };
    frame.render_widget(Line::from(vec![prompt, typed]), left);
    frame.render_widget(
        Line::from(Span::styled(chips, Style::new().fg(Color::DarkGray))),
        right,
    );

    // The real cursor, so the terminal blinks it where the caret is and a
    // wide character does not push it half a column out.
    let at = left.x + 2 + app.query[..app.cursor].width() as u16;
    frame.set_cursor_position((at.min(left.right().saturating_sub(1)), left.y));
}

fn results_list(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.hits.is_empty() {
        let hint = match &app.error {
            Some(err) => Line::from(Span::styled(err.clone(), Style::new().fg(Color::Red))),
            None if app.query.is_empty() => {
                Line::from(Span::styled("nothing indexed yet", Style::new().dim()))
            }
            None => Line::from(Span::styled(
                "no matches — try fewer words",
                Style::new().dim(),
            )),
        };
        frame.render_widget(
            Paragraph::new(hint).block(Block::new().padding(Padding::new(1, 1, 1, 0))),
            area,
        );
        return;
    }

    // Two columns of padding, and one for the selection marker.
    let width = area.width.saturating_sub(3) as usize;
    let items: Vec<ListItem> = app.hits.iter().map(|hit| hit_item(hit, width)).collect();
    let list = List::new(items)
        .block(Block::new().padding(Padding::new(1, 1, 0, 0)))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut app.list);
}

/// One result: where it came from, then why it matched.
fn hit_item(hit: &Hit, width: usize) -> ListItem<'static> {
    let mut header = vec![
        Span::styled(when(hit.timestamp), Style::new().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(format!("{:<6}", hit.harness), Style::new().dim()),
        Span::raw(" "),
        Span::styled(
            format!("{:<6}", hit.kind),
            Style::new().fg(kind_color(&hit.kind)),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{}#{}", short_id(&hit.session), hit.turn),
            Style::new().fg(Color::Cyan),
        ),
    ];
    // Which project it was is worth a column of its own: the same question
    // comes up in two repositories a week apart, and the title alone will not
    // tell you which answer you are looking at.
    let what = match hit.file.as_deref() {
        Some(file) => first_line(file, 48),
        None => first_line(&hit.title, 48),
    };
    header.push(Span::styled(
        format!("  {}", project_name(&hit.project)),
        Style::new().fg(Color::Blue),
    ));
    if !what.is_empty() {
        header.push(Span::styled(format!("  {what}"), Style::new().dim()));
    }

    let mut lines = vec![Line::from(header)];
    lines.extend(excerpt_lines(&hit.excerpt, width.saturating_sub(2)));
    lines.push(Line::default());
    ListItem::new(lines)
}

/// The matching passage, with the matched words picked out.
///
/// Highlights arrive as byte ranges into the excerpt rather than as markup, so
/// they survive being re-broken here at whatever width the pane turned out to be.
fn excerpt_lines(excerpt: &Excerpt, width: usize) -> Vec<Line<'static>> {
    let mut chars: Vec<(char, Emphasis)> = Vec::with_capacity(excerpt.text.len());
    for (at, ch) in excerpt.text.char_indices() {
        let lit = excerpt.highlights.iter().any(|r| r.contains(&at));
        // Excerpts are a window cut out of the middle of a document; newlines in
        // one are noise, not structure.
        let ch = if ch == '\n' || ch == '\r' { ' ' } else { ch };
        chars.push((
            ch,
            if lit {
                Emphasis::Strong
            } else {
                Emphasis::None
            },
        ));
    }

    let mut broken = break_lines(&chars, width.max(8));
    let clipped = broken.len() > EXCERPT_LINES;
    broken.truncate(EXCERPT_LINES);
    let last = broken.len().saturating_sub(1);
    broken
        .into_iter()
        .enumerate()
        .map(|(i, run)| {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(runs_to_spans(&run, Style::new().dim()));
            if clipped && i == last {
                spans.push(Span::styled("…", Style::new().dim()));
            }
            Line::from(spans)
        })
        .collect()
}

fn preview_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::new()
        .borders(Borders::LEFT)
        .border_style(Style::new().fg(Color::DarkGray))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let thinking = app.thinking;
    let Some(preview) = &mut app.preview else {
        return;
    };
    preview.lay(inner.width, inner.height, thinking);
    frame.render_widget(body_paragraph(preview), inner);
}

// ---- the reading view ----------------------------------------------------

fn read_view(frame: &mut Frame, app: &mut App) {
    let [top, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let thinking = app.thinking;
    let Some(reading) = &mut app.reading else {
        return;
    };
    let inner = Rect {
        x: body.x + 1,
        width: body.width.saturating_sub(2),
        ..body
    };
    reading.lay(inner.width, inner.height, thinking);

    frame.render_widget(read_header(reading), top);
    frame.render_widget(body_paragraph(reading), inner);
    frame.render_widget(
        keys(&[
            ("↑↓", "scroll"),
            ("[ ]", "turn"),
            (
                "t",
                if thinking {
                    "hide thinking"
                } else {
                    "thinking"
                },
            ),
            ("esc", "back"),
            ("^C", "quit"),
        ]),
        foot,
    );
}

fn read_header(reading: &Reading) -> Line<'static> {
    let session = &reading.session;
    let at = position_of(session, reading.turn).map_or(0, |i| i + 1);
    let title = session
        .title
        .as_deref()
        .map(|t| format!("  {}", first_line(t, 50)))
        .unwrap_or_default();
    Line::from(vec![
        Span::styled(
            format!(" {}", short_id(&session.id)),
            Style::new().fg(Color::Cyan),
        ),
        Span::styled(
            format!("  {}  ", session.harness),
            Style::new().fg(Color::DarkGray),
        ),
        Span::styled(session.project.clone(), Style::new().dim()),
        Span::styled(title, Style::new().dim()),
        Span::styled(
            format!("  turn {at}/{}", session.turns.len()),
            Style::new().fg(Color::DarkGray),
        ),
    ])
}

/// The visible slice of a laid-out conversation.
///
/// The lines were already broken to this width, so the paragraph must not wrap
/// again — it would undo the code-block rule and every hanging indent.
fn body_paragraph(reading: &Reading) -> Paragraph<'static> {
    let Some(laid) = reading.laid() else {
        return Paragraph::new("");
    };
    let visible: Vec<Line<'static>> = laid
        .lines
        .iter()
        .skip(reading.scroll)
        .take(reading.height as usize)
        .cloned()
        .collect();
    Paragraph::new(visible)
}

// ---- turning a session into lines ---------------------------------------

/// A whole conversation, broken to `width`, with the line each turn starts on.
pub fn conversation(session: &Session, thinking: bool, width: u16) -> Laid {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut starts = Vec::with_capacity(session.turns.len());
    for turn in &session.turns {
        starts.push(lines.len());
        lines.extend(turn_lines(turn, thinking, width));
    }
    Laid {
        width,
        thinking,
        lines,
        starts,
    }
}

/// One turn: what was asked, what came back, and what it did.
pub fn turn_lines(turn: &Turn, thinking: bool, width: u16) -> Vec<Line<'static>> {
    let w = width.max(8) as usize;
    let mut lines = vec![turn_rule(turn, w)];

    if let Some(prompt) = &turn.prompt {
        lines.extend(flow_text(
            prompt.text.trim(),
            w,
            "❯ ",
            "  ",
            Style::new().fg(Color::Cyan),
            Style::new().fg(Color::Cyan),
        ));
        lines.push(Line::default());
    }

    for step in &turn.steps {
        if thinking && !step.thinking.trim().is_empty() {
            lines.extend(flow_text(
                step.thinking.trim(),
                w,
                "~ ",
                "  ",
                Style::new().dim(),
                Style::new().dim().italic(),
            ));
            lines.push(Line::default());
        }
        if !step.text.trim().is_empty() {
            lines.extend(wrap(&render::renderer().parse(&step.text), width));
            lines.push(Line::default());
        }
        for call in &step.tool_calls {
            lines.push(tool_line(call, w));
        }
        if !step.tool_calls.is_empty() {
            lines.push(Line::default());
        }
    }
    lines
}

fn turn_rule(turn: &Turn, width: usize) -> Line<'static> {
    let stamp = when(turn.prompt.as_ref().and_then(|p| p.timestamp));
    let mut spans = vec![
        Span::styled("── ", Style::new().fg(Color::DarkGray)),
        Span::styled(format!("#{}", turn.index), Style::new().bold()),
        Span::styled(
            format!("  {}", stamp.trim()),
            Style::new().fg(Color::DarkGray),
        ),
    ];
    if turn.interrupted {
        spans.push(Span::styled("  interrupted", Style::new().fg(Color::Red)));
    }
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    spans.push(Span::styled(
        format!(" {}", "─".repeat(width.saturating_sub(used + 1))),
        Style::new().fg(Color::DarkGray),
    ));
    Line::from(spans)
}

fn tool_line(call: &ToolCall, width: usize) -> Line<'static> {
    let mark = match (call.denied, call.is_error) {
        (true, _) => Some(Span::styled("  ✗ denied", Style::new().fg(Color::Red))),
        (_, true) => Some(Span::styled("  ✗ error", Style::new().fg(Color::Yellow))),
        _ => None,
    };
    let budget = width.saturating_sub(2 + mark.as_ref().map_or(0, |s| s.content.width()));
    let mut spans = vec![
        Span::styled("· ", Style::new().fg(Color::DarkGray)),
        Span::styled(clip(&call.summary(budget), budget), Style::new().dim()),
    ];
    spans.extend(mark);
    Line::from(spans)
}

// ---- line breaking -------------------------------------------------------

/// Lay a [`Doc`] out at `width` columns.
pub fn wrap(doc: &Doc, width: u16) -> Vec<Line<'static>> {
    let w = width.max(8) as usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    for block in &doc.blocks {
        if !out.is_empty() {
            out.push(Line::default());
        }
        match block {
            Md::Paragraph(spans) => out.extend(flow(spans, w, "", "", Style::new(), Style::new())),
            Md::Heading { spans, .. } => out.extend(flow(
                spans,
                w,
                "",
                "",
                Style::new(),
                Style::new().bold().fg(Color::Yellow),
            )),
            Md::Bullet { depth, spans } => {
                let indent = "  ".repeat(*depth as usize);
                out.extend(flow(
                    spans,
                    w,
                    &format!("{indent}• "),
                    &format!("{indent}  "),
                    Style::new().fg(Color::DarkGray),
                    Style::new(),
                ))
            }
            Md::Quote(spans) => out.extend(flow(
                spans,
                w,
                "▏ ",
                "▏ ",
                Style::new().fg(Color::DarkGray),
                Style::new().dim(),
            )),
            // Verbatim, clipped, never reflowed.
            Md::Code { lines, .. } => out.extend(lines.iter().map(|line| {
                Line::from(vec![
                    Span::styled("│ ", Style::new().fg(Color::DarkGray)),
                    Span::styled(
                        clip(&expand_tabs(line), w.saturating_sub(2)),
                        Style::new().fg(Color::Green),
                    ),
                ])
            })),
            Md::Rule => out.push(Line::from(Span::styled(
                "─".repeat(w),
                Style::new().fg(Color::DarkGray),
            ))),
        }
    }
    out
}

fn flow_text(
    text: &str,
    width: usize,
    first: &str,
    rest: &str,
    marker: Style,
    base: Style,
) -> Vec<Line<'static>> {
    flow(&[MdSpan::plain(text)], width, first, rest, marker, base)
}

/// Break `spans` to `width`, with a marker on the first line and an indent on
/// the rest — a bullet, a quote bar, the `❯` in front of a prompt.
fn flow(
    spans: &[MdSpan],
    width: usize,
    first: &str,
    rest: &str,
    marker: Style,
    base: Style,
) -> Vec<Line<'static>> {
    let pad = first.width().max(rest.width());
    let inner = width.saturating_sub(pad).max(1);
    let chars = flatten(spans);
    break_lines(&chars, inner)
        .into_iter()
        .enumerate()
        .map(|(i, run)| {
            let lead = if i == 0 { first } else { rest };
            let mut out = Vec::with_capacity(run.len() + 1);
            if !lead.is_empty() {
                out.push(Span::styled(lead.to_string(), marker));
            }
            out.extend(runs_to_spans(&run, base));
            Line::from(out)
        })
        .collect()
}

/// Greedy line breaking, measured in columns.
///
/// The break goes at the last opportunity on the line — after a space, or
/// between two characters that may be split anywhere — and, when there is no
/// such opportunity, wherever the width ran out. That last case is not a
/// fallback for this corpus: it is what a run of Chinese with no spaces in it
/// requires.
fn break_lines(chars: &[(char, Emphasis)], width: usize) -> Vec<Vec<(char, Emphasis)>> {
    let width = width.max(1);
    let mut out: Vec<Vec<(char, Emphasis)>> = Vec::new();
    let mut line: Vec<(char, Emphasis)> = Vec::new();
    let mut col = 0usize;
    let mut brk: Option<usize> = None;

    for &(ch, em) in chars {
        if ch == '\n' {
            out.push(trim_end(std::mem::take(&mut line)));
            col = 0;
            brk = None;
            continue;
        }
        let w = column_width(ch);
        if col + w > width && !line.is_empty() {
            match brk.filter(|&at| at > 0 && at < line.len()) {
                Some(at) => {
                    let carried = line.split_off(at);
                    out.push(trim_end(line));
                    col = run_width(&carried);
                    line = carried;
                }
                _ => {
                    out.push(trim_end(std::mem::take(&mut line)));
                    col = 0;
                }
            }
            brk = None;
        }
        line.push((ch, em));
        col += w;
        if ch == ' ' || breaks_anywhere(ch) {
            brk = Some(line.len());
        }
    }
    if !line.is_empty() {
        out.push(trim_end(line));
    }
    out
}

/// Scripts written without spaces, where a line may break between any two
/// characters. A different question from the one [`crate::search`] asks about
/// script — that one is about how to match a typo, this one is about where a
/// line may end — so the two predicates are deliberately not shared.
fn breaks_anywhere(ch: char) -> bool {
    matches!(ch as u32,
        0x2E80..=0x9FFF      // CJK radicals, kana, punctuation, unified ideographs
        | 0xF900..=0xFAFF    // compatibility ideographs
        | 0xFF00..=0xFF60    // fullwidth forms
        | 0x20000..=0x3FFFF) // the extensions
}

/// A tab in a terminal cell is a control character, not a width. Code blocks
/// are the one place they survive as far as the screen, so give them one before
/// anything tries to measure or clip them.
fn expand_tabs(line: &str) -> String {
    line.replace('\t', "    ")
}

fn column_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

fn run_width(run: &[(char, Emphasis)]) -> usize {
    run.iter().map(|(ch, _)| column_width(*ch)).sum()
}

fn trim_end(mut run: Vec<(char, Emphasis)>) -> Vec<(char, Emphasis)> {
    while run.last().is_some_and(|(ch, _)| *ch == ' ') {
        run.pop();
    }
    run
}

/// Spans to a flat stream of styled characters, so a line may break in the
/// middle of one. `Plain` emits a single span per block, but a markdown
/// renderer will emit many and a break must be free to land between them.
fn flatten(spans: &[MdSpan]) -> Vec<(char, Emphasis)> {
    let mut out = Vec::new();
    for span in spans {
        for ch in span.text.chars() {
            // A tab in a terminal cell is not a width, it is a control
            // character; give it a fixed one before anything measures it.
            if ch == '\t' {
                out.extend(std::iter::repeat_n((' ', span.emphasis), 4));
            } else if ch == '\r' {
                continue;
            } else {
                out.push((ch, span.emphasis));
            }
        }
    }
    out
}

/// Back from characters to as few styled spans as the run allows.
fn runs_to_spans(run: &[(char, Emphasis)], base: Style) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut current: Option<Emphasis> = None;
    for &(ch, em) in run {
        if current != Some(em) {
            if let Some(was) = current.take() {
                out.push(Span::styled(std::mem::take(&mut text), style_of(was, base)));
            }
            current = Some(em);
        }
        text.push(ch);
    }
    if let Some(em) = current {
        out.push(Span::styled(text, style_of(em, base)));
    }
    out
}

fn style_of(emphasis: Emphasis, base: Style) -> Style {
    match emphasis {
        Emphasis::None => base,
        Emphasis::Strong => base.bold().not_dim(),
        Emphasis::Emph => base.italic(),
        Emphasis::Code => base.fg(Color::Green),
        Emphasis::Link => base.fg(Color::Blue).underlined(),
    }
}

/// Cut a string to `width` columns, marking it if anything was lost.
fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut col = 0;
    for ch in text.chars() {
        let w = column_width(ch);
        if col + w > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        col += w;
    }
    out.push('…');
    out
}

// ---- chrome --------------------------------------------------------------

/// The directory a project is known by, rather than the path it lives at.
fn project_name(project: &str) -> &str {
    project
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(project)
}

fn kind_color(kind: &str) -> Color {
    match kind {
        "prompt" => Color::Cyan,
        "reply" => Color::Green,
        "think" => Color::Magenta,
        "edit" => Color::Yellow,
        _ => Color::Gray,
    }
}

fn status_line(app: &App) -> Line<'static> {
    if let Some(err) = &app.error {
        return Line::from(vec![
            Span::raw(" "),
            Span::styled(err.clone(), Style::new().fg(Color::Red)),
        ]);
    }
    if app.relaxed {
        return Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "nothing matched exactly — relaxed to fuzzy matching",
                Style::new().fg(Color::Yellow),
            ),
        ]);
    }
    keys(&[
        ("↑↓", "move"),
        ("⏎", "read"),
        ("⇥", "kind"),
        ("⇧⇥", "harness"),
        ("^U", "clear"),
        ("esc", "quit"),
    ])
}

fn keys(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::with_capacity(pairs.len() * 3);
    for (key, what) in pairs {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::new().fg(Color::Cyan),
        ));
        spans.push(Span::styled(what.to_string(), Style::new().dim()));
    }
    Line::from(spans)
}
