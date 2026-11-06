---
title: "Give each error code a docs url"
kind: improvement
component: lib
status: needs-discussion
origin: discussion
tags: [error-handling, ux]
---
# Give each error code a docs url

Every explained error and warning of the library has a stable code such as
`antiq::push::no_target`, shown as `code` in the JSON error and read from
miette's `Diagnostic`. The same trait has a `url()` method, which antiq does
not set. A url per code would send a reader from the message to a page that
explains that code.

## Open

- Which page a code points to. The README, now a tutorial and a reference,
  has no section that explains the error codes; it needs a stable anchor per
  code first.
- Whether the JSON error gets the url as a key next to `code` and `fix`, and
  whether the console block shows it.
