//! `whence tui` — the full-screen browser.
//!
//! The command is thin on purpose: everything it does is in [`whence::tui`],
//! where it can be driven by a test that never opens a terminal.

use anyhow::{bail, Result};
use whence::index::SearchIndex;
use whence::search::Query;
use whence::tui::{self, App};

pub fn run(index: SearchIndex, query: Query) -> Result<()> {
    if !tui::is_interactive() {
        bail!("`whence tui` needs a terminal — for a pipe or a script, use `whence search`");
    }
    // Whatever they were last looking at, spelled the way you would type it, so
    // the shell is left holding a pointer into the corpus rather than nothing —
    // and, under it, the way back into the conversation itself, which is the
    // one thing the eight-character id on screen cannot give you.
    if let Some(exit) = tui::run(App::new(index, query))? {
        println!("whence show {}", exit.show);
        if let Some(resume) = exit.resume {
            println!("{resume}");
        }
    }
    Ok(())
}
