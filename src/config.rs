// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The collection's config file.

/// Errors of the config file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {}

impl crate::error::HasKind for ConfigError {}
