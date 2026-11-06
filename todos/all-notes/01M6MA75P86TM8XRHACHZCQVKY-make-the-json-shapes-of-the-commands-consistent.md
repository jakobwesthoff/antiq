---
title: "Make the JSON shapes of the commands consistent"
kind: bug
component: project
origin: implementation
tags: [architecture]
---
# Make the JSON shapes of the commands consistent

Found by the E4 scenario tests and the C4, C7 and C8 work on main d06a21b. The general JSON rule says a `--dry-run` plan carries `"dry_run": true` and actions of a `kind`; the commands differ:

- a real `push --json` carries `"dry_run": false` right after `command` (the scenario envelope rejects it);
- the dry-run actions of `push` (`write_marker`, `mirror`) and of `init` use the key `action` where every other command uses `kind`; a real `init` also emits an empty `actions` key the spec names for the dry run only;
- the `repairs` of a real run are `{"kind": "repair", "repair": ...}` in archive, forget and pin, but bare `{"repair": ...}` in unearth and fork (check rebuild, push and import too).

## Fix

Tests first, through the binary in JSON mode, one per command and shape: no `dry_run` key outside a dry run, `kind` in every action, every repair of a real run kind-tagged as in forget. Then the code, and the spec's JSON output section and the command paragraphs in the same commits. Text output stays as it is.

## Relations

- Part of: [Rework antiq v1 to the simplified concept](01M6KFHS2FVQ9APAGQ947FP3CM-rework-antiq-v1-to-the-simplified-concept.md)
