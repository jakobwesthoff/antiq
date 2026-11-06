# Architecture decision records

Each numbered file in this directory records one decision. ADRs are
created with `adrs new --no-edit "<title>"` in Nygard format. ADR 0001
records the decision to keep them; this file holds the rules for writing
them and may be changed in place.

## What goes into an ADR

An ADR says three things:

- why the decision had to be made at all,
- what was decided,
- what follows from it, if that is more than the decision itself.

Nothing else goes in. A section is not filled because the template has
it, and no sentence is there because it reads well.

## Sections

**Title.** The decision itself, as an imperative: "Write the tool in
Rust", not "Programming language".

**Context.** Why the question came up: the problem or situation that
forced a choice. A few sentences. Background the reader does not need
for this decision stays out.

**Decision.** What was decided, and the reason for the choice if there
is one. An alternative is mentioned only when it helps to understand the
decision: the obvious choice was not taken, or several options were
equally viable and one was picked. One short sentence each.

**Consequences.** Only effects that go beyond the decision: what it
costs, what it rules out, what it requires. If the consequences would
only restate the decision, the section reads `None further.`, so that
`adrs doctor` stays usable.

Date and status stay as `adrs` writes them.

## How to write

- One statement per sentence. Several short sentences beat one that
  carries several things.
- Give the information the reader needs. Do not add prose around it.
- No invented reasons. Some choices are a matter of preference and carry
  no reason; the ADR then gives none.
- No history: not who decided, not when or where it was discussed, not
  what was proposed before. Git and the todos keep that.
- Open points go into the todo vault, not into an ADR. An ADR names what
  it leaves undecided only where a reader would otherwise assume it is
  covered.

## One decision per ADR

Parts go into the same ADR when they only make sense together, so that
changing one means rethinking the others. Parts that can be replaced on
their own get their own ADR, even when they were decided at the same
moment.

An ADR is superseded as a whole. If two independent choices shared one,
replacing either would force the other to be restated in the new ADR.
For example, typed errors in the library and errors with added context
in the binary form one error-handling design and share an ADR. The
argument parser can be swapped without touching that design, so it has
its own.

## Changing a decision

An ADR that has been pushed is not rewritten. A changed decision gets a
new ADR (`adrs new --no-edit --supersedes <number> "<title>"`), and the
old one only gains its status link.

Until the first release, ADRs are rewritten in place instead when the
design changes. The exception ends with the first release.