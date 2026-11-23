---
title: "Tmux stand-in test fails under the load of the full test suite"
kind: bug
component: cli
impact: low
origin: implementation
tags: [testing]
---
# Tmux stand-in test fails under the load of the full test suite

`cli::emblem::tmux::tests::tmux_is_asked_for_the_format_and_its_answer_is_read`
(`src/cli/emblem/tmux.rs`) failed once in a full `just check` on
2026-11-23 and passed on the rerun. Run alone eight times on the same
tree it passed eight times. The failure was `left: None`, `right:
Some(ClientTerminal { termtype: Some("ghostty 1.3.2"), cell_pixels:
Some((15, 39)) })`: `ask_program` got no answer from the stand-in
script. Seen while checking commit `cbe5415` (find and grep search live
pieces), which does not touch this module; the test dates from
`b904997`.

## Problem

Not reproduced in isolation, so the cause is a hypothesis. Two fit the
symptom:

- `ask_program` gives up after `ANSWER_WITHIN` (500 ms, `tmux.rs` l. 16).
  Under the load of a full parallel test run a `/bin/sh` stand-in can
  take longer to start and answer.
- The test writes the script and executes it at once. If another test
  thread forks while the script's write descriptor is open, the exec can
  fail with "Text file busy" (ETXTBSY), which `ask_program` turns into
  `None`.

## Suggested fix

Find out which by repeating the full suite under load and logging the
error `ask_program` swallows. For the first, let the test pass a longer
limit (a test seam on the limit); for the second, write the script
through a file that is closed before any exec, or retry an ETXTBSY
spawn in the test helper.
