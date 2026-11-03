# 57. Build the picker from ntropy's picker

Date: 2026-11-03

## Status

Accepted

## Context

`find` opens a picker (ADR 0021). Of the options researched, ntropy's
own picker draws on `/dev/tty` and is tied to ntropy only through its
candidate type; `nucleo-picker` has no preview and fixed colours,
`skim` pulls in about 200 crates, and a ratatui picker was rejected in
ntropy.

## Decision

antiq's picker starts as a copy of ntropy's picker. It offers
unearthing (Enter), a view of a piece's contents, printing the path
(`--print`), forking and pinning or unpinning. The contents show as a
tree inside the list: a key expands the selected piece in place, and
members whose paths match the query show as its children.

## Consequences

None further.
