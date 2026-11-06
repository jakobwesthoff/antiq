---
title: "Publish the antiq entry of the homepage once the project page is online"
kind: chore
component: project
status: blocked
origin: request
---
# Publish the antiq entry of the homepage once the project page is online

The homepage repository `jakobwesthoff.github.io` (local clone in
`~/Development/github/jakobwesthoff/jakobwesthoff.github.io`) lists antiq
on its projects page with the commit `94e3c11` "List antiq on the
projects page" (`src/pages/projects.astro`: name, tagline, colour
`#b49cff`, link `https://antiq.westhoffswelt.de`). That commit sits on
the local `main` on top of `cb4fed0` and is not pushed, because the link
would lead nowhere until antiq's project page is online. Everything
before it was pushed on 2026-11-06.

## To do

Once antiq's first release is out and `https://antiq.westhoffswelt.de`
answers, push the homepage's `main` (`git -C
~/Development/github/jakobwesthoff/jakobwesthoff.github.io push origin
main`) after checking that the commit is still on top of `origin/main`,
and check that the deploy workflow ("Deploy to GitHub Pages") succeeds
and the projects page shows the entry.

## Relations

- Blocked by: antiq's first release and its project page going online at antiq.westhoffswelt.de
