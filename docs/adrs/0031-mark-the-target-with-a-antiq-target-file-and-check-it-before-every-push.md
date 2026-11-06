# 31. Mark the target with a .antiq-target file and check it before every push

Date: 2026-11-03

## Status

Accepted

Amended by [68. Let push take over the target of its own collection](0068-let-push-take-over-the-target-of-its-own-collection.md)

## Context

A path target on an unmounted share can point at an empty local
directory. antiq cannot tell an unmounted mount point from a real
directory. On macOS an unmounted share takes its `/Volumes/<name>`
directory with it, while an ssh host that answers but lacks the
directory is misconfigured.

## Decision

The target root holds a marker file `.antiq-target`: one line of JSON
with the keys `format`, `id` (the collection ID), `host` (the machine
name) and `path` (the canonical collection root), in that order. Every `push` checks the marker before it mirrors anything. It refuses when the marker is missing
and the directory is not empty, when it names another collection, and
when it names another host or path. `push --force` pushes despite a host
or path mismatch and leaves the marker as it is.

Only `push --init` creates the marker, on an empty directory. antiq
never rewrites it. After renaming the machine or moving the collection
the user edits the `host` or `path` of the marker by hand, or pushes
with `--force`.

A missing directory of a path target is unreachable and antiq never
creates it, also with `--init`. A missing directory of an ssh target is
an error unless `--init` is given.

## Consequences

A renamed machine or a moved collection needs one edit of the marker.
