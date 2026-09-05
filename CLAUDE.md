# whence

A local search engine and time machine for coding-agent transcripts. Reads what
Claude Code and Codex leave on disk and makes it queryable. Open source, fully
local, no telemetry — never add a network call that ships transcript content
anywhere.

The name is the question the tool answers: *whence did this come?* — which
session changed this file, and why.

## Architecture

```
source/{claude,codex} ──▶ model ──┬──▶ index + search (tantivy, jieba)
 per-harness parsing              ├──▶ insights (corpus aggregates)
                                  ├──▶ render ──▶ CLI + ratatui TUI
                                  ├──▶ MCP server (redacted)
                                  └──▶ SQLite  (planned)
```

One rule holds the design together: **`src/source/` is the only place that knows
what a transcript looks like on disk, and `src/model.rs` is what every other
surface reads.** A harness that changes its format touches one directory; a new
harness adds one.

- `src/model.rs` is harness-neutral. Keep it free of format details — no field
  exists there because one harness happens to record it.
- `src/source/mod.rs` owns the `Source` trait, the adapter registry and
  discovery. Adding a harness = a directory here + a `Harness` variant + an
  entry in `ALL`. Nothing else in the tree changes.
- `src/source/<harness>/raw.rs` mirrors that harness's on-disk JSON exactly and
  interprets nothing. `normalize.rs` is where the hard work is.
- `src/render/` is the seam for markdown. Everything in a transcript is
  markdown; today `Plain` passes it through (recognising fenced code, which must
  never be reflowed). A real renderer is a new `Render` impl and nothing above
  it changes. **The index reads markdown source, never a rendering** — excerpt
  offsets have to point into the stored text.
- `whence show` and `whence inspect` go through `source` directly, not the
  index, so reading a session back never depends on the index being current.

## Format traps

Real, measured, and each with a test. Do not "simplify" the code that handles
them.

### Claude Code — `tests/claude.rs`

1. **One API response spans many `.jsonl` lines**, all repeating the same
   `requestId` *and the same `usage` block*. Fold by `requestId` and count usage
   once — per-line summing overcounts output tokens by 93%.
2. **`"type": "user"` is usually not the human.** Most such records are
   `tool_result` blocks; others are `isMeta` hook output or `<system-reminder>`
   injections. Only what survives filtering is a prompt.
3. **The signal is in the auxiliary records.** `interruptedMessageId`,
   `toolDenialKind` and `[Request interrupted by user]` mark where a session
   went wrong; `file-history-delta` links a file edit to the message that caused
   it (the "why" that `git blame` lacks).
4. **`file-history-delta.messageId` is a line `uuid`, not `message.id`.** A
   folded `Step` therefore keeps every line uuid it absorbed (`Step::uuids`);
   drop that and `whence file` silently returns nothing.
5. **Thinking blocks are encrypted on disk.** 26,443 of 26,455 carry only a
   `signature` and an empty string. That is the format, not a parser bug.

### Codex — `tests/codex.rs`

1. **`total_token_usage` is cumulative, not a delta.** Every `token_count` event
   repeats the running session total. Summing them inflates output tokens by
   **4268%** on the reference session (1,592,108 against a true 36,451). Session
   totals come from the *last* event, never a sum.
2. **Summing the per-response `last_token_usage` is also wrong, but subtly.** It
   reconciles exactly on 6 of 7 reference sessions. The one that disagrees is
   the one containing `thread_rolled_back`: a rolled-back turn's tokens were
   spent and then un-spent, so they appear per-response but never in the running
   total (37,280 summed against 36,451 charged). Trust the cumulative figure.
3. **Everything is recorded twice.** A prompt is a `message:user` response item
   *and* a `user_message` event; a reply is a `message:assistant` item *and* an
   `agent_message` event. Read the response items — they survive a fork or
   resume — and treat the events as confirmation. Counting both doubles the
   corpus.
4. **`message:user` is usually not the human either.** `AGENTS.md`,
   `<environment_context>` and permission preambles all arrive as user messages.
   Every reference session carries exactly one such injection before the first
   real prompt.
5. **A patch's `call_id` matches nothing.** `patch_apply_end` — the only record
   of a file edit — identifies itself with an internal `exec-<uuid>` sandbox id
   found nowhere else in the file, not with the `call_...` id of the tool call
   that applied it. Attribution is **positional**: a patch always falls between
   its tool call and that call's output. Matching on `call_id` attributes 0 of
   32 edits.
6. **Reasoning is encrypted.** All 89 reasoning items in the corpus carry an
   `encrypted_content` blob, an empty `summary` and no `content`.

### Cross-harness

**The harnesses disagree about what an input token is.** Codex satisfies
`total_tokens == input_tokens + output_tokens`, meaning `input_tokens`
*includes* `cached_input_tokens`. Claude reports cache reads as a separate
bucket outside `input_tokens`. `model::UsageTotals` uses Claude's arrangement —
disjoint buckets — and the Codex adapter subtracts the cached portion back out.
Skip that and a Codex session's input tokens read as several times a comparable
Claude one's.

Parsing stays permissive everywhere: unknown record types and fields are
ignored, and a corrupt line must never abort a file.

## Conventions

- Verify against the real corpora, not just fixtures. `whence stats` over both
  harnesses (974 sessions, ~800 MB) should stay near 0.2s.
- `whence sources` shows which harnesses were found; use it first when something
  seems missing.
- Fuzzy matching dispatches on script and the two halves are not interchangeable:
  CJK gets substring, Latin gets edit distance. `重构` has ~110 neighbours at
  edit distance 1; `tantivy` has zero. Do not unify them.
- Beware when testing search against the real corpus: these transcripts include
  *this* conversation, so a word you just typed will match itself.
- Cross-check token math against `jq` before trusting a change to folding logic.
- `cargo clippy --all-targets` stays clean; `cargo fmt` before committing.
- Transcripts contain API keys and private code. Any export or sharing feature
  must redact by default, and gets a test that a planted key does not survive.
- Do not index tool results. They are most of the corpus and they are where
  pasted secrets and file contents live — keeping them out is a structural
  protection, not an optimization. Codex's `patch_apply_end.changes` carries
  whole file contents and is subject to the same rule: take the paths only.

## Status

Done: the `Source` trait and registry, the Claude and Codex adapters, the
markdown seam, and `whence sources | inspect | stats`.

Next, ported from the previous single-harness version: `index`, `search`,
`show`, `file`, `tui`, `mcp`, `insights`, `redact`, `completions`. The index
schema gains a `harness` field so results say where a hit came from and
`--harness` filters work.
