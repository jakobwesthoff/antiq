---
title: "Give refusal reasons one typed form and the wording of status"
kind: improvement
component: lib
horizon: next
origin: discussion
tags: [ux, architecture]
---
# Give refusal reasons one typed form and the wording of status

Found on 2026-11-18 while comparing the comment columns of the views. A
reason why `archive` or `import` leaves a piece alone exists in three
forms in the code, and the views word it differently. The WHAT column of
`antiq status` has the best wording. It becomes the reference for every
view, the pipe lines and the JSON `text`, and the reason gets one typed
form from detection to output.

## Problem

Read in the code at `ec7d5a2` on 2026-11-18.

| | `status` WHAT | cards | `archive` pipe line | JSON `text` |
|---|---|---|---|---|
| pin | `kept by .antiq-pin` | `pinned, so it stays as it is. antiq unpin X releases it` | `refused X: pinned: X/.antiq-pin` | `pinned: X/.antiq-pin` |
| nested | `nested antiq collection: …` | `nested collection: …` | `nested collection: …` | `nested collection: …` |
| several reasons | joined by `; ` | one paragraph each | joined by `, ` | list |

How a reason travels today:

- `status`: `survey::Verdict::Refused(Vec<Refusal>)`, typed as
  `Refusal { kind: RefusalKind, path, line }` with a `Display`
  (`src/commands/survey.rs`). The CLI rewrites the nested text in
  `src/cli/cmd/status.rs`, `reason_text`.
- `archive` and `import`: `Reason { kind: &'static str, path: String,
  text: String, line }` in `src/commands/archive/report.rs`. The kind is
  a string and the text is rendered on construction (`From<&Refusal>`,
  `Reason::pinned`, `Reason::empty`, and struct literals in
  `src/commands/import.rs` around lines 649 and 669).
  `RefusedPiece::reasons_text` and import's own `reasons_text` (used in
  the error `cannot import {folder}: …`) join with `, `.
- Progress events carry strings: `progress::Outcome::Refused(Vec<String>)`
  in `src/progress.rs`. The cards spot a pin by comparing the string
  with `format!("pinned: {name}/{PIN_FILE}")` in
  `src/cli/console/cards.rs`, `reason_text`.

## Decisions

- 2026-11-18, user: the wording of the WHAT column of `status` is the
  reference.
- 2026-11-18, user: breaking changes are fine, the JSON `text` included,
  with a CHANGELOG entry. antiq has few users yet.
- 2026-11-18, user: one typed reason in the library whose `Display` is
  the wording. Rejected: library data with the words only in the CLI
  (the library error `cannot import X: …` needs the words, the JSON
  would lose `text`, and errors and warnings are worded in the library
  too), and keeping strings with a rewrite in the CLI (the string
  comparison stays).
- 2026-11-18, user: the type is layered,
  `Reason { Walk(Refusal), Pinned { pin }, Empty { piece } }`. The
  survey keeps `Vec<Refusal>`, so `status` cannot hold a pin as a
  reason. Rejected: adding `Pinned` and `Empty` to `RefusalKind`.
- 2026-11-18, user: several reasons on one line are joined by `; `
  everywhere. A reason holds `: ` and a path, and paths may hold commas,
  so a comma cannot tell two reasons from two parts of one.
- 2026-11-18, user: the cards say `kept by .antiq-pin. antiq unpin <piece>
  releases it` for a pinned named piece, with the `-C` of the run in the
  command as today.
- 2026-11-18, user: the pipe lines of `archive` and `import` take the
  new wording too.

## Proposal

- Replace the string `Reason` with the layered enum. Its `Display`
  gives the one wording: the `Display` of `Refusal` says
  `nested antiq collection: <dir>` (which retires `reason_text` in
  `status.rs`), `Pinned` says `kept by .antiq-pin`.
- A hand-written `Serialize` keeps the JSON keys `kind`, `path`, `line`
  and `text`, with `text` from `Display`.
- A newtype over a slice of reasons whose `Display` joins with `; `. It
  replaces both `reasons_text` functions, and the `archive` pipe lines,
  the dry run (`ArchivePlan::lines`) and the import error use it.
- `progress::Outcome::Refused` carries `Vec<Reason>`. The cards match
  `Reason::Pinned` to add the unpin hint.

## Open questions

- `empty: <piece>` has no counterpart in `status`. Keep it or propose a
  wording in the plan.
- Whether the WHAT text of `Verdict::Pinned` in `status` reuses the
  `Display` of `Reason::Pinned`, so the text lives in one place.

## Done when

- `status`, the cards, the `archive` pipe lines and dry run, the
  `import` error and the JSON `text` show the same text for each
  reason.
- No code compares reason text as a string.
- `docs/specs/spec-v1.md` is updated: the WHAT reasons (around line
  1610, which today fixes the split for nested collections), the
  `archive` lines (around lines 1023 and 1032), the separator (around
  line 2428) and the pin text of the cards (around line 2490).
- The CHANGELOG has an entry under Unreleased that names the changed
  texts in the pipe lines and the JSON.
