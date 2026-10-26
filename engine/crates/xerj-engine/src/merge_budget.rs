//! Merge admission against the process memory budget (#1122).
//!
//! An 8 GiB host used to take the 8 GiB AUTO step, so the process budget
//! was the whole machine. A segment-merge storm then allocated a working
//! set measured at 5–9× the input bytes and the process died. These
//! functions size and defer batches from the resolved budget. They do
//! not allocate.
//!
//! A budget whose 95% watermark still covers a historical 512 MiB batch
//! (8× working set plus the 2 GiB memtable floor) keeps that batch and
//! the configured parallelism. That is the 8 GiB tier and above — a
//! 16 GiB host and larger — so those machines merge as they did before.

/// Historical input ceiling for one merge batch. The AUTO path used this
/// whenever `XERJ_MAX_PROCESS_MEMORY_MB` was unset.
pub const HISTORICAL_BATCH_CAP_BYTES: u64 = 512 * 1024 * 1024;

/// Smallest input cap. Below this a tier can stop making progress.
pub const MIN_BATCH_CAP_BYTES: u64 = 32 * 1024 * 1024;

/// Planning multiplier for one batch's heap. Mailbox-shaped merges were
/// measured at 5–9× the input bytes (`merge_parsed` held 191 MB on a
/// 20k-doc index). 8× is the figure a batch must fit under.
pub const MERGE_WORKING_SET_NUMERATOR: u64 = 8;

/// Upper bound on the input bytes of one merge batch.
///
/// `budget_bytes == 0` means the budget is unknown (no governor). That
/// keeps the historical 512 MiB ceiling. A positive budget that can hold
/// the historical batch also keeps 512 MiB. A smaller budget uses
/// `budget / 64`, clamped to [`MIN_BATCH_CAP_BYTES`]..=[`HISTORICAL_BATCH_CAP_BYTES`].
/// A 4 GiB budget — the AUTO cap on an 8 GiB host — is 64 MiB.
pub fn merge_batch_input_cap_bytes(budget_bytes: u64) -> u64 {
    if budget_bytes == 0 || historical_batch_fits(budget_bytes) {
        return HISTORICAL_BATCH_CAP_BYTES;
    }
    (budget_bytes / 64).clamp(MIN_BATCH_CAP_BYTES, HISTORICAL_BATCH_CAP_BYTES)
}

/// `true` when a 512 MiB batch plus the 2 GiB memtable floor still sits
/// under the 95% watermark of `budget_bytes`.
fn historical_batch_fits(budget_bytes: u64) -> bool {
    let historical = HISTORICAL_BATCH_CAP_BYTES
        .saturating_mul(MERGE_WORKING_SET_NUMERATOR)
        .saturating_add(2 * 1024 * 1024 * 1024);
    budget_bytes.saturating_mul(95) / 100 >= historical
}

/// How many merge batches may run at once.
///
/// A budget that still uses the historical 512 MiB batch returns
/// `configured` unchanged (at least 1). A smaller budget limits
/// concurrency so the batches' estimated working sets share a quarter
/// of the budget, and never drops below one.
pub fn merge_parallelism_under_budget(configured: usize, budget_bytes: u64) -> usize {
    let configured = configured.max(1);
    let cap = merge_batch_input_cap_bytes(budget_bytes);
    if cap >= HISTORICAL_BATCH_CAP_BYTES {
        return configured;
    }
    let per = cap.saturating_mul(MERGE_WORKING_SET_NUMERATOR).max(1);
    let slots = usize::try_from((budget_bytes / 4) / per).unwrap_or(usize::MAX);
    configured.min(slots.max(1))
}

/// Whether one batch may start, ignoring the one-batch drain exception.
///
/// `rss_bytes == 0` means RSS is unknown. `watermark_bytes == 0` means
/// the RSS check is off. Either one starts the batch: a guess must not
/// stall merges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeBatchDecision {
    Start,
    Defer,
}

pub fn decide_merge_batch(
    input_bytes: u64,
    rss_bytes: u64,
    watermark_bytes: u64,
) -> MergeBatchDecision {
    if watermark_bytes == 0 || rss_bytes == 0 {
        return MergeBatchDecision::Start;
    }
    let estimate = input_bytes.saturating_mul(MERGE_WORKING_SET_NUMERATOR);
    if rss_bytes.saturating_add(estimate) >= watermark_bytes {
        MergeBatchDecision::Defer
    } else {
        MergeBatchDecision::Start
    }
}

/// Which of `input_bytes` (one entry per selected batch, policy order)
/// starts in this pass.
///
/// On a budget that still uses the 512 MiB batch, every entry starts.
/// On a smaller budget, a batch whose estimated working set would cross
/// the watermark waits for the next check. The first batch still starts:
/// its input is already capped, and a merge is what retires the
/// per-segment caches that hold RSS up. Zero batches would leave that
/// RSS in place.
pub fn throttle_merge_batches(
    input_bytes: &[u64],
    budget_bytes: u64,
    rss_bytes: u64,
    watermark_bytes: u64,
) -> Vec<bool> {
    let small = merge_batch_input_cap_bytes(budget_bytes) < HISTORICAL_BATCH_CAP_BYTES;
    let mut start = Vec::with_capacity(input_bytes.len());
    let mut started = 0usize;
    for bytes in input_bytes {
        if !small {
            start.push(true);
            continue;
        }
        let fits =
            decide_merge_batch(*bytes, rss_bytes, watermark_bytes) == MergeBatchDecision::Start;
        if fits || started == 0 {
            start.push(true);
            started += 1;
        } else {
            start.push(false);
        }
    }
    start
}

/// Estimated peak working set of a merge batch whose inputs total
/// `input_bytes`: the same `input * MERGE_WORKING_SET_NUMERATOR` that
/// [`decide_merge_batch`] uses.
pub fn merge_working_set_bytes(input_bytes: u64) -> u64 {
    input_bytes.saturating_mul(MERGE_WORKING_SET_NUMERATOR)
}

/// Process-wide merge admission against the RSS watermark (#2427).
///
/// [`throttle_merge_batches`] plans one index's pass from an RSS snapshot
/// and always lets that index's first batch start. With hundreds of indices
/// (autoindex) every index started one, and none of them saw the others'
/// working sets until they had allocated. This is checked under the
/// governor's merge ledger at launch time instead:
///
/// * `watermark == 0` (breaker off) or `observed_rss == 0` (no probe):
///   start.
/// * No merge in flight anywhere in the process: start one, so merging can
///   always make progress, unless the breaker is engaged and the merge is a
///   background one. A `forced` merge (`_forcemerge`) is still allowed as
///   the single in-flight merge while engaged; it is a deliberate operator
///   request and it waits on its own.
/// * Breaker engaged: defer. Background merges are the one memory consumer
///   the node can postpone, so they wait for the drain.
/// * Otherwise start only if `observed_rss + reserved + request` stays under
///   the watermark.
pub fn decide_merge_admission(
    watermark: u64,
    observed_rss: u64,
    reserved: u64,
    in_flight: u64,
    breaker_engaged: bool,
    request: u64,
    forced: bool,
) -> bool {
    if watermark == 0 || observed_rss == 0 {
        return true;
    }
    if in_flight == 0 {
        return !breaker_engaged || forced;
    }
    if breaker_engaged {
        return false;
    }
    observed_rss
        .saturating_add(reserved)
        .saturating_add(request)
        < watermark
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    #[test]
    fn merges_are_throttled_under_a_small_budget() {
        // AUTO cap on an 8 GiB host: half of RAM, the setting that kept
        // the Windows 10 field report up (`XERJ_MAX_PROCESS_MEMORY_MB=4096`).
        let budget = 4 * GIB;
        let cap = merge_batch_input_cap_bytes(budget);
        assert_eq!(
            cap,
            64 * MIB,
            "4 GiB / 64 = 64 MiB, not the 512 MiB default"
        );
        assert_eq!(
            merge_parallelism_under_budget(4, budget),
            2,
            "four workers would hold 4 × 512 MiB; the budget allows two"
        );

        let watermark = budget * 95 / 100;
        // RSS already within 32 MiB of the watermark. One capped batch
        // still runs (it retires segments). The other two wait.
        let rss = watermark - 32 * MIB;
        let plan = throttle_merge_batches(&[cap, cap, cap], budget, rss, watermark);
        assert_eq!(plan, vec![true, false, false]);

        // The same batches with headroom all start. Parallelism is a
        // separate bound, applied by the caller.
        let open = throttle_merge_batches(&[cap, cap, cap], budget, 256 * MIB, watermark);
        assert_eq!(open, vec![true, true, true]);

        // Unknown RSS does not stall the drain.
        assert_eq!(
            decide_merge_batch(cap, 0, watermark),
            MergeBatchDecision::Start
        );
        assert_eq!(decide_merge_batch(cap, rss, 0), MergeBatchDecision::Start);
    }

    #[test]
    fn a_normal_host_keeps_the_historical_batch_and_parallelism() {
        // The 8 GiB tier (16 GiB of RAM and up, under 64 GiB) and the
        // 16 / 32 GiB tiers all cover a 512 MiB batch under the watermark.
        for budget in [8 * GIB, 16 * GIB, 32 * GIB] {
            assert_eq!(
                merge_batch_input_cap_bytes(budget),
                HISTORICAL_BATCH_CAP_BYTES
            );
            assert_eq!(merge_parallelism_under_budget(4, budget), 4);
            let watermark = budget * 95 / 100;
            let plan = throttle_merge_batches(
                &[HISTORICAL_BATCH_CAP_BYTES; 3],
                budget,
                watermark,
                watermark,
            );
            assert_eq!(
                plan,
                vec![true, true, true],
                "budget {budget} must not defer a historical batch"
            );
        }
        // No governor: the pre-change fallback.
        assert_eq!(merge_batch_input_cap_bytes(0), HISTORICAL_BATCH_CAP_BYTES);
        assert_eq!(merge_parallelism_under_budget(1, 0), 1);
    }

    #[test]
    fn the_batch_cap_floors_at_thirty_two_mebibytes() {
        assert_eq!(merge_batch_input_cap_bytes(GIB), MIN_BATCH_CAP_BYTES);
        assert_eq!(merge_batch_input_cap_bytes(2 * GIB), MIN_BATCH_CAP_BYTES);
    }

    #[test]
    fn merge_admission_is_open_when_the_breaker_is_off_or_rss_is_unknown() {
        assert!(decide_merge_admission(
            0,
            10_000 * MIB,
            0,
            5,
            true,
            4_000 * MIB,
            false
        ));
        assert!(decide_merge_admission(
            2_850 * MIB,
            0,
            0,
            5,
            true,
            4_000 * MIB,
            false
        ));
    }

    #[test]
    fn merge_admission_always_lets_one_merge_run_when_not_engaged() {
        // Even an estimate that alone exceeds the watermark starts when no
        // other merge is in flight: merging must make progress.
        assert!(decide_merge_admission(
            2_850 * MIB,
            2_700 * MIB,
            0,
            0,
            false,
            400 * MIB,
            false
        ));
    }

    #[test]
    fn merge_admission_counts_other_in_flight_reservations() {
        let wm = 2_850 * MIB;
        // One 47 MiB batch (376 MiB working set) already reserved.
        let reserved = merge_working_set_bytes(47 * MIB);
        assert!(decide_merge_admission(
            wm,
            2_000 * MIB,
            reserved,
            1,
            false,
            reserved,
            false
        ));
        // At 2,200 MiB RSS a second one would cross the watermark.
        assert!(!decide_merge_admission(
            wm,
            2_200 * MIB,
            reserved,
            1,
            false,
            reserved,
            false
        ));
        // And a third does not fit at all once two are reserved.
        assert!(!decide_merge_admission(
            wm,
            2_000 * MIB,
            2 * reserved,
            2,
            false,
            reserved,
            false
        ));
    }

    #[test]
    fn merge_admission_defers_background_merges_while_engaged() {
        let wm = 2_850 * MIB;
        // Engaged: no background merge starts, not even the first.
        assert!(!decide_merge_admission(
            wm,
            2_900 * MIB,
            0,
            0,
            true,
            8 * MIB,
            false
        ));
        assert!(!decide_merge_admission(
            wm,
            1_000 * MIB,
            0,
            1,
            true,
            8 * MIB,
            false
        ));
        // A forced merge may run alone while engaged, but not beside another.
        assert!(decide_merge_admission(
            wm,
            2_900 * MIB,
            0,
            0,
            true,
            8 * MIB,
            true
        ));
        assert!(!decide_merge_admission(
            wm,
            2_900 * MIB,
            64 * MIB,
            1,
            true,
            8 * MIB,
            true
        ));
    }

    #[test]
    fn merge_working_set_matches_the_batch_decision() {
        assert_eq!(
            merge_working_set_bytes(10),
            10 * MERGE_WORKING_SET_NUMERATOR
        );
        assert_eq!(merge_working_set_bytes(u64::MAX), u64::MAX);
    }
}
