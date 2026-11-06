// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The compression pool shared by library tests that pack.

use std::sync::LazyLock;

use crate::pack::pool::Pool;

/// The pool of this machine, sized as in production. Library tests that pack share it, so
/// parallel tests do not each start level-19 encoders.
///
/// It suits tests that need some grant. A test that asserts grant counts
/// or concurrency builds its own `Pool::new`, since what this one hands out
/// depends on the machine and on what other tests hold.
pub fn shared_pool() -> &'static Pool {
    static POOL: LazyLock<Pool> = LazyLock::new(|| {
        Pool::for_machine().expect("the test machine reports its physical memory")
    });
    &POOL
}
