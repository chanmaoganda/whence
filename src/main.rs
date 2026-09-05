use anyhow::Result;
use clap::Parser;

mod cli;

fn main() -> Result<()> {
    // Rust ignores SIGPIPE, which turns `whence show … | head` into a panic on
    // a broken pipe rather than a clean exit. Restore the default: quitting
    // when the reader goes away is the correct behaviour for a CLI.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    cli::Cli::parse().run()
}
