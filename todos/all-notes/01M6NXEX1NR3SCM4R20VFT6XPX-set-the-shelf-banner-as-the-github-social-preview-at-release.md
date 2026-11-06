---
title: "Set the shelf banner as the GitHub social preview at release"
kind: chore
component: project
status: blocked
origin: request
---
# Set the shelf banner as the GitHub social preview at release

The repository's social preview on GitHub still shows nothing of antiq's
design. The banner that the project page uses as its link preview,
`docs/pages/assets/og.png` (1280x640, about 702 KiB, under GitHub's
1 MB limit), is to become the social preview when antiq is first
released, so the two show the same image.

## What to do

GitHub has no API for the social preview. Upload `docs/pages/assets/og.png`
by hand under the repository's Settings, General, Social preview.

## Relations

- Blocked by: the first release of antiq
