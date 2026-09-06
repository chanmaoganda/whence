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

use super::app::{position_of, App, Group, Laid, Reading, Row, Show, View};
use crate::model::{first_line, short_id, when, Session, ToolCall, Turn};
use crate::render::{self, Block as Md, Doc, Emphasis, Span as MdSpan};
use crate::search::{Excerpt, Hit};
use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, Padding, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// How a matched word is marked, wherever one is shown. Bold alone does not
/// catch the eye in a wall of prose — half of a code-heavy transcript is bold
/// already — so the mark is bold *and* underlined, which no rendered markdown
/// uses.
fn marked(base: Style) -> Style {
    base.bold().underlined().not_dim()
}

/// The selected row's background. An indexed colour rather than an RGB one, so
/// it comes from the terminal's own palette and stays legible under whichever
/// theme the user actually has.
const SELECTED: Color = Color::Indexed(24);

/// Below this the preview pane costs the results list more than it gives back,
/// so the results get the whole width.
const PREVIEW_FROM: u16 = 96;
/// Lines of excerpt under one match. A shut session gets one — see
/// [`session_item`].
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
        Constraint::Length(3),
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

/// The search box.
///
/// A box, rather than a line at the top of the screen: this is the one thing
/// on screen you type into, and a bare row of text at the top of a wall of
/// results does not look like anywhere you can type. The border is the cheapest
/// way to say so, and it carries the filter chips on its own top edge.
fn query_line(frame: &mut Frame, app: &App, area: Rect) {
    // How many conversations, not only how many matches: a hundred hits from
    // two sessions and a hundred from ninety are the same number and not
    // remotely the same result.
    let found = match (app.hits.len(), app.tree.len()) {
        (0, _) => "no matches".to_string(),
        (n, 1) => format!("{n} in 1 session"),
        (n, s) => format!("{n} in {s} sessions"),
    };
    let chips = format!(
        " {} · {} · {} ",
        app.harness_label(),
        app.kind.as_deref().unwrap_or("all kinds"),
        found
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if app.query.is_empty() {
            Color::DarkGray
        } else {
            Color::Cyan
        }))
        .title(Span::styled(
            " search ",
            Style::new().fg(Color::Cyan).bold(),
        ))
        .title_top(
            Line::from(Span::styled(chips, Style::new().fg(Color::DarkGray))).right_aligned(),
        )
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let prompt = Span::styled("❯ ", Style::new().fg(Color::Cyan).bold());
    let typed = if app.query.is_empty() {
        Span::styled(
            "search prompts, replies, thinking and edits",
            Style::new().fg(Color::DarkGray),
        )
    } else {
        // The query is the one string on screen the user wrote themselves;
        // it gets the brightest thing a terminal has.
        Span::styled(app.query.clone(), Style::new().fg(Color::White).bold())
    };
    // What you typed is not always what is being looked for: `src/model.rs` is
    // three words and `重构索引` is two, and every one of them has to be
    // present. Say so where the analyzer did the splitting rather than you.
    let mut line = vec![prompt, typed];
    if app.terms.len() > app.query.split_whitespace().count() {
        line.push(Span::styled(
            format!("   {}", app.terms.join(" + ")),
            Style::new().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Line::from(line), inner);

    // The real cursor, so the terminal blinks it where the caret is and a
    // wide character does not push it half a column out.
    let at = inner.x + 2 + app.query[..app.cursor].width() as u16;
    frame.set_cursor_position((at.min(inner.right().saturating_sub(1)), inner.y));
}

fn results_list(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.tree.is_empty() {
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

    // Two columns of padding, and two for the selection bar.
    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .tree
        .rows()
        .iter()
        .map(|row| match *row {
            Row::Session(g) => {
                let group = &app.tree.groups[g];
                session_item(group, &app.hits, width)
            }
            Row::Hit(g, i) => {
                let group = &app.tree.groups[g];
                let hit = &app.hits[group.hits[i]];
                hit_item(hit, i + 1 == group.hits.len(), width)
            }
        })
        .collect();
    let list = List::new(items)
        .block(Block::new().padding(Padding::new(1, 1, 0, 0)))
        // Not `REVERSED`. Reversing swaps every cell's colours, so the dimmed
        // excerpt under a hit turns light grey on white and the words you were
        // reading disappear. A background of our own keeps every colour in the
        // row, and clearing DIM brings the excerpt up rather than washing it out.
        .highlight_style(
            Style::new()
                .bg(SELECTED)
                .remove_modifier(Modifier::DIM)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▌ ");
    frame.render_stateful_widget(list, area, &mut app.list);
}

/// One conversation: which one it was, and how much of the query it answered.
///
/// This is the row you choose between, so it carries what tells two sessions
/// apart — when, which project, and what the conversation was called — and not
/// what the matches inside it say. Shut, it still shows its best passage,
/// because a row that cannot say why it is there is a row you have to open to
/// find out.
fn session_item(group: &Group, hits: &[Hit], width: usize) -> ListItem<'static> {
    let best = &hits[group.hits[group.best]];
    let newest = group.hits.iter().filter_map(|&at| hits[at].timestamp).max();
    let count = match group.hits.len() {
        1 => "1 match".to_string(),
        n => format!("{n} matches"),
    };

    let arrow = if group.open { "▾ " } else { "▸ " };
    let stamp = day(newest);
    // Beside a preview the list is around fifty columns, which is not enough
    // for every column at once — so the parts that only narrow it down go in
    // while there is room and are left out when there is not, in that order.
    // Nothing wraps: a header that spills onto a second line stops being a
    // header.
    let mut room = width.saturating_sub(arrow.width() + stamp.width() + count.width() + 1);
    let mut fits = |text: String| -> Option<String> {
        let want = text.width();
        (want <= room).then(|| {
            room -= want;
            text
        })
    };
    let project = fits(format!("  {}", project_name(&best.project)));
    let id = fits(format!("  {}", short_id(&best.session)));
    let harness = fits(format!("  {}", best.harness));

    let mut head = vec![
        Span::styled(arrow, Style::new().fg(Color::Cyan)),
        // Grey rather than dark grey: it has to stay legible on the selection
        // bar as well as recede on a plain row.
        Span::styled(stamp, Style::new().fg(Color::Gray)),
    ];
    head.extend(harness.map(|h| Span::styled(h, Style::new().dim())));
    head.extend(project.map(|p| Span::styled(p, Style::new().fg(Color::LightBlue))));
    head.extend(id.map(|i| Span::styled(i, Style::new().dim())));
    // The count goes to the right edge, so a column of them lines up and the
    // conversation that answered the query six times is visible at a glance.
    head.push(Span::raw(" ".repeat(room + 1)));
    head.push(Span::styled(count, Style::new().fg(Color::DarkGray)));

    let mut lines = vec![Line::from(head)];
    // What the conversation was called is the answer to "which one is this?",
    // so it gets a line of its own rather than a column that is the first thing
    // squeezed out. Most sessions have no title and this is their opening
    // prompt — either way it is the sentence you would recognise.
    let name = first_line(best.name(), width);
    if !name.is_empty() {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(clip(&name, width.saturating_sub(2)), Style::new()),
        ]));
    }
    // Shut, the row still shows the passage that answered the query — a result
    // that cannot say why it is there is one you have to open to find out. One
    // line of it, because the point of shutting it was to see more sessions.
    if !group.open {
        lines.extend(excerpt_lines(&best.excerpt, width, "  ", 1));
        lines.push(Line::default());
    }
    ListItem::new(lines)
}

/// A session's stamp, without the year. The list is a column of them a week
/// apart and the year is four columns the project name needs more.
fn day(ts: Option<DateTime<Utc>>) -> String {
    match ts {
        Some(t) => t.format("%m-%d %H:%M").to_string(),
        None => " ".repeat(11),
    }
}

/// One match inside a conversation: where in it, and what it says.
///
/// Drawn as a branch of the session above it. Everything the two rows would
/// share — the harness, the project, the session id — is on that row already;
/// what is left here is the part that differs between one match and the next.
fn hit_item(hit: &Hit, last: bool, width: usize) -> ListItem<'static> {
    let guide = if last { "└ " } else { "├ " };
    let under = if last { "  " } else { "│ " };
    let mut header = vec![
        Span::styled(guide, Style::new().fg(Color::DarkGray)),
        Span::styled(format!("#{:<4}", hit.turn), Style::new().fg(Color::Cyan)),
        Span::styled(
            format!("{:<7}", hit.kind),
            Style::new().fg(kind_color(&hit.kind)),
        ),
        Span::styled(clock(hit.timestamp), Style::new().fg(Color::Gray)),
    ];
    // An edit hit is about a file, and which file is the whole of it.
    if let Some(file) = hit.file.as_deref() {
        header.push(Span::styled(
            format!("  {}", first_line(file, 48)),
            Style::new().dim(),
        ));
    }

    let mut lines = vec![Line::from(header)];
    lines.extend(excerpt_lines(
        &hit.excerpt,
        width,
        &format!("{under}  "),
        EXCERPT_LINES,
    ));
    if let Some(why) = hit.why() {
        lines.push(Line::from(vec![
            Span::styled(under.to_string(), Style::new().fg(Color::DarkGray)),
            Span::styled(
                format!("  ↳ {}", first_line(&why, width.saturating_sub(6))),
                Style::new().fg(Color::Yellow),
            ),
        ]));
    }
    // The blank belongs to the group, not the branch: it goes under the last
    // one, so an open conversation reads as a block rather than as a stack of
    // separate results.
    if last {
        lines.push(Line::default());
    }
    ListItem::new(lines)
}

/// The time of day. A child row sits under a session row that already carries
/// the date, and repeating it there costs the columns the excerpt needs.
fn clock(ts: Option<DateTime<Utc>>) -> String {
    match ts {
        Some(t) => t.format("%H:%M").to_string(),
        None => " ".repeat(5),
    }
}

/// The matching passage, with the matched words picked out.
///
/// Highlights arrive as byte ranges into the excerpt rather than as markup, so
/// they survive being re-broken here at whatever width the pane turned out to be.
fn excerpt_lines(
    excerpt: &Excerpt,
    width: usize,
    indent: &str,
    lines: usize,
) -> Vec<Line<'static>> {
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

    // The indent and the room for the ellipsis come out of the width here,
    // rather than at each call site: an excerpt broken to the full width has
    // nowhere to put the mark that says it was cut, and loses its last word to
    // the right edge instead.
    let inner = width.saturating_sub(indent.width() + 1);
    let mut broken = break_lines(&chars, inner.max(8));
    let clipped = broken.len() > lines;
    broken.truncate(lines);
    let last = broken.len().saturating_sub(1);
    broken
        .into_iter()
        .enumerate()
        .map(|(i, run)| {
            let mut spans = vec![Span::styled(
                indent.to_string(),
                Style::new().fg(Color::DarkGray),
            )];
            spans.extend(runs_to_spans(&run, Style::new().dim(), marked));
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

    let show = app.show();
    let Some(preview) = &mut app.preview else {
        return;
    };
    preview.lay(inner.width, inner.height, show);
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

    let show = app.show();
    let Some(reading) = &mut app.reading else {
        return;
    };
    let inner = Rect {
        x: body.x + 1,
        width: body.width.saturating_sub(2),
        ..body
    };
    reading.lay(inner.width, inner.height, show);

    frame.render_widget(read_header(reading), top);
    frame.render_widget(body_paragraph(reading), inner);
    frame.render_widget(
        keys(&[
            ("↑↓", "scroll"),
            ("[ ]", "turn"),
            (
                "t",
                if show.thinking {
                    "hide thinking"
                } else {
                    "thinking"
                },
            ),
            ("o", if show.tools { "fold tools" } else { "tools" }),
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
pub fn conversation(session: &Session, show: Show, width: u16, words: &[String]) -> Laid {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut starts = Vec::with_capacity(session.turns.len());
    for turn in &session.turns {
        starts.push(lines.len());
        lines.extend(turn_lines(turn, show, width));
    }
    mark(&mut lines, words);
    Laid {
        width,
        show,
        lines,
        starts,
    }
}

/// Pick the matched words out of a laid-out conversation.
///
/// A transcript is thousands of lines and the reason you opened this one is a
/// sentence somewhere inside it. The results list can say *that* a hit matched;
/// only the conversation can show you where, and scrolling a wall of text
/// hunting for a word you can no longer see is the thing this tool exists to
/// stop you doing.
///
/// Marking happens after the lines are broken, so a word split across two lines
/// is marked on both halves rather than not at all.
fn mark(lines: &mut [Line<'static>], words: &[String]) {
    if words.is_empty() {
        return;
    }
    for line in lines {
        if line.spans.is_empty() {
            continue;
        }
        let marked: Vec<Span<'static>> = std::mem::take(&mut line.spans)
            .into_iter()
            .flat_map(|span| mark_span(span, words))
            .collect();
        line.spans = marked;
    }
}

/// One span, cut into marked and unmarked pieces. Reversed video rather than a
/// colour, so the mark stands out whatever the span was already styled as and
/// whatever palette the terminal has.
fn mark_span(span: Span<'static>, words: &[String]) -> Vec<Span<'static>> {
    let text = span.content.to_string();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for word in words {
        let mut from = 0;
        while let Some(at) = crate::search::find_word(&text, word, from) {
            ranges.push((at, at + word.len()));
            from = at + word.len().max(1);
        }
    }
    if ranges.is_empty() {
        return vec![span];
    }
    ranges.sort_unstable();

    let mut out: Vec<Span<'static>> = Vec::new();
    let mut at = 0;
    for (from, to) in ranges {
        // Two words that overlap in the text are one mark, not two.
        if from < at {
            continue;
        }
        if from > at {
            out.push(Span::styled(text[at..from].to_string(), span.style));
        }
        out.push(Span::styled(
            text[from..to].to_string(),
            marked(span.style).add_modifier(Modifier::REVERSED),
        ));
        at = to;
    }
    if at < text.len() {
        out.push(Span::styled(text[at..].to_string(), span.style));
    }
    out
}

/// A run of tool calls longer than this is folded to one line. Two or three
/// reads in a row are part of the sentence around them; the twentieth is not.
const FOLD_FROM: usize = 3;

/// One turn: what was asked, what came back, and what it did.
///
/// Tool calls are gathered rather than printed as they arrive. A single answer
/// can be eighty calls with three sentences threaded through it, and printed
/// one per line the sentences are what you scroll past. So a *run* — every call
/// with no prose between it and the next — collapses to a single line saying
/// how many there were and of what, and `o` puts them all back.
pub fn turn_lines(turn: &Turn, show: Show, width: u16) -> Vec<Line<'static>> {
    let w = width.max(8) as usize;
    let mut lines = vec![turn_rule(turn, w)];

    if let Some(prompt) = &turn.prompt {
        lines.extend(flow_text(
            prompt.text.trim(),
            w,
            "❯ ",
            "  ",
            Style::new().fg(Color::Cyan).bold(),
            Style::new().fg(Color::Cyan).bold(),
        ));
        lines.push(Line::default());
    }

    let mut run: Vec<&ToolCall> = Vec::new();
    for step in &turn.steps {
        let thinking = show.thinking && !step.thinking.trim().is_empty();
        let talking = !step.text.trim().is_empty();
        if thinking || talking {
            lines.extend(tool_run(&std::mem::take(&mut run), show.tools, w));
        }
        if thinking {
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
        if talking {
            lines.extend(wrap(&render::renderer().parse(&step.text), width));
            lines.push(Line::default());
        }
        run.extend(step.tool_calls.iter());
    }
    lines.extend(tool_run(&run, show.tools, w));
    lines
}

/// A run of consecutive tool calls, either spelled out or folded to a line.
fn tool_run(run: &[&ToolCall], expanded: bool, width: usize) -> Vec<Line<'static>> {
    if run.is_empty() {
        return Vec::new();
    }
    let mut lines = if expanded || run.len() <= FOLD_FROM {
        run.iter().map(|call| tool_line(call, width)).collect()
    } else {
        vec![folded_tools(run, width)]
    };
    lines.push(Line::default());
    lines
}

/// `· 23 tools  Bash ×11 · Read ×8 · Edit ×4  ✗ 2` — the shape of the work,
/// which is what you actually want from a run you are scrolling past.
fn folded_tools(run: &[&ToolCall], width: usize) -> Line<'static> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for call in run {
        match counts.iter_mut().find(|(name, _)| *name == call.name) {
            Some((_, n)) => *n += 1,
            None => counts.push((call.name.as_str(), 1)),
        }
    }
    // Busiest first, and ties in the order they were called, so the line is
    // stable between redraws.
    counts.sort_by_key(|(_, n)| std::cmp::Reverse(*n));

    let wrong = run.iter().filter(|c| c.denied || c.is_error).count();
    let mark =
        (wrong > 0).then(|| Span::styled(format!("  ✗ {wrong}"), Style::new().fg(Color::Yellow)));

    let head = format!("· {} tool calls  ", run.len());
    let budget =
        width.saturating_sub(head.width() + mark.as_ref().map_or(0, |s| s.content.width()));
    let names = counts
        .iter()
        .map(|(name, n)| format!("{name} ×{n}"))
        .collect::<Vec<_>>()
        .join(" · ");

    let mut spans = vec![
        Span::styled(head, Style::new().fg(Color::Magenta)),
        Span::styled(clip(&names, budget), Style::new().fg(Color::DarkGray)),
    ];
    spans.extend(mark);
    Line::from(spans)
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
    let mut last: Option<&Md> = None;
    for block in &doc.blocks {
        // Blocks are separated by a blank line, except between two bullets: a
        // list double-spaced is twice as long and reads as unrelated lines
        // rather than as a list.
        let tight = matches!((last, block), (Some(Md::Bullet { .. }), Md::Bullet { .. }));
        if !out.is_empty() && !tight {
            out.push(Line::default());
        }
        last = Some(block);
        match block {
            Md::Paragraph(spans) => out.extend(flow(spans, w, "", "", Style::new(), Style::new())),
            // The level is worth a mark rather than a font: a terminal has one
            // size, and `###` three levels down still has to look subordinate.
            Md::Heading { level, spans } => out.extend(flow(
                spans,
                w,
                &format!("{} ", "#".repeat((*level).clamp(1, 6) as usize)),
                "",
                Style::new().fg(Color::DarkGray),
                Style::new().bold().fg(heading_color(*level)),
            )),
            Md::Bullet {
                depth,
                marker,
                spans,
            } => {
                let indent = "  ".repeat(*depth as usize);
                // The marker's own width, so `10.` and `•` both hang their
                // continuation lines under the text rather than under the mark.
                let first = format!("{indent}{marker} ");
                out.extend(flow(
                    spans,
                    w,
                    &first,
                    &" ".repeat(first.width()),
                    Style::new().fg(Color::Cyan),
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
            Md::Table { head, rows } => out.extend(table_lines(head, rows, w)),
            Md::Rule => out.push(Line::from(Span::styled(
                "─".repeat(w),
                Style::new().fg(Color::DarkGray),
            ))),
        }
    }
    out
}

/// A markdown table, in columns.
///
/// Columns get the width they ask for when the pane can afford it, and are cut
/// down in proportion when it cannot — a cell is clipped rather than wrapped,
/// because a table whose rows are two lines tall stops reading as a table.
fn table_lines(head: &[String], rows: &[Vec<String>], width: usize) -> Vec<Line<'static>> {
    let cols = head.len().max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if cols == 0 {
        return Vec::new();
    }
    const GAP: &str = " │ ";
    let mut want = vec![0usize; cols];
    for row in std::iter::once(&head.to_vec()).chain(rows) {
        for (i, cell) in row.iter().enumerate().take(cols) {
            want[i] = want[i].max(cell.trim().width());
        }
    }

    let budget = width.saturating_sub(GAP.width() * (cols - 1)).max(cols * 3);
    let asked: usize = want.iter().sum();
    if asked > budget {
        // Proportional, with a floor: a column squeezed to nothing tells you
        // less than a column that says it was cut.
        for w in want.iter_mut() {
            *w = (*w * budget / asked.max(1)).max(3);
        }
        while want.iter().sum::<usize>() > budget {
            let Some(worst) = longest(&want) else { break };
            want[worst] -= 1;
        }
    }

    let gap = Span::styled(GAP.to_string(), Style::new().fg(Color::DarkGray));
    let mut out = Vec::with_capacity(rows.len() + 2);
    if !head.is_empty() {
        out.push(row_line(head, &want, gap.clone(), Style::new().bold()));
        out.push(Line::from(Span::styled(
            want.iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("─┼─"),
            Style::new().fg(Color::DarkGray),
        )));
    }
    for row in rows {
        out.push(row_line(row, &want, gap.clone(), Style::new()));
    }
    out
}

fn row_line(row: &[String], widths: &[usize], gap: Span<'static>, style: Style) -> Line<'static> {
    let mut spans = Vec::with_capacity(widths.len() * 2);
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            spans.push(gap.clone());
        }
        let cell = row.get(i).map(|c| c.trim()).unwrap_or("");
        spans.push(Span::styled(pad(&clip(cell, *w), *w), style));
    }
    Line::from(spans)
}

/// The widest column, which is the one that can afford to lose a character.
fn longest(widths: &[usize]) -> Option<usize> {
    widths
        .iter()
        .enumerate()
        .filter(|(_, w)| **w > 3)
        .max_by_key(|(_, w)| **w)
        .map(|(i, _)| i)
}

fn pad(text: &str, width: usize) -> String {
    let mut out = text.to_string();
    out.push_str(&" ".repeat(width.saturating_sub(text.width())));
    out
}

/// Headings shade off as they get deeper, so `##` under a `#` reads as being
/// under it in a terminal that has exactly one font size.
fn heading_color(level: u8) -> Color {
    match level {
        1 => Color::Yellow,
        2 => Color::LightYellow,
        _ => Color::White,
    }
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
            out.extend(runs_to_spans(&run, base, style_of_strong));
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
/// `strong` is how [`Emphasis::Strong`] is drawn, because the two callers mean
/// different things by it: in rendered markdown it is the author's own bold, and
/// in an excerpt it is the word you searched for.
fn runs_to_spans(
    run: &[(char, Emphasis)],
    base: Style,
    strong: fn(Style) -> Style,
) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut current: Option<Emphasis> = None;
    for &(ch, em) in run {
        if current != Some(em) {
            if let Some(was) = current.take() {
                out.push(Span::styled(
                    std::mem::take(&mut text),
                    style_of(was, base, strong),
                ));
            }
            current = Some(em);
        }
        text.push(ch);
    }
    if let Some(em) = current {
        out.push(Span::styled(text, style_of(em, base, strong)));
    }
    out
}

fn style_of_strong(base: Style) -> Style {
    base.bold().not_dim()
}

fn style_of(emphasis: Emphasis, base: Style, strong: fn(Style) -> Style) -> Style {
    match emphasis {
        Emphasis::None => base,
        Emphasis::Strong => strong(base),
        Emphasis::Emph => base.italic(),
        Emphasis::Strike => base.crossed_out(),
        Emphasis::Code => base.fg(Color::Green),
        Emphasis::Link => base.fg(Color::Blue).underlined(),
    }
}

/// Cut a string to `width` columns, marking it if anything was lost.
fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
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
        let what = match crate::search::relaxation(&app.hits) {
            Some(pairs) => format!("nothing matched exactly — relaxed: {pairs}"),
            None => "nothing matched exactly — relaxed to fuzzy matching".to_string(),
        };
        return Line::from(vec![
            Span::raw(" "),
            Span::styled(what, Style::new().fg(Color::Yellow)),
        ]);
    }
    keys(&[
        ("↑↓", "move"),
        ("←→", "fold"),
        ("⏎", "read"),
        ("⇥", "kind"),
        ("⇧⇥", "harness"),
        // Said out loud because the arrows no longer do it: the caret is on
        // the readline keys, which is where the rest of this box already is.
        ("^B^F", "caret"),
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
