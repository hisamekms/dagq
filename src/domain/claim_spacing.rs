//! The spacing of new claims while the load hold is on (ADR-t1479-1): a
//! supervisor with `--max-load` makes no new claim until `claim_spacing`
//! seconds have passed since the queue's latest `run_claimed`, whichever
//! supervisor recorded it, and judges the load again before the next one.
//! The 1-minute load average does not hold the build of a run claimed a
//! moment ago, so claims made back to back on a low load overshoot
//! `--max-load` a few minutes later.

/// The default of `claim_spacing`, in seconds: the load of a claimed run's
/// build shows in the 1-minute load average about 2 minutes after its claim
/// (finding 36; the records are in the claim hold's design).
pub const DEFAULT_CLAIM_SPACING_SECS: usize = 180;

/// The spacing in effect: `spacing_secs` while the load hold is on, none
/// when it is off (`max_load` `None`) or the spacing is 0.
pub fn in_effect(max_load: Option<f64>, spacing_secs: usize) -> Option<usize> {
    (max_load.is_some() && spacing_secs > 0).then_some(spacing_secs)
}

/// When the next new claim may be made, in Unix milliseconds: the latest
/// claim's time plus the spacing in effect; `None` when none is in effect
/// or no claim is recorded (a claim may be made now).
pub fn next_claim_ms(spacing_secs: Option<usize>, last_claim_ms: Option<i64>) -> Option<i64> {
    let secs = i64::try_from(spacing_secs?).ok()?;
    Some(last_claim_ms?.saturating_add(secs.saturating_mul(1000)))
}

/// Whether a new claim waits for the spacing at `now_ms`.
pub fn waits(next_claim_ms: Option<i64>, now_ms: i64) -> bool {
    next_claim_ms.is_some_and(|next| now_ms < next)
}

/// The whole seconds from `since_ms` to `now_ms`, 0 when the clock went back.
pub fn waited_secs(since_ms: i64, now_ms: i64) -> u64 {
    u64::try_from(now_ms.saturating_sub(since_ms).div_euclid(1000)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_load_hold_with_a_spacing_spaces_the_claims() {
        assert_eq!(in_effect(Some(16.0), 180), Some(180));
        assert_eq!(in_effect(None, 180), None);
        assert_eq!(in_effect(Some(16.0), 0), None);
        assert_eq!(in_effect(None, 0), None);
    }

    #[test]
    fn the_next_claim_waits_for_the_spacing_after_the_latest_claim() {
        let last = 1_000_000;
        let next = next_claim_ms(Some(180), Some(last));
        assert_eq!(next, Some(last + 180_000));
        assert!(waits(next, last));
        assert!(waits(next, last + 179_999));
        assert!(!waits(next, last + 180_000));
        // No claim recorded, or no spacing in effect: claim now.
        assert_eq!(next_claim_ms(Some(180), None), None);
        assert_eq!(next_claim_ms(None, Some(last)), None);
        assert!(!waits(None, last));
    }

    #[test]
    fn the_wait_is_counted_in_whole_seconds() {
        assert_eq!(waited_secs(1_000, 62_999), 61);
        assert_eq!(waited_secs(5_000, 1_000), 0);
    }
}
