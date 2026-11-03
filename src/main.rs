// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Output goes through `cli::output`, whose functions return `io::Result`.
// A `println!` panics when the reader of the pipe is gone.
#![deny(clippy::print_stdout, clippy::print_stderr)]

mod cli;

use std::process::ExitCode;

/// The most open files a run asks for. macOS refuses a soft limit above
/// `OPEN_MAX`, which is 10240.
const WANTED_OPEN_FILES: u64 = 10_240;

fn main() -> ExitCode {
    // First, before anything opens a file: a scheduled run starts with a
    // soft limit of 256 under launchd, and a run over many pieces holds
    // more files open than that.
    raise_open_file_limit();
    let mut out = cli::output::Output::stdio();
    ExitCode::from(cli::run(std::env::args_os().collect(), &mut out))
}

/// Raises the soft limit on open files towards `WANTED_OPEN_FILES`. A
/// failure leaves the inherited limit in place: it only matters for large
/// runs, and nothing can be printed before the output rules are known.
/// Returns the soft limit that is in force afterwards.
fn raise_open_file_limit() -> Option<u64> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

    let current = getrlimit(Resource::Nofile);
    let raised = raised_limit(current.current, current.maximum).filter(|wanted| {
        let limit = Rlimit {
            current: Some(*wanted),
            maximum: current.maximum,
        };
        setrlimit(Resource::Nofile, limit).is_ok()
    });
    raised.or(current.current)
}

/// The soft limit to ask for, or `None` when the current one is enough.
/// `None` as a limit means unlimited.
fn raised_limit(soft: Option<u64>, hard: Option<u64>) -> Option<u64> {
    let wanted = hard.map_or(WANTED_OPEN_FILES, |hard| hard.min(WANTED_OPEN_FILES));
    match soft {
        Some(soft) if soft < wanted => Some(wanted),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

    #[test]
    fn a_low_soft_limit_is_raised_to_the_wanted_one() {
        assert_eq!(raised_limit(Some(256), None), Some(WANTED_OPEN_FILES));
        assert_eq!(
            raised_limit(Some(256), Some(1 << 20)),
            Some(WANTED_OPEN_FILES)
        );
    }

    #[test]
    fn the_hard_limit_caps_the_wanted_one() {
        assert_eq!(raised_limit(Some(256), Some(4096)), Some(4096));
    }

    #[test]
    fn a_sufficient_or_unlimited_soft_limit_stays() {
        assert_eq!(raised_limit(Some(WANTED_OPEN_FILES), None), None);
        assert_eq!(raised_limit(Some(1 << 20), None), None);
        assert_eq!(raised_limit(None, None), None);
        assert_eq!(raised_limit(Some(4096), Some(4096)), None);
    }

    /// Puts the soft limit back when the test ends, also by panic.
    struct RestoreLimit(Rlimit);

    impl Drop for RestoreLimit {
        fn drop(&mut self) {
            setrlimit(Resource::Nofile, self.0).expect("the old limit can be restored");
        }
    }

    #[test]
    fn the_soft_limit_is_above_256_after_raising_it_from_256() {
        let before = getrlimit(Resource::Nofile);
        assert!(
            before.maximum.is_none_or(|hard| hard > 256),
            "the test needs a hard limit above 256"
        );
        let _restore = RestoreLimit(before);
        setrlimit(
            Resource::Nofile,
            Rlimit {
                current: Some(256),
                maximum: before.maximum,
            },
        )
        .expect("a soft limit can be lowered");

        let after = raise_open_file_limit();

        assert!(
            after.is_some_and(|soft| soft > 256),
            "soft limit: {after:?}"
        );
        assert_eq!(getrlimit(Resource::Nofile).current, after);
    }
}
