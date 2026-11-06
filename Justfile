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
