# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# List the recipes.
default:
    @just --list

# Run every check a commit has to pass: formatting, lints and tests.
check: fmt-check clippy test

# Check the formatting without changing a file.
fmt-check:
    cargo fmt --check

# Format the sources.
fmt:
    cargo fmt

# Lint every target, with warnings as errors.
clippy:
    cargo clippy --all-targets -- -D warnings

# Run the whole test suite.
test:
    cargo test

# List the lines under src/ changed since `base` that no test runs.
coverage base:
    mkdir -p target/llvm-cov
    cargo llvm-cov --all-targets --lcov --output-path target/llvm-cov/lcov.info
    scripts/uncovered-lines.sh {{ base }}

# Render the images of the README and the project page from real runs.
shots:
    scripts/render.sh

# Render only the demo video of the page and the GIF of the README.
demo:
    scripts/render.sh --demo

# Render the social preview image from assets/social/og.html with Chrome.
og:
    scripts/og.sh

# Render Archie's emblem as terminal text, one file per cell shape and glyph set.
emblem-ansi:
    scripts/emblem-ansi.sh

# Test the builder of the demo world that the images are rendered from.
shots-test:
    cargo build --release
    scripts/world.test.sh
