---
title: "Set the comment text of status, the cards and grep in italics"
kind: improvement
component: cli
status: blocked
horizon: next
origin: discussion
tags: [ux]
---
# Set the comment text of status, the cards and grep in italics

Decided on 2026-11-18: the explanatory comment text in the console
output is set in italics, in addition to the styling it has today, so
it reads apart from the data around it.

## Scope

- The WHAT column of `antiq status` (`src/cli/cmd/status.rs`, `table`),
  dim today.
- The reasons under the finished line of a card that was refused,
  skipped or unfinished (`src/cli/console/cards.rs`,
  `finished_lines`), plain today with the command in bold.
- The right column of `grep` (`src/cli/cmd/grep.rs`, `piece_row` and
  `file_row`), plain today.

## Decisions

- 2026-11-18, user: the three places above become italic.
- 2026-11-18, user: the result column of the cards (`1 234 files`,
  `refused`, `skipped`) and the marks of `find` and `list` (`damaged`,
  `incomplete`, `newer format`, `unreadable`) stay as they are. They
  name a state, the counterpart of the STATE column of `status`, not a
  comment.
- 2026-11-18, user: this runs after the wording of the reasons is
  unified, since both change `status.rs` and `cards.rs`.

## Proposal

Add an italic style to `src/cli/console/theme.rs` and combine it with
the style each text has. Italics then drop out wherever the paint does
(no colour, `NO_COLOR`, a pipe). `src/cli/version_screen.rs` writes its
italics as a raw escape (`ITALIC`); moving it onto the theme style is
optional.

## Done when

- The three texts are italic on a colour terminal and unchanged in a
  pipe.
- `docs/specs/spec-v1.md` describes the italics where it describes the
  styling of each of the three.
- The CHANGELOG has an entry under Unreleased.

## Relations

- Depends on: [Give refusal reasons one typed form and the wording of status](01M7Q4N592G2SPEDZRSFSMC9C3-give-refusal-reasons-one-typed-form-and-the-wording-of-status.md)
