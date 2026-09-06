# whence

A local search engine and time machine for coding-agent transcripts. Reads what
Claude Code and Codex leave on disk and makes it queryable. Open source, fully
local, no telemetry — never add a network call that ships transcript content
anywhere.

The name is the question the tool answers: *whence did this come?* — which
session changed this file, and why.

## Architecture

```
source/{claude,codex} ──▶ model ──┬──▶ tokenize ──▶ index + search (tantivy)
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
- `src/render/` is the seam for markdown. `Markdown` (pulldown-cmark) parses it
  into `Doc`; `Plain` remains as the escape hatch that passes markup through
  untouched. Both must keep fenced code verbatim — it is never reflowed.
  **The index reads markdown source, never a rendering** — excerpt offsets have
  to point into the stored text. Inline markup becomes `Emphasis` on spans,
  never characters, so a line stays free to break inside a bold run; tables are
  the one construct that gives up its inline markup, because a clipped column
  cannot carry emphasis ranges through the clip.
- `src/tokenize.rs` is the analyzer both the index and every query go through,
  and the only place jieba is named. It cuts text into runs of CJK and runs of
  everything else, and **only the CJK runs reach jieba** — the dictionary is
  ~100 ms to load, once per process, and tokenizing the query is what triggers
  it, so a query with no Chinese in it used to spend 100 ms of its 110 ms
  building a dictionary it never consulted. The split is a property of the text,
  never of the caller, which is what keeps a document and a query cut the same
  way; a term the two spell differently can never be found. Punctuation and
  whitespace are dropped — 58.7% of what jieba emitted over the corpus, 29% of
  the searchable index, and `" "` as a term meant every multi-word query
  intersected a posting list holding every document and highlighted every space
  in the excerpt. Positions stay character offsets, because jieba's search mode
  overlaps a compound word with its pieces and only a real offset can say so.
- `whence show` and `whence inspect` go through `source` directly, not the
  index, so reading a session back never depends on the index being current.
- **`Source::resume` is the one method that hands a transcript back.** The id
  every surface prints is eight characters, which names a session to whence and
  to nothing else; `claude --resume` and `codex resume` want the whole uuid, and
  want it in the directory the session ran in, because that is how both
  harnesses find one. `whence show` prints the command; the reader carries it on
  the right of its header, because reading is where you decide to go back; and
  quitting the TUI leaves it under the `whence show` pointer. The header gives
  up the `cd` first when the line runs out of room (the project is named two
  spans to its left) and then the title (the results list already showed it) —
  the id never shortens, since that is the half you cannot reconstruct. A Claude
  subagent resumes as the conversation that spawned it — the directory its
  `subagents/` folder sits in — because a subagent was never a session you drove
  and `--resume` will not take its id.
- `src/tui/` is the browser. `app.rs` is the state machine and never mentions a
  terminal, so every key is testable; `ui.rs` owns layout, including `wrap` —
  the function `render`'s docs point at. It measures display width and breaks
  mid-run, because Chinese has no spaces to break on; it never reflows a code
  block. The reader loads sessions through `source` using the path the index
  stored on the hit, so it shows what the index deliberately never keeps —
  tool calls above all. Those are *folded*: a run of more than three with no
  prose between them collapses to one line of counts, because a turn is often
  forty calls around three sentences. `o` expands them, and the fold always
  keeps the number of denied or failed calls — that is the one you went looking
  for. Prompts are deliberately **not** rendered as markdown: a prompt is what
  was typed, and this is a tool for reading back what happened.
- **The results list is a tree, not a list** (`tui::Tree`). A query that matches
  a conversation at all usually matches it a dozen times — the same question
  asked in slightly different words all week — and a dozen near-identical rows
  push every *other* conversation off the screen, which is what the list was
  for. So hits are gathered under the session they came from, one row per
  conversation: what tells two apart (when, which project, what it was called)
  plus its best passage and how many matches it holds. `→` opens it onto its
  own matches, `←` shuts it, `⏎` reads whichever row you are on. All four
  arrows are the tree's, always: sharing them with the caret meant `→` opened
  a session while `←` only walked back through what had just been typed, one
  key doing two things depending on a caret nobody was looking at. The query
  box keeps the readline keys it already answered to — `^B`/`^F` move the
  caret, `^A`/`^E`/Home/End its ends. Rank orders the sessions and the
  searcher's
  favourite is what a shut row shows, but *inside* a session the matches read
  forwards, in turn order — a conversation is a sequence and reading it out of
  order is how you lose the thread. Grouping never reorders the hits
  themselves: rank is a fact about the whole corpus, and this is a view over it.
- **A session has to be able to name itself.** Codex records no title at all and
  Claude only sometimes, so the index stores each session's opening prompt
  (`opening` — *stored*, never indexed, or every long session would be counted
  once per document it produced). `Hit::name()` is the title where there is one
  and that prompt otherwise; without it the row that answers "which
  conversation is this?" is blank exactly where it matters.

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
- **A result has to explain itself.** `Results::terms` is the query as the
  analyzer cut it — `src/model.rs` is three words and all of them are required —
  and every `Hit` carries the words it actually matched, paired with the words
  you typed. The two differ exactly when the query was relaxed (`normlize` finds
  `normalize`), which is the case where an unmarked excerpt is least
  explicable. `Hit::why` is the one sentence both surfaces print, and
  `Found::Title` covers the other silent case: a hit that matched only the
  boosted title, whose excerpt otherwise reads as though it has nothing to do
  with the query. The reader marks those words through the whole conversation,
  because the results list can only say *that* something matched. Latin marks
  land on word boundaries — `rs`, cut from `src/model.rs`, is also inside
  `first` and `parse` — while CJK stays a substring, since jieba cuts finer than
  anyone types. A mark is bold **and** underlined, in the terminal and in the
  TUI both: half of a transcript is bold already, so bold alone is not a mark.
  The reader adds reverse video on top, because there a match has to be found
  while scrolling past it rather than read.
- Excerpts are located by searching the stored body for the query's own tokens,
  not with tantivy's `SnippetGenerator`, which re-tokenizes every hit it is
  shown: at the TUI's limit of 200 that was 47 ms of jieba per keystroke on a
  Chinese query against 4 ms. It also could never highlight a fuzzy or regex
  match, which report no terms — so this is one path where there were two.
- `INDEX_FORMAT` covers the analyzer and the stored fields, not just the
  searchable schema. Changing how text is cut changes what the terms *are*, and
  an index full of the old ones answers new queries with silence; a field the
  code reads and the index never wrote is silence of the same kind. Bump it and
  the stale index is rebuilt (0.3 s over the whole corpus), never queried.
- The TUI is single-threaded on purpose: events are drained before each redraw,
  so a held-down key costs one search and one transcript read rather than one
  per repeat. Measured, that is 1–4 ms per keystroke against the real index
  (it was 6–47 ms before excerpts stopped going through `SnippetGenerator`) and
  11 ms to open the largest transcript in the corpus — do not add a thread
  before a measurement says one is needed.
- Reading a `TestBackend` buffer back is not a screenshot: the cell after a wide
  character is skipped by ratatui's diff, so it holds whatever the last frame
  left there. Draw into a fresh terminal, and skip the cell after any symbol
  wider than one column, or CJK assertions fail on garbage a real terminal never
  shows.
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
markdown renderer, the index and searcher (whose schema carries a `harness` field,
so results say where a hit came from and `--harness` filters work), and
`whence sources | inspect | stats | index | search | file | show | completions |
tui`.

Next, ported from the previous single-harness version: `mcp`, `insights`,
`redact`.
