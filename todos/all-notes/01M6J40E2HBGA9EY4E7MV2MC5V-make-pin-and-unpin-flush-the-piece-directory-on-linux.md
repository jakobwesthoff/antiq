---
title: "Make pin and unpin flush the piece directory on Linux"
kind: bug
component: lib
impact: high
origin: implementation
---
# Make pin and unpin flush the piece directory on Linux

On Linux every `pin` and `unpin` fails with "Bad file descriptor" while
it flushes the piece's top directory. Found on 2026-11-04 when the Linux
gate (the `antiq-i03f-linux` container) ran the tests of main for the
first time since I10.

## Problem

`TreeRoot::open_dir_file` (`src/fsx/tree.rs`) opens the directory with
the `SEARCH` flags. On Linux these are `O_PATH`, and `fsync` on an
`O_PATH` descriptor fails with `EBADF`. `pin::flush_piece` is its only
caller. Six tests in `src/pin.rs` fail on Linux; on macOS `O_SEARCH`
allows `fsync`, so they pass there.

## Suggested fix

Open the directory for reading (`O_RDONLY | O_DIRECTORY | O_NOFOLLOW`,
the flags `TreeRoot` uses for listing) in `open_dir_file`, so `fsync`
works on both systems. The six failing Linux tests are the red tests.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
