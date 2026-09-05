//! The full-screen browser: search as you type, read what you find.
//!
//! ```text
//!   index ──▶ search ──▶ hits ──┬──▶ results list
//!                               │
//!   source ──▶ Session ──▶ render::Doc ──▶ ui::wrap ──┴──▶ preview / reader
//! ```
//!
//! Three decisions shape this module.
//!
//! **Reading a session goes through [`crate::source`], never the index.** A
//! [`Hit`](crate::search::Hit) already carries the transcript path it came from,
//! so opening a conversation is one file read — no directory scan, and no
//! dependence on the index being current beyond the hit itself. This is the same
//! rule `whence show` follows, for the same reason.
//!
//! **Nothing is threaded.** Events are drained before each redraw, so holding a
//! key down or pasting a word costs one search and one transcript read rather
//! than one per keystroke. That is enough to keep typing smooth over the
//! reference corpus, and it keeps the whole surface deterministic: the same
//! keys always produce the same state, which is what [`App`] is tested on.
//!
//! **The layout work lives in [`ui`], the state in [`app`].** `ui` decides how
//! wide things are and breaks lines to fit; `app` never knows a terminal is
//! involved. Only the width and height measured during a draw flow back, so a
//! resize re-lays out and nothing else changes.

pub mod app;
pub mod ui;

pub use app::{App, View};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event};
use std::time::Duration;

/// Run the browser until the user quits.
///
/// Returns the `whence show` target of whatever they were last looking at, so
/// the shell they came back to has a pointer into the corpus rather than
/// nothing at all.
pub fn run(mut app: App) -> Result<Option<String>> {
    ratatui::run(|terminal| {
        while !app.quit {
            // Whatever the last batch of keys asked for — a new search, a
            // transcript to read — happens here, once, before anything is drawn.
            app.settle();
            terminal.draw(|frame| ui::draw(frame, &mut app))?;

            app.handle(event::read()?);
            // Everything already queued belongs to the same burst: apply it all,
            // then redraw once. A held-down arrow key must not mean one
            // transcript read per repeat.
            while event::poll(Duration::ZERO)? {
                app.handle(event::read()?);
            }
        }
        Ok(app.exit_hint())
    })
}

/// Whether a full-screen UI can be drawn at all. Piping `whence tui` somewhere
/// is a mistake worth reporting rather than a terminal to corrupt.
pub fn is_interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

/// Handling for one key press, kept out of [`App`] so the event plumbing and the
/// state machine can be read separately.
fn key_press(event: &Event) -> Option<event::KeyEvent> {
    match event {
        // Terminals that report key releases (kitty's protocol, Windows) send
        // every key twice; only the press means anything here.
        Event::Key(key) if key.kind == event::KeyEventKind::Press => Some(*key),
        _ => None,
    }
}
