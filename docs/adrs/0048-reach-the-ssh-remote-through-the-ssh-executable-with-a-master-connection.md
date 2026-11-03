# 48. Reach the SSH remote through the ssh executable with a master connection

Date: 2026-11-03

## Status

Accepted

## Context

The SSH remote runs commands on the NAS and transfers live packages
with rsync, which needs a program that behaves like `ssh host command`.
The user's NAS access is described in `~/.ssh/config`
(`docs/research/ssh.md`).

## Decision

antiq runs the system's `ssh`. It starts one master connection per run
with a short, finite `ControlPersist` and a short `ControlPath`, runs
commands as channels of it, and passes rsync
`-e "ssh -S <socket> -o ControlMaster=no"`. It always passes
`BatchMode=yes`, `ConnectTimeout`, `ServerAliveInterval` and
`ServerAliveCountMax`, and a `HostKeyAlias` when one is configured.
Remote arguments are quoted by antiq.

Library clients (russh, ssh2, libssh) cannot hand their connection to
rsync and read the user's SSH configuration partly or not at all.

## Consequences

A sync without changes uses one connection. Reachability is told from
ssh's exit status 255 and its error text.
