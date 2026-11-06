# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Settings and prelude that every tape of the captures starts with.
# scripts/render.sh runs vhs from this folder, with the antiq under test first
# in PATH and ANTIQ_DEMO_ROOT naming the demo world that scripts/world.sh built.

Set Shell "bash"
Set FontSize 28
Set Padding 40
# 1900 pixels at this font size and padding give 105 columns, above the 100
# from which antiq lays its tables out in the wide form. render.sh measures the
# columns with tapes/00-columns.tape and refuses any other number.
Set Width 1900
Set TypingSpeed 35ms
Set WaitTimeout 120s

# The colours are the 16 named ANSI colours that antiq writes, so this theme
# decides the shades. The magenta is the accent of the project page.
Set Theme { "name": "antiq", "black": "#0f0f14", "red": "#ef6487", "green": "#5eca89", "yellow": "#fdd877", "blue": "#65aef7", "magenta": "#b49cff", "cyan": "#43c1be", "white": "#e5e5e5", "brightBlack": "#9ca3af", "brightRed": "#ef6487", "brightGreen": "#5eca89", "brightYellow": "#fdd877", "brightBlue": "#65aef7", "brightMagenta": "#cbbcff", "brightCyan": "#43c1be", "brightWhite": "#ffffff", "background": "#0f0f14", "foreground": "#e5e5e5", "selection": "#2d2640", "cursor": "#b49cff" }

Hide
Type `export PS1='$ ' && unset COLUMNS && cd "$ANTIQ_DEMO_ROOT/experiments" && clear`
Enter
Show
