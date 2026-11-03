#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# The checks every commit has to pass.

set -eu

cd "$(git rev-parse --show-toplevel)"

cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
