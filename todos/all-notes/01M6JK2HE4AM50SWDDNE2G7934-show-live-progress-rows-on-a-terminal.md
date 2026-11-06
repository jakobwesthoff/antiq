---
title: "Show live progress rows on a terminal"
kind: feature
component: cli
origin: request
tags: [architecture]
---
# Show live progress rows on a terminal

Long runs of `archive`, `unearth` and `push` show their pieces as live rows on a terminal: one row per piece with its path, its size, a progress bar, the operation running on it (packing, verifying, storing, removing) and one detail, such as the files checked. A row takes one line when the path fits and two lines otherwise, the full path above the bar. Without a terminal only the finished lines are printed.
