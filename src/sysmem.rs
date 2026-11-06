// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Physical memory of the machine.

/// Errors of reading the physical memory.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum SysmemError {
    #[error("the operating system does not report the physical memory")]
    Unavailable,
}

impl crate::error::HasKind for SysmemError {}

/// Bytes of physical memory: `sysconf(_SC_PHYS_PAGES)` times the page size.
pub fn physical_memory() -> Result<u64, SysmemError> {
    // SAFETY: `sysconf` reads a system constant and has no preconditions.
    let (pages, page_size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    // `c_long` is 64 bits wide on 64-bit unix and 32 bits wide elsewhere.
    // The conversion is an identity on the first and a widening on the
    // second, so clippy's complaint on this target does not apply.
    #[allow(clippy::useless_conversion)]
    bytes_from(i64::from(pages), i64::from(page_size))
}

/// `sysconf` answers -1 when it cannot tell, and the product of the two
/// counts has to fit the result.
fn bytes_from(pages: i64, page_size: i64) -> Result<u64, SysmemError> {
    let pages = u64::try_from(pages)
        .ok()
        .filter(|&pages| pages > 0)
        .ok_or(SysmemError::Unavailable)?;
    let page_size = u64::try_from(page_size)
        .ok()
        .filter(|&size| size > 0)
        .ok_or(SysmemError::Unavailable)?;
    pages.checked_mul(page_size).ok_or(SysmemError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_memory_is_positive_and_page_aligned() {
        let bytes = physical_memory().expect("the test machine reports its memory");
        assert!(bytes > 0);
        assert_eq!(bytes % 4096, 0, "{bytes} is not a multiple of 4096");
    }

    #[test]
    fn bytes_from_multiplies_pages_and_page_size() {
        assert_eq!(bytes_from(1000, 4096).expect("valid"), 4_096_000);
    }

    #[test]
    fn bytes_from_rejects_a_failed_page_count() {
        assert!(matches!(
            bytes_from(-1, 4096),
            Err(SysmemError::Unavailable)
        ));
    }

    #[test]
    fn bytes_from_rejects_a_failed_page_size() {
        assert!(matches!(
            bytes_from(1000, -1),
            Err(SysmemError::Unavailable)
        ));
    }

    #[test]
    fn bytes_from_rejects_an_overflow() {
        assert!(matches!(
            bytes_from(i64::MAX, 4096),
            Err(SysmemError::Unavailable)
        ));
    }
}
