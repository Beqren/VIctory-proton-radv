//! What a refresh reads, by 4 KiB page (`VICTORY_READ_PAGES=1`; off by
//! default): the planning of `Shadow::copy_stale` taken out as a function
//! of the copy's notes and the journal's answer, and the counts of the
//! per-second line "reading by page".
//!
//! Without the switch a stale 64 KiB block is read whole. With it a block
//! the write watch reported by page (`crate::watch::fine`) is read only in
//! the pages written since the copy last took it; a block known only whole
//! (the guards, our own writes told whole) or never taken is read whole as
//! before. `copied` stays by 64 KiB block: after the plan every page of the
//! block written up to this asking is in the copy, which is what its
//! number says.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::watch::PAGE;
use crate::watch::fine::SUB;

const KERNEL_PAGE: usize = PAGE / SUB;

/// Runs closer than this many bytes are read as one, the bytes between
/// read too (`VICTORY_READ_PAGES_GAP=<KiB>`; 0 by default: only runs that
/// touch). Every read has a fixed cost (a system call through the kernel's
/// copy, little with `VICTORY_COPY_PLAIN=1`), and every range sent to the
/// card is a copy command and a hash pass of its own.
pub(crate) fn gap() -> usize {
    static GAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *GAP.get_or_init(|| {
        std::env::var("VICTORY_READ_PAGES_GAP").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(0) << 10
    })
}

/// Since the last line: blocks read whole because the copy never took
/// them; blocks read whole because the journal knew them only whole;
/// blocks read by page, their 4 KiB pages read and left out; reads made
/// (runs), their bytes, bytes of gaps read with them; with the check on,
/// 4 KiB pages left out looked at, and of them found differing.
static COUNTS: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
const FIRST: usize = 0;
const WHOLE: usize = 1;
const FINE: usize = 2;
const FINE_READ: usize = 3;
const FINE_LEFT: usize = 4;
const RUNS: usize = 5;
const BYTES: usize = 6;
const GAP_BYTES: usize = 7;
pub(crate) const CHECKED: usize = 8;
pub(crate) const CHECK_DIFFERED: usize = 9;

/// [`Shadow::took_back`](crate::shadow::Shadow::took_back) since the last
/// line: calls, blocks the copy counted as taken (its reread left out),
/// blocks it could not (a report it had not taken, or never taken), their
/// bytes left out of rereads.
pub(crate) static TOOK_BACK: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

pub(crate) fn count(i: usize, n: u64) {
    COUNTS[i].fetch_add(n, Relaxed);
}

/// A copy's plan for `[from, to)` (inside the copy `[start, end)`): which
/// bytes to read, as title-address runs in order, and the 4 KiB pages it
/// leaves out of stale blocks it reads in part (for the check).
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Plan {
    pub(crate) runs: Vec<(usize, usize)>,
    pub(crate) left_out: Vec<usize>,
}

/// Plans a refresh of `[from, to)` of the copy `[start, end)`: `copied`
/// holds the asking each 64 KiB block was last taken at (block `base`
/// first; 0: never), `written` the journal's answer (block, last asking)
/// in block order, `now` this asking. `pages(block, since, written)` gives
/// the block's pages written after `since` as bits and whether the journal
/// knew them by page ([`crate::watch::fine::stale_pages`]). Blocks wholly
/// in `[from, to)` are noted as taken at `now`; a block only partly in it
/// is read in part and stays stale for the rest, as before.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan(
    (from, to): (usize, usize),
    (start, end): (usize, usize),
    copied: &mut [u64],
    base: usize,
    now: u64,
    written: &[(usize, u64)],
    mut pages: impl FnMut(usize, u64, u64) -> (u16, bool),
    gap: usize,
    check: bool,
) -> Plan {
    let mut plan = Plan::default();
    if from >= to {
        return plan;
    }
    let mut writes = written.iter().peekable();
    let push = |runs: &mut Vec<(usize, usize)>, a: usize, b: usize| {
        let (a, b) = (a.max(from), b.min(to));
        if a >= b {
            return;
        }
        match runs.last_mut() {
            Some(last) if a <= last.1 + gap => {
                if a > last.1 {
                    count(GAP_BYTES, (a - last.1) as u64);
                }
                last.1 = last.1.max(b);
            }
            _ => runs.push((a, b)),
        }
    };
    for block in from / PAGE..=(to - 1) / PAGE {
        let write = match writes.peek() {
            Some(&&(p, w)) if p == block => {
                writes.next();
                w
            }
            _ => 0,
        };
        let Some(at) = copied.get_mut(block - base) else {
            continue;
        };
        let mask = if *at == 0 {
            count(FIRST, 1);
            u16::MAX
        } else if write > *at {
            let (mask, by_page) = pages(block, *at, write);
            if by_page {
                count(FINE, 1);
                count(FINE_READ, u64::from(mask.count_ones()));
                count(FINE_LEFT, u64::from(mask.count_zeros()));
            } else {
                count(WHOLE, 1);
            }
            mask
        } else {
            continue;
        };
        if (block * PAGE).max(start) >= from && ((block + 1) * PAGE).min(end) <= to {
            *at = now;
        }
        if mask == u16::MAX {
            push(&mut plan.runs, block * PAGE, (block + 1) * PAGE);
            continue;
        }
        let mut k = 0;
        while k < SUB {
            if mask & (1 << k) == 0 {
                if check {
                    let page = block * PAGE + k * KERNEL_PAGE;
                    if page + KERNEL_PAGE > from && page < to {
                        plan.left_out.push(page);
                    }
                }
                k += 1;
                continue;
            }
            let first = k;
            while k < SUB && mask & (1 << k) != 0 {
                k += 1;
            }
            push(&mut plan.runs, block * PAGE + first * KERNEL_PAGE, block * PAGE + k * KERNEL_PAGE);
        }
    }
    count(RUNS, plan.runs.len() as u64);
    count(BYTES, plan.runs.iter().map(|r| (r.1 - r.0) as u64).sum());
    plan
}

/// The per-second line, after "asking and reading" (diagnostics builds).
pub(crate) fn line(frames: f64) {
    let c: [f64; 10] = std::array::from_fn(|i| COUNTS[i].swap(0, Relaxed) as f64 / frames);
    let t: [f64; 4] = std::array::from_fn(|i| TOOK_BACK[i].swap(0, Relaxed) as f64 / frames);
    if c.iter().chain(&t).all(|&n| n == 0.0) {
        return;
    }
    tracing::info!(
        "reading by page per frame (VICTORY_READ_PAGES): stale blocks read whole as never taken {:.0}, read whole as known \
         only whole (the guards, writes told whole) {:.0}, read by 4 KiB page {:.0} ({:.0} pages read, {:.0} left out, \
         {:.1} MB not read); reads {:.0}, {:.1} MB, of it {:.1} MB of gaps joined (VICTORY_READ_PAGES_GAP); pages left out \
         checked {:.0}, differing {:.1} (VICTORY_REFRESH_CHECK=1); took back (VICTORY_TOOK_BACK): {:.0} calls, {:.0} blocks \
         left out of the reread ({:.1} MB), {:.0} could not be",
        c[FIRST],
        c[WHOLE],
        c[FINE],
        c[FINE_READ],
        c[FINE_LEFT],
        c[FINE_LEFT] * KERNEL_PAGE as f64 / 1e6,
        c[RUNS],
        c[BYTES] / 1e6,
        c[GAP_BYTES] / 1e6,
        c[CHECKED],
        c[CHECK_DIFFERED],
        t[0],
        t[1],
        t[3] / 1e6,
        t[2],
    );
}

#[cfg(test)]
mod tests {
    use super::{KERNEL_PAGE, Plan, plan};
    use crate::watch::PAGE;

    /// The journal by page for these tests: block → mask written after
    /// any copy's asking (or whole).
    fn by_page(masks: &[(usize, u16)]) -> impl FnMut(usize, u64, u64) -> (u16, bool) + '_ {
        move |block, _, _| masks.iter().find(|m| m.0 == block).map_or((u16::MAX, false), |m| (m.1, true))
    }

    const B: usize = 100;

    fn at(block: usize, page: usize) -> usize {
        block * PAGE + page * KERNEL_PAGE
    }

    #[test]
    fn a_write_to_one_page_reads_that_page_only() {
        let mut copied = vec![1u64; 4];
        let p = plan((at(B, 0), at(B + 4, 0)), (at(B, 0), at(B + 4, 0)), &mut copied, B, 5, &[(B + 1, 3)], by_page(&[(B + 1, 1 << 6)]), 0, false);
        assert_eq!(p.runs, vec![(at(B + 1, 6), at(B + 1, 7))]);
        assert_eq!(copied, vec![1, 5, 1, 1]);
    }

    #[test]
    fn two_pages_of_one_block_are_two_reads_or_one_with_a_gap() {
        let masks = [(B, (1 << 2) | (1 << 5))];
        let mut copied = vec![1u64; 1];
        let p = plan((at(B, 0), at(B + 1, 0)), (at(B, 0), at(B + 1, 0)), &mut copied, B, 5, &[(B, 3)], by_page(&masks), 0, true);
        assert_eq!(p.runs, vec![(at(B, 2), at(B, 3)), (at(B, 5), at(B, 6))]);
        // The check looks at the fourteen pages left out.
        assert_eq!(p.left_out.len(), 14);
        assert!(!p.left_out.contains(&at(B, 2)) && !p.left_out.contains(&at(B, 5)));
        let mut copied = vec![1u64; 1];
        let p = plan((at(B, 0), at(B + 1, 0)), (at(B, 0), at(B + 1, 0)), &mut copied, B, 5, &[(B, 3)], by_page(&masks), 2 * KERNEL_PAGE, false);
        assert_eq!(p.runs, vec![(at(B, 2), at(B, 6))]);
    }

    #[test]
    fn a_write_across_a_page_boundary_reads_both_pages_as_one_run() {
        // Pages 15 of one block and 0 of the next: one read.
        let mut copied = vec![1u64; 2];
        let masks = [(B, 1 << 15), (B + 1, 1)];
        let p = plan((at(B, 0), at(B + 2, 0)), (at(B, 0), at(B + 2, 0)), &mut copied, B, 5, &[(B, 3), (B + 1, 3)], by_page(&masks), 0, false);
        assert_eq!(p.runs, vec![(at(B, 15), at(B + 1, 1))]);
        assert_eq!(copied, vec![5, 5]);
    }

    #[test]
    fn a_range_with_partial_first_and_last_pages_is_cut_to_it() {
        let from = at(B, 3) + 100;
        let to = at(B, 9) + 200;
        let mut copied = vec![1u64; 1];
        let masks = [(B, (1 << 3) | (1 << 9) | (1 << 12))];
        let p = plan((from, to), (at(B, 0), at(B + 1, 0)), &mut copied, B, 5, &[(B, 3)], by_page(&masks), 0, false);
        assert_eq!(p.runs, vec![(from, at(B, 4)), (at(B, 9), to)]);
        // Only part of the block is in the range: still stale.
        assert_eq!(copied, vec![1]);
    }

    #[test]
    fn blocks_never_taken_or_known_only_whole_are_read_whole_and_clean_ones_not_at_all() {
        let mut copied = vec![0, 4, 1, 2];
        // B: never taken; B+1: written at 3, taken at 4: clean; B+2: the
        // guards (whole); B+3: not written since.
        let p = plan((at(B, 0), at(B + 4, 0)), (at(B, 0), at(B + 4, 0)), &mut copied, B, 9, &[(B + 1, 3), (B + 2, 3)], by_page(&[]), 0, false);
        assert_eq!(p.runs, vec![(at(B, 0), at(B, 0) + PAGE), (at(B + 2, 0), at(B + 3, 0))]);
        assert_eq!(copied, vec![9, 4, 9, 2]);
    }

    #[test]
    fn the_journal_by_page_reads_what_the_journal_by_block_reads_when_every_page_is_written() {
        let mut a = vec![1u64, 1, 0];
        let mut b = a.clone();
        let w = [(B, 3), (B + 1, 3)];
        let range = (at(B, 0) + 10, at(B + 3, 0) - 10);
        let whole = plan(range, (at(B, 0), at(B + 3, 0)), &mut a, B, 5, &w, by_page(&[]), 0, false);
        let fine = plan(range, (at(B, 0), at(B + 3, 0)), &mut b, B, 5, &w, by_page(&[(B, u16::MAX), (B + 1, u16::MAX)]), 0, false);
        assert_eq!(whole, fine);
        assert_eq!(whole, Plan { runs: vec![range], left_out: vec![] });
        assert_eq!(a, b);
    }
}
