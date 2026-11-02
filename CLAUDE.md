# timecapsule

Archive what you no longer touch, find it when you need it. One Rust
crate with a library (`src/lib.rs`) and a command-line binary
(`src/main.rs`), in early design. Architecture decisions live in
`docs/adrs/`; deferred work and open design questions live in the todo
vault at `todos/`.

## Licensing

timecapsule is licensed under the **Mozilla Public License 2.0**
(`LICENSE`). `Cargo.toml` declares it as `license = "MPL-2.0"`.

**Every source file must begin with the MPL-2.0 header comment.** This
covers Rust sources, tests, examples, build scripts and shell scripts.
For Rust files, use line comments:

```rust
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
```

Adapt the comment syntax to the file's language, keeping the wording
verbatim. In a script, the header goes directly below the shebang line.

Manifests and configuration files (`Cargo.toml`, `Cargo.lock`, CI
workflows), Markdown documents and the todo notes carry no header.

Third-party files copied into the repository keep their own license and
header and are listed here as exceptions, together with where their
license text lives. There are none yet.

## Documentation conventions

- ADRs (`docs/adrs/`) follow the rules in `docs/adrs/README.md`. Read
  them before writing or changing an ADR.
- The specification of version 1 lives in `docs/specs/spec-v1.md`. It
  describes what is decided, without the reasons, which the ADRs carry.
- Whatever needs to be addressed or thought about later, including
  things that may end up not being done, goes into the ntropy todo vault
  at `todos/` (format and commands in `todos/README.md`), one note per
  topic, as soon as it comes up. A todo is deleted once completed.
- When a decision lands or code changes make a doc stale, update the
  README, the spec and the affected todos in the same unit of work.
