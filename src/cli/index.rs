//! `whence index` — building and refreshing the search index.

use super::report::thousands;
use anyhow::Result;
use whence::index::SearchIndex;

pub fn build(cli: &super::Cli, force: bool) -> Result<()> {
    let roots = cli.roots()?;
    let transcripts = cli.transcripts()?;
    let index = SearchIndex::open_or_create(&cli.index_dir()?)?;
    let report = index.build(&transcripts, &roots, force)?;

    for root in &roots {
        println!("root        {}  ({})", root.path.display(), root.harness);
    }
    println!(
        "transcripts {} scanned, {} unchanged and never opened",
        report.scanned, report.unchanged
    );
    println!(
        "read        {} ({} new, {} grown, {} gone) — {:.1} MB",
        report.touched(),
        report.fresh,
        report.updated,
        report.dropped,
        report.bytes as f64 / 1_048_576.0
    );
    println!(
        "documents   {} indexed in {:.1}s",
        thousands(report.docs as u64),
        report.elapsed.as_secs_f64()
    );
    if report.unreadable > 0 {
        println!(
            "unreadable  {} — left unindexed, retried next run",
            report.unreadable
        );
    }
    if report.touched() == 0 && report.dropped == 0 {
        println!("\nnothing changed since the last run.");
    }
    Ok(())
}

/// Open the index for a query, bringing it up to date on the way in. An
/// incremental refresh costs a fraction of a second when nothing changed, so
/// there is no reason to make you remember `whence index` first.
///
/// A refresh that cannot happen — a read-only cache, another process holding
/// the writer — is not a reason to refuse to search, so it falls back to
/// querying whatever is already on disk.
pub fn for_query(cli: &super::Cli, no_refresh: bool) -> Result<SearchIndex> {
    let dir = cli.index_dir()?;
    if !no_refresh {
        if let (Ok(roots), Ok(transcripts), Ok(index)) = (
            cli.roots(),
            cli.transcripts(),
            SearchIndex::open_or_create(&dir).map_err(drop),
        ) {
            match index.build(&transcripts, &roots, false) {
                Ok(report) => {
                    if report.touched() > 0 || report.dropped > 0 {
                        eprintln!(
                            "index  +{} documents from {} transcripts ({:.1}s)",
                            report.docs,
                            report.touched(),
                            report.elapsed.as_secs_f64()
                        );
                    }
                    return Ok(index);
                }
                Err(err) => eprintln!("index  not refreshed ({err}); searching what is on disk"),
            }
        }
    }
    SearchIndex::open(&dir)
}
