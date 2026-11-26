//! The journal at the kernel's grain (`VICTORY_READ_PAGES=1`; off by
//! default): for blocks the write watch reported, the asking that last
//! found each of its sixteen 4 KiB pages written, so that a refresh reads
//! the pages written since its copy last took the block and not the whole
//! 64 KiB block (`crate::readgrain`).
//!
//! Why: run d4-off (diagnostics, the world, 2026-11-25) read 139-225 MB a
//! frame into host copies in 1,700-2,450 reads, and a sampled compare found
//! only 13-25% of those bytes in 4 KiB pages that differed from the copy.
//! The kernel reports 4 KiB pages (11,168 a frame there), the journal kept
//! only their 64 KiB blocks.
//!
//! Kept beside the journal (`super::Journal`), filled under its lock: a
//! block without an entry has all sixteen pages at the block's number in
//! the journal (blocks the guards report, which know 64 KiB only, our own
//! writes told whole). Numbers only rise, so a page's number read without
//! the journal's lock is at least what it was when the caller asked: a mask
//! read late holds every page it held early (it may hold more, which costs
//! a read and nothing else).
//!
//! Also here: [`took_back`], the journal's side of a copy-back that tells
//! the copy it came from that title memory holds that copy's bytes.

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::{KERNEL_PAGE, PAGE};

/// 4 KiB pages a 64 KiB block holds.
pub(crate) const SUB: usize = PAGE / KERNEL_PAGE;

/// Whether the journal keeps 4 KiB pages and refreshes read by them
/// (`VICTORY_READ_PAGES=1`).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_READ_PAGES").is_some_and(|v| v == "1"))
}

/// Block number → each 4 KiB page's last asking.
pub(crate) struct Fine(BTreeMap<usize, [u64; SUB]>);

static FINE: Mutex<Fine> = Mutex::new(Fine(BTreeMap::new()));

fn fine() -> std::sync::MutexGuard<'static, Fine> {
    FINE.lock().unwrap_or_else(|e| e.into_inner())
}

impl Fine {
    #[cfg(test)]
    pub(crate) const fn new() -> Fine {
        Fine(BTreeMap::new())
    }

    /// The 4 KiB page at title address `at` found written at asking `now`;
    /// `prior` is its block's number in the journal before this note (0:
    /// never written), which the block's other pages keep.
    pub(crate) fn page(&mut self, at: usize, now: u64, prior: u64) {
        let (block, k) = (at / PAGE, at % PAGE / KERNEL_PAGE);
        // An entry stays once made, even with all its pages alike: a block
        // noted page by page within one asking would otherwise lose the
        // pages noted before its entry went (`prior` is read before the
        // journal notes the block). About 128 bytes a block the kernel
        // ever reported.
        self.0.entry(block).or_insert([prior; SUB])[k] = now;
    }

    /// All of `block` found written at the block's number (the guards, a
    /// write told whole): no entry.
    pub(crate) fn whole(&mut self, block: usize) {
        self.0.remove(&block);
    }

    /// The pages of `block` written after asking `since`, as bits (bit `k`:
    /// the `k`th 4 KiB page), and whether the journal knew them by page.
    /// `written` is the block's number as the caller read it from the
    /// journal: a block without an entry is all written if that is after
    /// `since`.
    pub(crate) fn mask(&self, block: usize, since: u64, written: u64) -> (u16, bool) {
        match self.0.get(&block) {
            Some(pages) => (
                pages.iter().enumerate().filter(|&(_, &p)| p > since).fold(0u16, |m, (k, _)| m | 1 << k),
                true,
            ),
            None => (if written > since { u16::MAX } else { 0 }, false),
        }
    }
}

/// The kernel found the 4 KiB page at `at` written at asking `now`
/// (`super::ask_kernel`, `super::writes`), before the journal notes its
/// block. Nothing without [`on`].
pub(super) fn kernel_page(written: &BTreeMap<usize, u64>, at: usize, now: u64) {
    if on() {
        fine().page(at, now, written.get(&(at / PAGE)).copied().unwrap_or(0));
    }
}

/// `block` was found written whole at the journal's next note (the guards).
pub(super) fn whole_block(block: usize) {
    if on() {
        fine().whole(block);
    }
}

/// We wrote `[at, at + len)` (`super::wrote`), noted at asking `now`
/// before the journal notes its blocks: its 4 KiB pages only.
pub(super) fn wrote_range(written: &BTreeMap<usize, u64>, at: usize, len: usize, now: u64) {
    if !on() || len == 0 {
        return;
    }
    let mut f = fine();
    for page in at / KERNEL_PAGE..at.saturating_add(len).div_ceil(KERNEL_PAGE) {
        let at = page * KERNEL_PAGE;
        f.page(at, now, written.get(&(at / PAGE)).copied().unwrap_or(0));
    }
}

/// The pages of `block` a copy that last took it at asking `since` must
/// read again, as bits, and whether the journal knew them by page;
/// `written` is the block's number from the caller's answer of
/// [`super::writes`]. Without [`on`]: all of them if `written` is after
/// `since`.
pub(crate) fn stale_pages(block: usize, since: u64, written: u64) -> (u16, bool) {
    if !on() {
        return (if written > since { u16::MAX } else { 0 }, false);
    }
    fine().mask(block, since, written)
}

/// The copy-back wrote `[at, at + len)` of title memory from one copy's
/// bytes: noted in the journal as written at a new asking, which every
/// other copy of that memory then reads again, and `take(block, prior,
/// now)` is called for each block with the block's number before this note
/// (`prior`, 0 for never) and the new one (`now`), all under the journal's
/// lock. Returns `now`.
///
/// The copy the bytes came from may count the block as taken at `now`
/// only if it had taken it at `prior` or later: then every write of the
/// title's reported before this note is in that copy, and every write the
/// title made before or after the copy-back's that is reported after it
/// gets a number above `now` and is read. A title write that lands between
/// the copy-back's write and its guarding again is such a write, as long as
/// it is reported at all (it faults, or it finds the block open and the
/// guards report the block): numbers are given under the journal's lock, so
/// a report made after this note is numbered after it. That is why the
/// number is taken here, together with `prior`, and not handed in.
#[allow(dead_code)]
pub(crate) fn took_back(at: usize, len: usize, mut take: impl FnMut(usize, u64, u64)) -> u64 {
    let mut j = super::journal();
    j.asked += 1;
    let now = j.asked;
    if len == 0 {
        return now;
    }
    wrote_range(&j.written, at, len, now);
    for block in at / PAGE..at.saturating_add(len).div_ceil(PAGE) {
        let prior = j.written.insert(block, now).unwrap_or(0);
        take(block, prior, now);
    }
    now
}

#[cfg(test)]
mod tests {
    use super::{Fine, SUB};
    use crate::watch::{KERNEL_PAGE, PAGE};

    #[test]
    fn a_write_to_one_page_marks_that_page_only() {
        let mut f = Fine::new();
        let block = 7;
        f.page(block * PAGE + 5 * KERNEL_PAGE + 100, 3, 1);
        assert_eq!(f.mask(block, 1, 3), (1 << 5, true));
        // A copy that took the block at 3 reads nothing.
        assert_eq!(f.mask(block, 3, 3), (0, true));
        // A copy that never took it after 1's write... took it at 0: all
        // pages are older than its asking but page 5; the others at 1.
        assert_eq!(f.mask(block, 0, 3), (u16::MAX, true));
    }

    #[test]
    fn two_pages_of_one_block_and_a_page_reported_twice() {
        let mut f = Fine::new();
        let block = 9;
        f.page(block * PAGE + 2 * KERNEL_PAGE, 4, 0);
        f.page(block * PAGE + 2 * KERNEL_PAGE, 4, 4);
        f.page(block * PAGE + 11 * KERNEL_PAGE, 4, 4);
        assert_eq!(f.mask(block, 2, 4), ((1 << 2) | (1 << 11), true));
        // Reported again later: the later number stands.
        f.page(block * PAGE + 2 * KERNEL_PAGE, 6, 4);
        assert_eq!(f.mask(block, 4, 6), (1 << 2, true));
    }

    #[test]
    fn a_write_straddling_a_page_boundary_marks_both_pages() {
        // The kernel reports both pages of a write across 4 KiB.
        let mut f = Fine::new();
        let block = 3;
        let at = block * PAGE + 4 * KERNEL_PAGE - 8;
        for page in at / KERNEL_PAGE..(at + 16).div_ceil(KERNEL_PAGE) {
            f.page(page * KERNEL_PAGE, 2, 1);
        }
        assert_eq!(f.mask(block, 1, 2), ((1 << 3) | (1 << 4), true));
    }

    #[test]
    fn a_block_told_whole_has_no_entry() {
        let mut f = Fine::new();
        let block = 5;
        f.page(block * PAGE, 2, 0);
        f.whole(block);
        assert_eq!(f.mask(block, 1, 3), (u16::MAX, false));
        assert_eq!(f.mask(block, 3, 3), (0, false));
        for k in 0..SUB {
            f.page(block * PAGE + k * KERNEL_PAGE, 8, 3);
        }
        assert_eq!(f.mask(block, 3, 8), (u16::MAX, true));
        assert_eq!(f.mask(block, 8, 8), (0, true));
    }

    /// Far from anything the other tests or the runtime note: only the
    /// journal is touched, no memory.
    const FAR: usize = 0x7f00_0000_0000;

    #[test]
    fn a_took_back_note_numbers_after_every_earlier_report() {
        let block = FAR / PAGE;
        // A report the copy has not taken: `prior` says so.
        let reported = {
            let mut j = crate::watch::journal();
            j.asked += 1;
            let n = j.asked;
            j.written.insert(block, n);
            n
        };
        let mut seen = Vec::new();
        let now = super::took_back(FAR + 100, 200, |b, prior, now| seen.push((b, prior, now)));
        assert_eq!(seen, vec![(block, reported, now)]);
        assert!(now > reported);
        // The next report of the block is numbered after the note.
        let later = super::took_back(FAR, 1, |_, prior, _| assert_eq!(prior, now));
        assert!(later > now);
    }
}
