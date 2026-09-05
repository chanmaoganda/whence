//! Shared formatting. Every surface prints numbers the same way, so a count in
//! `stats` and the same count in `inspect` are comparable at a glance.

use std::collections::HashMap;

/// Lives in the model so the TUI, which is not part of this binary, prints
/// timestamps the same way `stats` and `show` do.
pub use whence::model::when;

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn pct(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64 * 100.0
    }
}

/// The `n` biggest entries, ties broken by name so output is stable.
pub fn top(map: &HashMap<String, usize>, n: usize) -> Vec<(&str, usize)> {
    let mut v: Vec<(&str, usize)> = map.iter().map(|(k, &c)| (k.as_str(), c)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    v.truncate(n);
    v
}

pub fn preview_list(items: &[&str], n: usize) -> String {
    let shown = items.iter().take(n).cloned().collect::<Vec<_>>().join(", ");
    if items.len() > n {
        format!("{shown}, +{} more", items.len() - n)
    } else {
        shown
    }
}
