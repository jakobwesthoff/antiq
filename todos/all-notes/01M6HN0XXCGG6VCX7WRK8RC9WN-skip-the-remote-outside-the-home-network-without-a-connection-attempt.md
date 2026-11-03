---
title: "Skip the remote outside the home network without a connection attempt"
kind: improvement
component: lib
horizon: someday
origin: discussion
tags: [config]
---
# Skip the remote outside the home network without a connection attempt

A machine setting could tell antiq the home network, so that outside it
the remote counts as away at once instead of after the SSH connection
timeout. The v1 spec had it as `machine.network_check` ("router address
of the home network; outside it the remote is away"); on 2026-11-03 the
user took it out of v1. Without it, an unreachable NAS becomes away
after `ConnectTimeout`, and scheduled runs with `--if-reachable` exit 0,
so the check only saves that wait.

## Findings

From the review of the v1 implementation plan (2026-11-03):

- No crate antiq uses reads the default gateway or its hardware
  address. On macOS that means `route -n get default` and `arp -n
  <gateway>` (or a routing-table sysctl); on Linux `/proc/net/route`
  and `/proc/net/arp`.
- The gateway's IP address is easy to read, but private addresses such
  as `192.168.1.1` repeat across networks, so a foreign network can pass
  as home.
- The gateway's MAC address tells networks apart but comes from the ARP
  cache, whose entries expire: 20 minutes on macOS
  (`net.link.ether.inet.max_age` 1200, measured on the user's machine).
  A scheduled run after wake can find no entry.
- Whatever is compared, a network antiq cannot determine must lead to a
  connection attempt, never to "away", or scheduled backups at home are
  skipped silently.

## Revisit when

Waiting for the SSH timeout outside home turns out to bother in
practice.
