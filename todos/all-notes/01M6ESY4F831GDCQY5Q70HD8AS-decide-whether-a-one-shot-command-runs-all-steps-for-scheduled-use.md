---
title: "Decide whether a one-shot command runs all steps for scheduled use"
kind: investigation
component: cli
status: needs-discussion
horizon: someday
origin: discussion
---
# Decide whether a one-shot command runs all steps for scheduled use

Raised in the first CLI design round on 2026-11-02: a single command
(working name `run`) that archives idle folders, moves archives between
tiers and syncs to the store in one go, so launchd or cron can call one
command. The alternative is scheduling each command on its own. Neither
playground script is scheduled today; both run by hand.

## Decisions

- 2026-11-02, user: not decided now; comes back when scheduling comes up.

## Revisit when

Scheduled runs of the tool are designed, or the command set of the first
version is settled.
