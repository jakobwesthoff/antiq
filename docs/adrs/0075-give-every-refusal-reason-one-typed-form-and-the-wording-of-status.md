# 75. Give every refusal reason one typed form and the wording of status

Date: 2026-11-18

## Status

Accepted

## Context

A reason why `archive` or `import` leaves a folder alone existed in three
forms. `status` held it as a typed `Refusal`. `archive` and `import` held
it as a struct with a kind and a text made when the reason was built. The
progress events carried the text as a string, and the cards recognised a
pin by comparing that string with a text they built themselves.

The views worded the same reason differently. A pin read `kept by
.antiq-pin` in `status` and `pinned: <path>` in the `refused` lines of
`archive` and in the JSON `text`. A nested collection read `nested antiq
collection` in `status` and `nested collection` elsewhere. `archive` joined
several reasons with `, `, `status` with `; `.

## Decision

The wording of the WHAT column of `status` is the wording of every view,
the lines of `archive` and `import` on a pipe, the dry run, the cards and
the JSON `text`.

The library holds a reason as `Reason`, a layered type: `Walk` wraps the
`Refusal` of the survey, `Pinned` holds the pin file and `Empty` holds the
piece. Its `Display` is the wording. The survey keeps `Vec<Refusal>`, so a
pin stays a verdict of its own there, and `RefusalKind` has no `pinned` or
`empty`. The JSON of a reason keeps the keys `kind`, `path`, `line` and
`text`, with `text` from `Display`.

One source of truth runs from the detection of a reason to its output. The
library already words its errors and warnings. The cards match
`Reason::Pinned` and compare no string.

Several reasons on one line are joined by `; `. A reason holds `: ` and a
path, and a path may hold commas.

A piece without a file that counts reads `empty: no file counts` in
`archive` and `import`.

## Consequences

The JSON `text` of a `pinned`, `empty` or `nested_collection` reason
changes. A script that tells reasons apart reads `kind`, which stays as it
was.

The module `progress` depends on `commands::archive`, because the progress
event of a refused piece carries its `Reason` rather than a text.
