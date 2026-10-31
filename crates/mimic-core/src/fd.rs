//! Open-file limit decision (spec §4.1 Resources). Pure: the caller reads and applies limits.

/// Value of an unlimited `RLIMIT_NOFILE` (`RLIM_INFINITY`).
pub const UNLIMITED: u64 = u64::MAX;

/// Target used when an unlimited hard limit meets an unreadable `fs.nr_open`.
pub const DEFAULT_NR_OPEN: u64 = 1_048_576;

/// The soft limit to set, or `None` when the current one already reaches the target.
///
/// The target is the hard limit; when that is unlimited, `nr_open` (the kernel's per-process
/// ceiling for descriptors), else [`DEFAULT_NR_OPEN`].
pub fn fd_limit_target(soft: u64, hard: u64, nr_open: Option<u64>) -> Option<u64> {
    let target = if hard == UNLIMITED {
        nr_open.unwrap_or(DEFAULT_NR_OPEN)
    } else {
        hard
    };
    (soft < target).then_some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prx_25_a_finite_hard_limit_above_the_soft_limit_is_the_target() {
        assert_eq!(
            fd_limit_target(1024, 524_288, Some(2_000_000)),
            Some(524_288)
        );
        assert_eq!(fd_limit_target(1024, 524_288, None), Some(524_288));
    }

    #[test]
    fn prx_25_an_unlimited_hard_limit_gives_nr_open() {
        assert_eq!(
            fd_limit_target(1024, UNLIMITED, Some(2_147_483_584)),
            Some(2_147_483_584)
        );
    }

    #[test]
    fn prx_25_an_unlimited_hard_limit_without_nr_open_gives_1048576() {
        assert_eq!(fd_limit_target(1024, UNLIMITED, None), Some(1_048_576));
    }

    #[test]
    fn prx_25_a_soft_limit_at_or_above_the_target_is_unchanged() {
        assert_eq!(fd_limit_target(524_288, 524_288, None), None);
        assert_eq!(fd_limit_target(UNLIMITED, UNLIMITED, Some(1_000)), None);
        assert_eq!(fd_limit_target(2_000_000, UNLIMITED, None), None);
    }
}
