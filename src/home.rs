//! Where the user's files live, on whichever platform this was built for.
//!
//! Every harness keeps its transcripts in a dotted directory under the home
//! directory, and every surface here needs to find it. That is one question
//! with one platform-dependent answer, so it is asked in one place: `$HOME` is
//! not set on Windows, and a binary that reads it directly finds nothing at all
//! and reports an empty corpus rather than an error.

use std::ffi::OsString;
use std::path::PathBuf;

/// The user's home directory.
///
/// `$HOME` everywhere, and `%USERPROFILE%` on Windows, where `$HOME` is
/// normally unset — except under a Unix-shaped shell (Git Bash, MSYS), which
/// sets `$HOME` to something other than the profile directory. Checking `$HOME`
/// first is deliberate: it is what that shell means by home, and it is also the
/// override anyone would reach for.
pub fn dir() -> Option<PathBuf> {
    non_empty("HOME")
        .or_else(|| non_empty("USERPROFILE"))
        .map(PathBuf::from)
}

/// Where a derived, disposable cache belongs: the search index and nothing
/// else. `$XDG_CACHE_HOME`, `%LOCALAPPDATA%` on Windows, else `~/.cache`.
pub fn cache() -> Option<PathBuf> {
    if let Some(dir) = non_empty("XDG_CACHE_HOME") {
        return Some(PathBuf::from(dir));
    }
    if cfg!(windows) {
        if let Some(dir) = non_empty("LOCALAPPDATA") {
            return Some(PathBuf::from(dir));
        }
    }
    Some(dir()?.join(".cache"))
}

/// `$var`, treating unset and empty as the same thing — an exported but empty
/// `HOME` is what a stripped environment looks like, and joining onto it would
/// silently produce a relative path.
pub fn non_empty(var: &str) -> Option<OsString> {
    std::env::var_os(var).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reason this module exists: `HOME` is not the answer on Windows, and
    /// a home directory found by joining onto an empty string is not one.
    #[test]
    fn an_empty_variable_is_not_a_home_directory() {
        assert_eq!(non_empty("WHENCE_TEST_UNSET_VAR"), None);
        // Safe here: this is the only test that touches this variable, and it
        // reads it back on the same thread before anything else runs.
        unsafe { std::env::set_var("WHENCE_TEST_EMPTY_VAR", "") };
        assert_eq!(non_empty("WHENCE_TEST_EMPTY_VAR"), None);
    }
}
