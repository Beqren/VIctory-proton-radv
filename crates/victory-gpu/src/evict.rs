//! Which host copies of textures the title fills to let go while the card
//! is full, or while they have not been read for long
//! (`VICTORY_TEXTURE_EVICT`, `descriptor::host::HostHeap::evict`).
//!
//! The pass over the queue of textures picked by index admits nothing new
//! past nine tenths of the card's budget, and the textures already made
//! stay: a made slot keeps its texture whether anything reads it or not,
//! and the sweep (`resource::let_go_idle`) frees only textures no slot
//! shows. In a 150 s run of the opening (2026-11-25) the card held 9.5 GB
//! of textures (6.6 GB of them the title fills) while 68,015 slot makings
//! were refused for room. Here the order in which shown textures give way:
//! the one read longest ago first, never one read in the last few frames,
//! never one shown by a slot whose reads the host cannot see.
//! Pure logic: tested without a host.

/// A host copy that descriptor slots show, as the eviction sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Shown {
    /// What names it (the host texture's address).
    pub(crate) id: usize,
    /// The card's bytes it takes.
    pub(crate) bytes: u64,
    /// When it was last read, as far as the host can tell (a moment of
    /// `crate::host::moment`: frames begun in the high half, tenths of a
    /// second in the low): the latest of its slots' reads through tables,
    /// draws' asking for them by index, and the host's own last use.
    pub(crate) read: u64,
    /// Whether every slot showing it is one whose next read the host sees
    /// (a table's, or one a draw asked for by index): only then is a slot
    /// made null made again when it is next read. A slot only programs the
    /// host cannot follow pick by index would stay null.
    pub(crate) known: bool,
}

/// Whether `moment` lies at least `ms` milliseconds and `frames` frames
/// before `now` (moments as `crate::host::moment` makes them).
pub(crate) fn before(moment: u64, now: u64, ms: u64, frames: u64) -> bool {
    let tenths = (now & 0xffff_ffff).saturating_sub(moment & 0xffff_ffff);
    (now >> 32).saturating_sub(moment >> 32) >= frames && tenths * 100 >= ms
}

/// What a texture must not have been read in to be let go: the current
/// frame and the ones before it, and two seconds.
pub(crate) const KEEP_FRAMES: u64 = 8;
pub(crate) const KEEP_MS: u64 = 2_000;

/// The textures of `shown` to let go at `now`, in order: those read longest
/// ago first (the larger first among those read at the same moment), only
/// ones whose every slot's next read is seen (`known`) and that were not
/// read within [`KEEP_FRAMES`] frames and [`KEEP_MS`]. With `age`
/// (milliseconds and frames), every such texture not read for that long;
/// without, as many as hold `need` bytes.
pub(crate) fn pick(shown: &[Shown], need: u64, now: u64, age: Option<(u64, u64)>) -> Vec<usize> {
    let (ms, frames) = age.unwrap_or((0, 0));
    let mut order: Vec<&Shown> = shown
        .iter()
        .filter(|s| s.known && before(s.read, now, ms.max(KEEP_MS), frames.max(KEEP_FRAMES)))
        .collect();
    order.sort_unstable_by(|a, b| a.read.cmp(&b.read).then(b.bytes.cmp(&a.bytes)).then(a.id.cmp(&b.id)));
    if age.is_some() {
        return order.into_iter().map(|s| s.id).collect();
    }
    let mut freed = 0u64;
    let mut out = Vec::new();
    for s in order {
        if freed >= need {
            break;
        }
        freed += s.bytes;
        out.push(s.id);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A moment of frame `frame`, `tenths` tenths of a second in.
    fn at(frame: u64, tenths: u64) -> u64 {
        frame << 32 | tenths
    }

    fn shown(id: usize, bytes: u64, read: u64, known: bool) -> Shown {
        Shown { id, bytes, read, known }
    }

    #[test]
    fn the_texture_read_longest_ago_goes_first() {
        let now = at(100, 1000);
        let s = [
            shown(1, 10, at(50, 500), true),
            shown(2, 10, at(20, 200), true),
            shown(3, 10, at(70, 700), true),
        ];
        assert_eq!(pick(&s, 15, now, None), [2, 1]);
        assert_eq!(pick(&s, 30, now, None), [2, 1, 3]);
        assert_eq!(pick(&s, 1000, now, None), [2, 1, 3]);
        assert!(pick(&s, 0, now, None).is_empty());
    }

    #[test]
    fn a_texture_read_lately_or_with_unseen_reads_stays() {
        let now = at(100, 1000);
        let s = [
            // Read in the current frame, and three frames ago.
            shown(1, 10, at(100, 1000), true),
            shown(2, 10, at(97, 900), true),
            // Eight frames ago but within two seconds (fast frames).
            shown(3, 10, at(92, 990), true),
            // Long ago, but a slot shows it whose reads the host cannot see.
            shown(4, 10, at(1, 1), false),
            // Long ago.
            shown(5, 10, at(10, 100), true),
        ];
        assert_eq!(pick(&s, u64::MAX, now, None), [5]);
    }

    #[test]
    fn among_textures_read_at_once_the_larger_goes_first() {
        let now = at(100, 1000);
        let s = [shown(1, 10, at(5, 50), true), shown(2, 40, at(5, 50), true), shown(3, 20, at(5, 50), true)];
        assert_eq!(pick(&s, 30, now, None), [2]);
        assert_eq!(pick(&s, 50, now, None), [2, 3]);
    }

    #[test]
    fn by_age_every_texture_unread_that_long_goes() {
        let now = at(1000, 3000);
        let s = [
            // 30 s unread.
            shown(1, 10, at(900, 2700), true),
            // 10 s unread.
            shown(2, 10, at(970, 2900), true),
            // 60 s unread, unseen reads.
            shown(3, 10, at(800, 2400), false),
            // 60 s unread.
            shown(4, 10, at(800, 2400), true),
        ];
        assert_eq!(pick(&s, 0, now, Some((20_000, 0))), [4, 1]);
        assert!(pick(&s, 0, now, Some((120_000, 0))).is_empty());
        // Upstream's frames too: 30 s but 100 frames unread stays.
        assert_eq!(pick(&s, 0, now, Some((20_000, 150))), [4]);
    }
}
