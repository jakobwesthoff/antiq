// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Physical memory of the machine.

/// Errors of reading the physical memory.
#[derive(Debug, thiserror::Error)]
pub enum SysmemError {}

impl crate::error::HasKind for SysmemError {}
