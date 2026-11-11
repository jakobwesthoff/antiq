# 72. Show every name from the user's files in one escaped form

Date: 2026-11-11

## Status

Accepted

## Context

A file or folder name can hold a newline, an ESC or a bidi control.
Printed as it is, such a name can end a line, forge another one, move the
cursor or make a name look different from what it is. Escaping was done
call site by call site, so a site that forgot it printed the raw name.
Error output and the pipe lines of `grep` did not escape at all.

## Decision

Paths and names from the user's files are shown in one escaped form in
all text output: result lines, dry-run lines, notes, warnings, errors,
excerpts of the output of other programs and the lines of `grep` in a
pipe.

The Unicode control characters (category Cc), the bidi controls U+202A to
U+202E and U+2066 to U+2069, and U+2028 and U+2029 are each shown as `%`
and the upper-case hex digits of each of their UTF-8 bytes. A `%` is shown
as `%25` and a byte that is not UTF-8 as `%XX`, so the shown form is
unambiguous and can be turned back into the exact bytes.

One library module, `src/shown.rs`, holds the rule.

## Consequences

A name with a control character is not the text `antiq` reads back as a
path. `--json` carries the structured fields (`piece`, `path`,
`candidates` and the like) as they are.
