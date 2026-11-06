---
title: "Return the claimed version number with a commit error after the rename"
kind: improvement
component: lib
origin: review
---
# Return the claimed version number with a commit error after the rename

Found in the opus diff review of I30 (`sync`) on 2026-11-04, finding Y1. When `remote::commit::commit` fails after the rename into place (for example the final `list_dir` of its confirm step fails), it returns only the error. The version number it claimed and renamed is lost, so `sync` cannot record it in the tracking entry's `committed_versions`.

## Problem

`sync` settles a recorded change whose `meta.N` another machine's cleanup already removed only when `committed_versions` proves the number. After a failed confirm the number is unknown, so the change stays a `version_conflict` with the reason "the change is in the history of version M", although the remote's history contains it. The user then has to answer a decision that has a provable answer.

## Suggested fix

Let `commit()` return the claimed number together with an error that happens after the rename (for example a `CommitError { renamed: Option<u64>, source }`). Have `sync` push that number into `committed_versions`, as it does for `Overtaken` and `Contested`, so the later settle has its proof.
