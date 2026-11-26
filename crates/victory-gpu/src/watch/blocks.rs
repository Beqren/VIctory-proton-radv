//! A set of block numbers kept as bits, for the guards' sets
//! (`super::Guards`): a range of thousands of blocks is a few words of it,
//! not a walk through a tree of every guarded block.
//!
//! After the sibling fork VIctory-7900XTX 7bc7c07 ("guarded blocks kept as
//! bits"). A frame asks the guards about some 8 GB of view memory, 125,000
//! blocks of 64 KiB, nearly all clean (run d-off: 2,529 guard calls a
//! frame, 1,393 of them over ranges that were all clean). With the sets as
//! B-trees each asking collected the range's clean blocks into a list to
//! learn that none was missing.

// The set's whole interface is tested; the guards use part of it.
#![allow(dead_code)]

use std::collections::BTreeMap;

/// Blocks as bits, in pieces of [`Blocks::IN_PIECE`] blocks made on first
/// use. The same answers as a `BTreeSet<usize>` of the same blocks.
pub(crate) struct Blocks(BTreeMap<usize, Box<[u64; Blocks::WORDS]>>);

impl Blocks {
    /// Words a piece: 65,536 blocks, 4 GiB of 64 KiB blocks.
    const WORDS: usize = 1 << 10;
    const IN_PIECE: usize = Blocks::WORDS * 64;

    pub(crate) const fn new() -> Blocks {
        Blocks(BTreeMap::new())
    }

    /// Word `word` (blocks `64 * word` on).
    fn word(&self, word: usize) -> u64 {
        self.0.get(&(word / Blocks::WORDS)).map_or(0, |piece| piece[word % Blocks::WORDS])
    }

    pub(crate) fn contains(&self, block: &usize) -> bool {
        self.word(block / 64) >> (block % 64) & 1 != 0
    }

    /// Whether it was not there.
    pub(crate) fn insert(&mut self, block: usize) -> bool {
        let piece = self.0.entry(block / Blocks::IN_PIECE).or_insert_with(|| Box::new([0; Blocks::WORDS]));
        let word = &mut piece[block % Blocks::IN_PIECE / 64];
        let was = *word;
        *word |= 1 << (block % 64);
        was != *word
    }

    /// Whether it was there.
    pub(crate) fn remove(&mut self, block: &usize) -> bool {
        let Some(piece) = self.0.get_mut(&(block / Blocks::IN_PIECE)) else {
            return false;
        };
        let word = &mut piece[block % Blocks::IN_PIECE / 64];
        let was = *word;
        *word &= !(1 << (block % 64));
        was != *word
    }

    /// Adds blocks `[from, to)`, a word at a time.
    pub(crate) fn extend(&mut self, blocks: std::ops::Range<usize>) {
        let (mut at, to) = (blocks.start, blocks.end);
        while at < to {
            let piece = self.0.entry(at / Blocks::IN_PIECE).or_insert_with(|| Box::new([0; Blocks::WORDS]));
            let word = at / 64 * 64;
            piece[at % Blocks::IN_PIECE / 64] |= in_word(word, at, to);
            at = word + 64;
        }
    }

    /// Whether it holds no block.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.values().all(|piece| piece.iter().all(|&w| w == 0))
    }

    /// Whether every block of `[from, to)` is in it.
    pub(crate) fn all(&self, from: usize, to: usize) -> bool {
        (from / 64..to.div_ceil(64)).all(|word| {
            let want = in_word(word * 64, from, to);
            self.word(word) & want == want
        })
    }

    /// Those of `[from, to)`, in order.
    pub(crate) fn range(&self, range: std::ops::Range<usize>) -> impl Iterator<Item = usize> + '_ {
        let (first, last) = (range.start, range.end.max(range.start));
        self.words(first, last, false)
    }

    /// Those of `[from, to)` that are NOT in it, in order.
    pub(crate) fn missing(&self, range: std::ops::Range<usize>) -> impl Iterator<Item = usize> + '_ {
        let (first, last) = (range.start, range.end.max(range.start));
        self.words(first, last, true)
    }

    fn words(&self, first: usize, last: usize, flip: bool) -> impl Iterator<Item = usize> + '_ {
        (first / 64..last.div_ceil(64)).flat_map(move |word| {
            let bits = self.word(word);
            let bits = if flip { !bits } else { bits };
            BitsOf(bits & in_word(word * 64, first, last)).map(move |bit| word * 64 + bit)
        })
    }
}

/// Of the 64 blocks from `at` on, those in `[from, to)`, a bit each.
fn in_word(at: usize, from: usize, to: usize) -> u64 {
    let low = from.saturating_sub(at).min(64);
    let high = to.saturating_sub(at).min(64);
    if low >= high { 0 } else { (u64::MAX >> (64 - (high - low))) << low }
}

/// The bits of a word that are set, by number.
struct BitsOf(u64);

impl Iterator for BitsOf {
    type Item = usize;
    fn next(&mut self) -> Option<usize> {
        let bit = (self.0 != 0).then(|| self.0.trailing_zeros() as usize)?;
        self.0 &= self.0 - 1;
        Some(bit)
    }
}

#[cfg(test)]
mod tests {
    use super::Blocks;
    use std::collections::BTreeSet;

    /// Against a `BTreeSet` of the same blocks, under random inserts,
    /// removes and extends across words and pieces.
    #[test]
    fn blocks_are_a_set() {
        let mut seed = 11u64;
        let mut random = move |n: usize| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize % n
        };
        let mut bits = Blocks::new();
        let mut set = BTreeSet::new();
        // Near a piece's edge and far up (title addresses reach 2^40 bytes).
        let base = [Blocks::IN_PIECE - 300, (1usize << 24) + 5, 0];
        for round in 0..4000 {
            let at = base[random(3)] + random(700);
            match random(4) {
                0 => assert_eq!(bits.insert(at), set.insert(at)),
                1 => assert_eq!(bits.remove(&at), set.remove(&at)),
                2 => {
                    let to = at + random(200);
                    bits.extend(at..to);
                    set.extend(at..to);
                }
                _ => assert_eq!(bits.contains(&at), set.contains(&at)),
            }
            let (from, to) = (at.saturating_sub(random(300)), at + random(300));
            assert_eq!(bits.range(from..to).collect::<Vec<_>>(), set.range(from..to).copied().collect::<Vec<_>>(), "round {round}");
            assert_eq!(
                bits.missing(from..to).collect::<Vec<_>>(),
                (from..to).filter(|b| !set.contains(b)).collect::<Vec<_>>(),
                "round {round}"
            );
            assert_eq!(bits.all(from, to), (from..to).all(|b| set.contains(&b)), "round {round}");
            assert_eq!(bits.is_empty(), set.is_empty());
        }
        assert!(bits.range(5..5).next().is_none() && bits.all(9, 9));
    }
}
