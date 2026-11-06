---
title: "Rework the README and docs into a pitch and a tutorial before the release"
kind: docs
component: project
status: needs-discussion
impact: high
origin: request
---
# Rework the README and docs into a pitch and a tutorial before the release

Before the first release the README and the documentation get a new
structure (Jakob, chat, 2026-11-05). Today they read like a reference:
complete, but a reader has to know what they are looking for. The
content stays; no information is lost. The order and the voice change.

## What Jakob asked for

- **The opening sells the tool.** The title area and the first
  paragraphs tell a reader quickly why they want to look at antiq: they
  have a problem, and antiq solves it. More marketing than today.
- **Name the use cases.** The folder where a developer's experiments pile
  up is one; many developers have such a folder, under whatever name.
  Another is the large context directories of AI conversations and agent
  work, where intermediate data piles up that one may want to keep for
  later.
- **Do not present `~/playground` as a folder everyone has.** It is
  Jakob's name for it. Phrase it as "the folder where your experiments
  pile up" with `~/playground` at most as an example. The same applies
  to the project page's first highlight ("Point it at ~/playground."),
  which needs a rewording in the same pass.
- **The docs become a tutorial.** After the opening, the documentation
  guides the reader step by step with quick successes: each section
  builds on the one before, so a reader can go from front to back and
  understand how antiq works. Not a how-to collection and not a
  reference first.
- **Show real output.** Well-cropped captures of antiq's output between
  the steps, as terminal transcripts or images, so the new console
  design shows.
- **Follow the stored rules for tutorials and how-tos.** Jakob thinks
  such rules exist in his private ntropy vault; look them up together
  when this starts.

## Open points

- Where the reference material goes once the README is a tutorial (a
  reference section at the end, or separate docs).
- Which captures, and in which form (images or text transcripts).
- The exact wording of the pitch and the use cases.

The GitHub topics already follow this planned README (2026-11-05):
`ai-agents`, `coding-agents`, `macos` and `linux` are set, so the
reworked README has to state the AI and agent context-directory use case
and both platforms. If it names Claude Code, `claude-code` can replace
`declutter` or `checksum` (the repository holds the maximum of 20).

Do this after the console overhaul and the live progress rows land, so
the captures show the final output.
