---
title: "Find why the worker-thread panic test of the live console failed once"
kind: bug
component: cli
origin: implementation
tags: [testing]
---
# Find why the worker-thread panic test of the live console failed once

`cli::console::live::tests::a_panic_on_a_worker_thread_is_written_whole_on_a_clean_screen_and_the_calling_thread_unwinds`
(`src/cli/console/live/tests.rs`) failed once in a full `just check`
run on 2026-11-06 (unit tests of the `antiq` binary, 399 passed, 2
failed; the other failure was a real one of the change under test) and
passed when run alone and in the next full run. A second agent saw it fail in
several `cargo test --bin antiq` runs, each time while another test in
the same binary was failing, and never otherwise; that points to the
process-wide panic hook being shared between tests. The log shows only
`FAILED` without the panic text of the assertion. Not reproduced yet, so
whether it is a flaky test or a race in the live console is unknown.

## To do

Run the test in a loop under load (`cargo test --bin antiq` several
hundred times, or with other tests in parallel) until it fails, keep the
output, and fix the cause: a test that waits on timing instead of a gate
or channel, or an ordering bug between the panic hook and the renderer.
