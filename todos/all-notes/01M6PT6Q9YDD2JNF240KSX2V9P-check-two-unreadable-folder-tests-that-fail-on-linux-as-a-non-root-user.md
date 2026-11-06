---
title: "Check two unreadable-folder tests that fail on Linux as a non-root user"
kind: investigation
component: lib
status: needs-discussion
origin: implementation
tags: [testing, error-handling]
---
# Check two unreadable-folder tests that fail on Linux as a non-root user

Not confirmed yet. On 2026-11-05 an agent auditing tests for clock and
environment dependence ran the whole suite in a Linux container as a
non-root user and reported two tests that pass on macOS but fail there.
The lead has not reproduced it; which container image and Linux version
were used is not recorded. Whether this is a bug in antiq on Linux or an
artefact of that container is open.

## What was reported

- `commands::import::tests::a_folder_that_cannot_be_opened_is_an_error`
  (`src/commands/import.rs`): the import fails with `ImportError::Pin`
  ("cannot look for a pin in ...") instead of the expected `Open` error.
- `commands::status::tests::a_directory_inside_a_leftover_that_cannot_be_read_is_a_warning_and_the_rest_is_named`
  (`src/commands/status.rs`): `status` gives 0 warnings instead of 1.

Both tests make a directory unreadable and expect antiq to report it.

## Next step

Reproduce on a real Linux machine (and in a container) as a non-root
user. If it reproduces, find out whether antiq checks for a pin before
it opens the folder on Linux, and why the unreadable directory gives no
warning there; then fix the code or the tests. If it does not reproduce,
delete this todo.
