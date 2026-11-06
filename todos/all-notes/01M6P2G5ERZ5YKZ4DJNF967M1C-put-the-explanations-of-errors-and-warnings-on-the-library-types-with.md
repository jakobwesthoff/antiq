---
title: "Put the explanations of errors and warnings on the library types with miette"
kind: refactor
component: lib
status: blocked
origin: discussion
tags: [error-handling, ux]
---
# Put the explanations of errors and warnings on the library types with miette

After the console overhaul (U1) lands, the texts that explain an error
or a warning move from the CLI (`src/cli/error_block.rs`,
`src/cli/notice.rs`, the status findings) onto the library's error and
warning types, so the terminal block, the pipe lines and `--json` read
one source. Decided with Jakob on 2026-11-05 in several asqr rounds.

## Decisions

- **miette as the data model.** Each error enum derives `thiserror::Error`
  and `miette::Diagnostic` next to each other. `#[error("...")]` is the
  message, `#[diagnostic(help("..."), code(antiq::<area>::<kind>))]`
  carries the fix and a stable code, warnings use `severity(Warning)`.
  Fields are interpolated as in `#[error]`.
- **No separate "why".** The reason goes into the message itself; the
  block has no "Why it happened" section any more. The message reads as
  title plus reason, e.g. "No push target is configured, so push has
  nowhere to mirror the antiq collection to".
- **Our renderer stays.** The CLI reads only the `Diagnostic` trait
  (message, `help()`, `code()`, `severity()`) and draws antiq's own
  blocks in `src/cli/console/`; "What antiq was doing" still comes from
  the run's context. miette's graphical handler is not used.
- **JSON** gets the same parts: `message`, `fix` (from `help`), the code
  as the kind, with code marked by backticks as in Markdown, no span
  structures. antiq is unreleased, so the JSON shape may change.
- An own derive crate (`#[explain(why, fix)]`) was considered and is not
  needed once the why lives in the message.

## Open points

- How the code (`antiq::push::no_target`) maps to today's JSON `kind`
  (`no_target`): keep the short kind, or switch to the full code.
- Whether `code` also gets a `url` to the docs once the README rework
  (todo 01M6NZRSXPYMZVV5WAM0QCNZZJ) has stable anchors.

## Relations

- Blocked by: the console overhaul U1 landing on main
