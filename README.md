# whence

*Whence did this come?* — a local search engine and time machine for your
coding agents' transcripts.

Claude Code, Codex and omp all leave a detailed record of what you asked, what
they did and why, and then it is never read again. `whence` makes that record
searchable, replayable and attributable — across every agent you use, entirely
on your own machine. No telemetry, no uploads.

```console
$ whence sources
harness    files  root
claude       965  ~/.claude/projects
codex          9  ~/.codex/sessions
omp            3  ~/.omp/agent/sessions

$ whence "card view"
6 matches across 3 sessions (claude, codex)

2026-07-11 02:04  codex   prompt  /code/acme/dashboard  1498aa04#5
  the card view cannot be scrolled, and the grid view is not laid out right

$ whence show 1498aa04#5        # read the conversation back
$ whence file src/app.rs        # which sessions changed this, and why
$ whence --harness codex ...    # narrow to one agent
$ whence tui                    # browse it: search as you type, read what you find
```

## Why it is not a `grep` over `~/.claude`

The on-disk formats are undocumented event logs, and reading them naively gives
wrong answers rather than no answers:

- A Claude Code response is spread over many lines that each repeat the same
  `usage` block. Summing per line overstates output tokens by **93%**.
- A Codex `token_count` event carries a running session total, not a delta.
  Summing those overstates output tokens by **4268%**.
- omp counts reasoning tokens *inside* its output figure rather than beside it.
  Adding the two overstates output tokens by **59%**.
- Under both, most records that look like the user are not: tool results, hook
  output, `AGENTS.md`, `<system-reminder>` injections.
- Codex records every prompt and reply *twice*, in two different streams, and
  omp records every tool call twice.
- The record that says which file an edit touched identifies itself, in both
  harnesses, with an id that matches nothing else in the file.

Each of these is documented, measured against a real corpus, and pinned by a
test. See [CLAUDE.md](CLAUDE.md).

## Design

```
source/{claude,codex,omp} ──▶ model ──┬──▶ index + search (tantivy, jieba)
 per-harness parsing                  ├──▶ insights (corpus aggregates)
                                      ├──▶ render ──▶ CLI + ratatui TUI
                                      └──▶ MCP server (redacted)
```

`src/source/` is the only place that knows an on-disk format; `src/model.rs` is
what every other surface reads. **Adding a harness is a directory, a `Harness`
variant, and one line in a registry** — nothing else in the tree changes.

Chinese transcripts are a first-class case: text is tokenized with jieba, and
fuzzy matching dispatches on script, because CJK needs substring matching where
Latin needs edit distance.

Tool *results* are deliberately never indexed. They are most of the corpus by
volume and they are where pasted secrets live.

`whence tui` is the same search with a screen around it: results narrow as you
type, the pane beside them previews the conversation each one came from, and
`⏎` opens it. Replies are shown as markdown — headings, lists, tables and
emphasis, not the asterisks and pipes they were written with. Text is broken to
the width it is drawn at, measured in columns and split mid-run where the script
has no spaces — code blocks are the one thing never reflowed, because a
rewrapped shell command is no longer one you can run.

A run of tool calls with no prose between it and the next folds to a line
(`· 23 tool calls  Bash ×11 · Read ×8 · Edit ×4`), because a turn is routinely
forty calls with three sentences threaded through them and the sentences are
what you came for. `o` unfolds them, `t` shows reasoning where a harness left
any in the clear.

## Status

Working: `sources`, `inspect`, `stats`, `index`, search, `file`, `show`, `tui`,
`completions`, over Claude Code, Codex and omp. 974 sessions read in 0.2s; a cold
index of 722 MB takes 0.4s and a warm refresh 0.13s. In the TUI a keystroke
costs 6–30 ms of search and under 1 ms of drawing, and the largest transcript in
the corpus (14 MB) opens in 11 ms.

Being ported from the previous single-harness version: the MCP server, insights
and redaction.

## License

MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
