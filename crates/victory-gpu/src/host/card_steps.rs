//! Constant buffers of descriptor tables whose bytes flights in the air may
//! still write, made on the card in the flights' order instead of on the
//! CPU after a wait for the flights (`VICTORY_CARD_STEPS=1`, see [`on`]).
//!
//! After the sibling fork VIctory-7900XTX cc03ecb (its stage 1), rebuilt on
//! this tree's relay: the fill's descriptor slot is pointed at a buffer of
//! ours that a list in front of the submission's lists fills from the host
//! copy the flights write.

use std::sync::Arc;

use super::*;
use crate::shadow::Shadow;

/// Whether table constant buffers that would wait for flights are made on
/// the card (`VICTORY_CARD_STEPS=1`; off by default until measured). Only
/// with the relay's exact waits (`relay_exact`).
///
/// Why: in the owner's session of 2026-11-25 (release, 218 world seconds)
/// the submitting thread, the title's own, stood 122 ms of a 242 ms frame in
/// 3.2 waits a frame for flights, all for "title memory read": 2.0 a frame a
/// fill of upload memory, 1.0 a title buffer copy, whose source lies in
/// memory a flight is declared to write. The card stood idle meanwhile. The
/// sibling fork found the same: a root view has no size, so a pass that
/// writes through one into the title's big heap is declared to write all
/// that is left of it, and the title's ring of constants lies there.
///
/// What is done: a list of ours goes to the card in front of the
/// submission's lists, in the same call, behind every flight. It copies the
/// bytes of each such fill from the host copy the flights write them through
/// into a buffer of ours, and the fill's descriptor slot (a dynamic slot of
/// the table made afresh for the draw: the fill's alone, and the list that
/// names it has not gone to the card) is made a constant buffer view of
/// that buffer. The CPU leaves the fill out.
///
/// Why that is the value the CPU would have read after the wait. After the
/// wait title memory holds, for every byte read, what the title wrote or
/// what the last flight that wrote it wrote (the landings put it there in
/// order). The host copy on the card holds the same when our list runs:
/// every flight that may write one of these bytes writes this same host copy
/// ([`holder`]: through another copy, or by a texture read-back or a value
/// written behind a flight, and the old wait is taken), they are all ahead
/// of our list on the queue, and the title's own bytes there are brought up
/// to date by a refresh step of our list, prepared like any other list's
/// (under flights' declared writes too: `stale_under_flights` puts the
/// title's changed bytes in beside the flights', or settles). Nothing of
/// this submission writes these bytes on the CPU ([`plan`] refuses a title
/// copy into them, or a step of the title's to run). Every draw that reads a
/// buffer view of the heap relies on the same refresh and the same order.
/// Where a page is not mapped the host copy may hold other bytes than the
/// CPU's copy would have left: refused.
///
/// `VICTORY_CARD_STEPS=2` (stage 2) does the title's buffer copies so too:
/// in run d6-time the costliest waits were for one 2 KiB title copy a frame
/// out of a 14 KiB range a flight's shaders write (mean 60 ms a wait), a true
/// dependency, not a loose declaration. Our list copies the bytes from the
/// source's host copy into our buffer and from there into the destination's
/// host copy, after a refresh of both, and declares a shader write of exactly
/// the bytes copied there: from then on it is what any shader's write is.
/// The relay compares, and puts what changed into title memory before the
/// title is told anything of this flight. Refused: another step of the
/// submission reads or writes the destination, or a refresh brings another
/// host copy of it up to date ([`plan`], [`elsewhere`]). While such a copy is
/// in the air, a submission that refreshes another host copy of its
/// destination waits for it ([`copied_read`], from `late_blocked`); with the
/// copy done on the CPU it was there at once. What this does not see: code
/// of ours that reads title memory away from a submission's steps (the pass
/// over textures picked by index): it finds the bytes when the flight has
/// landed, and the landing tells the guards, so a texture made from older
/// bytes is made again. This is why stage 2 is its own.
pub(super) fn stage() -> u32 {
    static STAGE: OnceLock<u32> = OnceLock::new();
    *STAGE.get_or_init(|| match std::env::var("VICTORY_CARD_STEPS").ok().as_deref() {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    })
}

pub(super) fn on() -> bool {
    stage() > 0
}

/// Our buffer's bytes: what one submission may make this way.
const BETWEEN: u64 = 1 << 20;

/// What a step of a submission does to title memory, as the plan sees it.
/// `asked`: flights in the air stop it from being done on the CPU now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Does {
    Copy { dst: usize, src: usize, len: usize, asked: bool },
    Fill { src: usize, len: usize, asked: bool, slot: Option<(usize, u32, u64)> },
    /// Another read on the CPU (a texture).
    Read { from: usize, to: usize, asked: bool },
    /// A refresh of a host copy (named by its address).
    Refresh { shadow: usize, from: usize, to: usize },
    /// Anything, anywhere.
    Run,
    /// Nothing the plan minds (refreshes, our own host work, shader writes).
    Other,
}

fn meet(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// The steps of `does` (by place) to be made on the card: every one that is
/// asked for, every later one that reads what a copy among them writes
/// ([`stage`] 2; after VIctory-7900XTX 007fe55, "steps done on the card take
/// along what depends on them"), or why not. Our list makes them one after
/// the other in the submission's order, in front of its lists, so that is
/// right only if every one of them can be made so, no step left to the CPU
/// writes what one of them reads or writes, and none left to the CPU reads
/// later what a copy among them writes.
///
/// In run d8-steps2 the one wait left a frame was refused for "a step of
/// the submission reads what it writes": a title copy into the big heap and
/// a later constant buffer of a table read out of what it wrote.
pub(super) fn plan(does: &[Does], stage: u32) -> Result<Vec<usize>, &'static str> {
    let mut taken = vec![false; does.len()];
    for (i, d) in does.iter().enumerate() {
        match *d {
            Does::Run => return Err("a step of the title's runs"),
            Does::Read { asked: true, .. } => return Err("a texture reads it"),
            Does::Copy { asked: true, .. } if stage < 2 => return Err("a title buffer copy (VICTORY_CARD_STEPS=2 does it on the card)"),
            Does::Fill { asked: true, slot: None, .. } => return Err("a fill without a descriptor slot of its own"),
            Does::Copy { asked: true, .. } | Does::Fill { asked: true, .. } => taken[i] = true,
            _ => {}
        }
    }
    let reads = |d: &Does| match *d {
        Does::Copy { src, len, .. } | Does::Fill { src, len, .. } => Some((src, src + len)),
        Does::Read { from, to, .. } => Some((from, to)),
        _ => None,
    };
    let writes = |d: &Does| match *d {
        Does::Copy { dst, len, .. } => Some((dst, dst + len)),
        _ => None,
    };
    // What depends on a copy made on the card comes along, in order.
    let mut grew = true;
    while grew {
        grew = false;
        for k in 0..does.len() {
            if taken[k] {
                continue;
            }
            let Some(r) = reads(&does[k]) else {
                continue;
            };
            let after_copy = (0..k).any(|i| taken[i] && writes(&does[i]).is_some_and(|w| meet(w, r)));
            if !after_copy {
                continue;
            }
            match does[k] {
                Does::Copy { .. } if stage >= 2 => taken[k] = true,
                Does::Fill { slot: Some(_), .. } => taken[k] = true,
                _ => return Err("a step of the submission reads what a copy made on the card writes"),
            }
            grew = true;
        }
    }
    let chosen: Vec<usize> = (0..does.len()).filter(|&i| taken[i]).collect();
    for &i in &chosen {
        let (src, dst) = (reads(&does[i]), writes(&does[i]));
        if let (Some(r), Some(w)) = (src, dst)
            && meet(r, w)
        {
            return Err("a copy into what it reads");
        }
        for (k, d) in does.iter().enumerate() {
            if taken[k] {
                continue;
            }
            // Left to the CPU, which does it now, before the card.
            let w = writes(d);
            if w.is_some_and(|w| src.is_some_and(|r| meet(w, r)) || dst.is_some_and(|x| meet(w, x))) {
                return Err("a copy left to the CPU writes what one made on the card reads or writes");
            }
            if k > i && dst.is_some_and(|x| reads(d).is_some_and(|r| meet(r, x))) {
                return Err("a step of the submission reads what it writes");
            }
        }
    }
    Ok(chosen)
}

/// Whether a refresh of `does` brings a host copy other than `shadow` up to
/// date in `[from, to)`.
fn elsewhere(does: &[Does], shadow: usize, from: usize, to: usize) -> bool {
    does.iter().any(|d| match *d {
        Does::Refresh { shadow: s, from: a, to: b } => s != shadow && meet((a, b), (from, to)),
        _ => false,
    })
}

/// Title memory something in the air writes: the host copy it goes through
/// (`None`: none, the bytes reach title memory only when the flight lands),
/// from, to.
pub(super) struct Writer {
    shadow: Option<Arc<Shadow>>,
    from: usize,
    to: usize,
    /// The destination of a copy made on the card ([`stage`] 2).
    copy: bool,
}

impl Writer {
    fn again(&self) -> Writer {
        Writer { shadow: self.shadow.clone(), from: self.from, to: self.to, copy: self.copy }
    }
}

/// What the line's flights write.
pub(super) fn writers(items: &[Late]) -> Vec<Writer> {
    let mut out = Vec::new();
    for item in items {
        match item {
            Late::Run(flight) => {
                for (shadow, ranges) in &flight.back {
                    out.extend(ranges.iter().map(|&(from, to)| Writer { shadow: Some(shadow.clone()), from, to, copy: false }));
                }
                out.extend(flight.texture_back_ranges.iter().map(|&(from, to)| Writer { shadow: None, from, to, copy: false }));
                out.extend(flight._own.iter().flat_map(Taken::copied));
            }
            Late::Write(at, _, bytes) => out.push(Writer { shadow: None, from: *at, to: *at + *bytes, copy: false }),
            Late::Signal(..) => {}
        }
    }
    out
}

/// The destinations of copies made on the card that have not landed.
pub(super) fn copies(line: &Line) -> Vec<Writer> {
    if stage() < 2 {
        return Vec::new();
    }
    let flying = line.line.iter().filter_map(|l| match l {
        Late::Run(flight) => flight._own.as_ref(),
        _ => None,
    });
    flying.flat_map(Taken::copied).chain(line.landing.iter().filter(|w| w.copy).map(Writer::again)).collect()
}

/// Whether `runs` refresh another host copy of a destination in `copies`:
/// that reads title memory, where the copy's bytes are not yet.
pub(super) fn copied_read(copies: &[Writer], runs: &[&HostList]) -> bool {
    if copies.is_empty() {
        return false;
    }
    runs.iter().any(|list| {
        list.deferred().copies.iter().any(|step| match step {
            Step::Refresh(shadow, at, len) => {
                let hit = copies
                    .iter()
                    .find(|w| w.from < *at + *len && *at < w.to && w.shadow.as_ref().is_none_or(|s| !Arc::ptr_eq(s, shadow)));
                #[cfg(feature = "diagnostics")]
                if let Some(w) = hit {
                    collided(w, shadow, *at, *len);
                }
                hit.is_some()
            }
            _ => false,
        })
    })
}

/// Diagnostics: a refresh of `shadow` (`[at, at + len)`) met the destination
/// of a copy made on the card through another host copy ([`copied_read`]):
/// the first few of each second said with both copies' spans, to judge
/// bringing the other copy up to date on the card.
#[cfg(feature = "diagnostics")]
fn collided(w: &Writer, shadow: &Arc<Shadow>, at: usize, len: usize) {
    static SAID: Mutex<(u64, u32)> = Mutex::new((0, 0));
    let now = now_ms() / 1000;
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.0 != now {
        *said = (now, 0);
    }
    if said.1 >= 4 {
        return;
    }
    said.1 += 1;
    let (other_start, other_len) = shadow.span();
    let (card_start, card_len) = w.shadow.as_ref().map_or((0, 0), |s| s.span());
    tracing::info!(
        copied = format_args!("{:#x}..{:#x}", w.from, w.to),
        through = format_args!("{card_start:#x}+{card_len:#x} (copy {})", w.shadow.as_ref().map_or(0, |s| s.id())),
        refreshed = format_args!("{at:#x}+{len:#x}"),
        other = format_args!("{other_start:#x}+{other_len:#x} (copy {}, on the card {})", shadow.id(), shadow.carded()),
        "a copy made on the card is read through another host copy: the submission waits"
    );
}

/// For the refreshes of `runs` that bring another host copy than the one a
/// copy made on the card writes through (`copies`) up to date over what it
/// writes: (the copy written through, the other copy, from, to) of each
/// overlap, or why it cannot be done on the card. Run d11-all named the case:
/// a 20 KiB title buffer whose copy was made on the card lies inside a 4 MiB
/// window of title memory a draw reads by address, and the next submission
/// refreshes the window: it waited for the copy to land.
fn mirrors(copies: &[Writer], runs: &[&HostList]) -> Result<Vec<(Arc<Shadow>, Arc<Shadow>, usize, usize)>, &'static str> {
    let mut out = Vec::new();
    for list in runs {
        for step in &list.deferred().copies {
            let Step::Refresh(other, at, len) = step else {
                continue;
            };
            for w in copies {
                let (from, to) = (w.from.max(*at), w.to.min(*at + *len));
                if from >= to {
                    continue;
                }
                let Some(through) = &w.shadow else {
                    return Err("a copy made on the card without a host copy");
                };
                if Arc::ptr_eq(through, other) {
                    continue;
                }
                if other.carded() || other.in_place() {
                    return Err("a copy made on the card is read through another host copy in the card's memory");
                }
                let (start, n) = other.span();
                if (from as u64) < start || to as u64 > start + n {
                    return Err("a copy made on the card is read through another host copy that holds part of it");
                }
                out.push((through.clone(), other.clone(), from, to));
            }
        }
    }
    Ok(out)
}

/// The host copy that holds `[from, to)` on the card once what `writers`
/// write is there: the one they all write it through.
fn holder(writers: &[Writer], from: usize, to: usize) -> Result<Arc<Shadow>, &'static str> {
    let mut found: Option<&Arc<Shadow>> = None;
    for w in writers.iter().filter(|w| meet((w.from, w.to), (from, to))) {
        match (&w.shadow, found) {
            (None, _) => return Err("a read-back or a value behind a flight is written there"),
            (Some(s), Some(f)) if !Arc::ptr_eq(s, f) => return Err("flights write it through two host copies"),
            (Some(s), _) => found = Some(s),
        }
    }
    let shadow = match found {
        Some(s) => s.clone(),
        None => crate::shadow::find(from as u64, (to - from) as u64).ok_or("no host copy of it")?,
    };
    let (start, len) = shadow.span();
    if shadow.in_place() || (from as u64) < start || to as u64 > start + len {
        return Err("no host copy holds all of it");
    }
    Ok(shadow)
}

/// Whether every page of `[from, to)` can be read.
fn mapped(from: usize, to: usize) -> bool {
    let mut bytes = vec![0u8; to - from];
    read_into(from as *const u8, &mut bytes)
}

/// A step made on the card.
struct Op {
    from: (Arc<Shadow>, usize),
    len: usize,
    /// Its place in our buffer.
    at: u64,
    to: To,
}

enum To {
    /// A host copy, and the title address.
    Copy(Arc<Shadow>, usize),
    /// Another host copy of the same title memory as a copy made on the card
    /// writes, brought up to date with it on the card ([`mirrors`]): nothing
    /// declared written (the copy's own host copy lands the bytes), nothing
    /// refreshed by our list (the submission's own refresh of it ran on the
    /// CPU before the card runs this).
    Mirror(Arc<Shadow>, usize),
    /// A descriptor slot (title heap, slot).
    Slot(usize, u32),
}

/// The list in front of a submission's, and its buffer ([`BETWEEN`] bytes).
struct Own {
    list: HostList,
    between: ID3D12Resource,
}

// SAFETY: D3D12 objects are free-threaded, and the list is touched by one
// thread at a time (the submitting one until it is sent, then nobody until
// its flight has landed and it is taken out of `POOL` again).
unsafe impl Send for Own {}

/// Those whose flight has landed.
static POOL: Mutex<Vec<Own>> = Mutex::new(Vec::new());

/// Since the last frame-rate line: fills made on the card, their bytes, and
/// submissions that waited instead, refused by a check.
static DONE: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// What [`Host::card_steps`] took off a submission: the list that does it,
/// for the submission to run first, and the steps the CPU leaves out (their
/// list by address, their place in it). Its flight holds it; when the
/// flight has landed (and is dropped) the list and buffer go back to the
/// pool.
pub(super) struct Taken {
    own: Option<Own>,
    /// The host copies our list reads or writes, held until it has run
    /// (review finding: a mirror's source is held by nothing else of ours,
    /// and the flight that wrote it may land and let it go first).
    _held: Vec<Arc<Shadow>>,
    steps: std::collections::HashSet<(usize, usize)>,
    /// The destinations of copies made on the card: host copy, from, to.
    copied: Vec<(Arc<Shadow>, usize, usize)>,
}

impl Taken {
    pub(super) fn list(&self) -> Option<&HostList> {
        self.own.as_ref().map(|o| &o.list)
    }

    /// Whether step `i` of `list` is made by this.
    pub(super) fn takes(&self, list: &HostList, i: usize) -> bool {
        self.steps.contains(&(std::ptr::from_ref(list) as usize, i))
    }

    fn copied(&self) -> impl Iterator<Item = Writer> + '_ {
        self.copied.iter().map(|(shadow, from, to)| Writer { shadow: Some(shadow.clone()), from: *from, to: *to, copy: true })
    }
}

impl Drop for Taken {
    fn drop(&mut self) {
        if let Some(own) = self.own.take() {
            // The host copies its steps held go; its commands stay until the
            // next use resets it.
            drop(std::mem::take(&mut *own.list.deferred()));
            POOL.lock().unwrap_or_else(|e| e.into_inner()).push(own);
        }
    }
}

/// Why the last refusal was, by reason, since the last line (diagnostics).
#[cfg(feature = "diagnostics")]
static REFUSED: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());

fn refused(why: &'static str) {
    DONE[3].fetch_add(1, Ordering::Relaxed);
    #[cfg(feature = "diagnostics")]
    {
        let mut r = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
        match r.iter_mut().find(|(w, _)| *w == why) {
            Some(e) => e.1 += 1,
            None => r.push((why, 1)),
        }
    }
    let _ = why;
}

impl Host {
    /// With flights in the air and `runs` blocked by them for a read: if
    /// what blocks is table constant buffers that can all be made on the
    /// card ([`on`]), the list that makes them. `None`: the old wait is due.
    /// Under the submission lock.
    pub(super) fn card_steps(&self, runs: &[&HostList]) -> Option<Taken> {
        if !on() || !relay_exact() {
            return None;
        }
        match self.take_steps(runs) {
            Ok(taken) => taken,
            Err(why) => {
                refused(why);
                None
            }
        }
    }

    fn take_steps(&self, runs: &[&HostList]) -> Result<Option<Taken>, &'static str> {
        let (written, writers, flying_mirrors) = {
            let line = line();
            if line.written.is_empty() {
                return Ok(None);
            }
            // The flights in the air, and those the relay is landing (out
            // of the line, still in `written` until they have landed).
            let mut all = writers(&line.line);
            all.extend(line.landing.iter().map(Writer::again));
            // Copies made on the card in the air whose destination this
            // submission refreshes through another host copy: that copy is
            // brought up to date on the card too, instead of waiting.
            let flying = copies(&line);
            (line.written.clone(), all, if copied_read(&flying, runs) { mirrors(&flying, runs)? } else { Vec::new() })
        };
        // Sent again after a submission made some of it on the card: its
        // slots go back to its upload memory, on the CPU (`submit`).
        if runs.iter().any(|l| l.deferred().card_taken) {
            return Err("a list sent again after its constant buffers were made on the card");
        }
        // A list sent before may still be read by that submission: its
        // dynamic slots are not rewritten under it (the class of a slot
        // written under a running submission, whatever the bytes).
        if runs.iter().any(|l| l.deferred().sent) {
            return Err("a list sent before since its reset");
        }
        let mut does = Vec::new();
        let mut steps = Vec::new();
        for list in runs {
            for (i, step) in list.deferred().copies.iter().enumerate() {
                does.push(match step {
                    &Step::Copy(dst, src, len) => Does::Copy {
                        dst,
                        src,
                        len,
                        asked: touches(&written, src, src + len) || touches(&written, dst, dst + len),
                    },
                    &Step::Fill(_, src, len, slot) => Does::Fill { src, len, asked: touches(&written, src, src + len), slot },
                    Step::Texture(staging) => {
                        let (at, len) = staging.memory();
                        Does::Read { from: at, to: at + len, asked: touches(&written, at, at + len) }
                    }
                    Step::Run(_) => Does::Run,
                    Step::Refresh(shadow, at, len) => Does::Refresh { shadow: Arc::as_ptr(shadow) as usize, from: *at, to: *at + *len },
                    Step::GpuWrite(..) | Step::Host(_) => Does::Other,
                });
                steps.push((std::ptr::from_ref::<HostList>(list) as usize, i));
            }
        }
        let chosen = plan(&does, stage())?;
        if chosen.is_empty() && flying_mirrors.is_empty() {
            return Ok(None);
        }
        let mut ops: Vec<Op> = Vec::with_capacity(chosen.len() + flying_mirrors.len());
        let mut at = 0u64;
        // First what copies in the air wrote, into the other host copies this
        // submission reads it through.
        for (through, other, from, to) in flying_mirrors {
            let len = to - from;
            if at + len as u64 > BETWEEN {
                return Err("too much at once");
            }
            ops.push(Op { from: (through, from), len, at, to: To::Mirror(other, from) });
            at = (at + len as u64).next_multiple_of(256);
        }
        for &i in &chosen {
            let (src, len) = match does[i] {
                Does::Copy { src, len, .. } | Does::Fill { src, len, .. } => (src, len),
                _ => continue,
            };
            if len == 0 {
                return Err("nothing to read");
            }
            if at + len as u64 > BETWEEN {
                return Err("too much at once");
            }
            // Earlier copies made here write through their destination's
            // host copy too.
            let mut writers_now: Vec<Writer> = writers.iter().map(Writer::again).collect();
            writers_now.extend(ops.iter().filter_map(|op: &Op| match &op.to {
                To::Copy(to, dst) => Some(Writer { shadow: Some(to.clone()), from: *dst, to: *dst + op.len, copy: true }),
                To::Slot(..) | To::Mirror(..) => None,
            }));
            let from = holder(&writers_now, src, src + len)?;
            if !mapped(src, src + len) {
                return Err("not all of what it reads is mapped");
            }
            let mut also = Vec::new();
            let to = match does[i] {
                Does::Copy { dst, .. } => {
                    let to = holder(&writers_now, dst, dst + len)?;
                    if !mapped(dst, dst + len) {
                        return Err("not all of what it writes is mapped");
                    }
                    // Another host copy of the destination this submission
                    // refreshes: brought up to date on the card after it.
                    if elsewhere(&does, Arc::as_ptr(&to) as usize, dst, dst + len) {
                        let written = [Writer { shadow: Some(to.clone()), from: dst, to: dst + len, copy: true }];
                        also = mirrors(&written, runs)?;
                    }
                    To::Copy(to, dst)
                }
                Does::Fill { slot: Some((heap, index, _)), .. } => To::Slot(heap, index),
                _ => continue,
            };
            ops.push(Op { from: (from, src), len, at, to });
            at = (at + len as u64).next_multiple_of(256);
            for (through, other, from, to) in also {
                let n = to - from;
                if at + n as u64 > BETWEEN {
                    return Err("too much at once");
                }
                ops.push(Op { from: (through, from), len: n, at, to: To::Mirror(other, from) });
                at = (at + n as u64).next_multiple_of(256);
            }
        }
        let own = self.own_list().ok_or("no host list or buffer for it")?;
        let mut seq = Vec::new();
        let mut copied = Vec::new();
        // Every refresh first, then the writes declared: the bytes compared
        // with afterwards are the title's.
        for op in &ops {
            // (A mirror reads what a copy made on the card wrote: no
            // refresh of it, which would leave that range alone anyway.)
            if !matches!(op.to, To::Mirror(..)) {
                op.from.0.refresh(&own.list, op.from.1 as u64, op.len as u64);
            }
            let out = match &op.to {
                To::Copy(to, dst) => {
                    to.refresh(&own.list, *dst as u64, op.len as u64);
                    Some((&to.buffer, to.offset(*dst as u64)))
                }
                To::Mirror(to, at) => Some((&to.buffer, to.offset(*at as u64))),
                To::Slot(..) => None,
            };
            seq.push((&op.from.0.buffer, op.from.0.offset(op.from.1 as u64), op.at, op.len as u64, out));
        }
        for op in &ops {
            match &op.to {
                To::Copy(to, dst) => {
                    to.gpu_writes(&own.list, *dst as u64, op.len as u64, false);
                    copied.push((to.clone(), *dst, *dst + op.len));
                }
                // Declared written too (review finding): a later refresh of
                // the other copy leaves the range alone until this has run
                // and landed, so the card does not put older bytes over a
                // newer refresh; what it compares there afterwards is the
                // same bytes the copy itself lands.
                To::Mirror(to, at) => to.gpu_writes(&own.list, *at as u64, op.len as u64, false),
                To::Slot(..) => {}
            }
        }
        let held: Vec<Arc<Shadow>> = ops
            .iter()
            .flat_map(|op| {
                let to = match &op.to {
                    To::Copy(to, _) | To::Mirror(to, _) => Some(to.clone()),
                    To::Slot(..) => None,
                };
                std::iter::once(op.from.0.clone()).chain(to)
            })
            .collect();
        #[cfg(feature = "diagnostics")]
        own.list.note_op(|| format!("{} steps of title memory made on the card, {at} bytes (VICTORY_CARD_STEPS)", ops.len()));
        record(&own.list.list, &own.between, &seq);
        own.list.close();
        // SAFETY: a live buffer.
        let gpu = unsafe { own.between.GetGPUVirtualAddress() };
        for op in &ops {
            DONE[2].fetch_add(op.len as u64, Ordering::Relaxed);
            match &op.to {
                To::Slot(heap, index) => {
                    DONE[0].fetch_add(1, Ordering::Relaxed);
                    // (A heap that is gone: nothing can read the slot.)
                    crate::descriptor::constant_slot(*heap, *index, gpu + op.at, op.len as u32);
                }
                To::Copy(..) => {
                    DONE[1].fetch_add(1, Ordering::Relaxed);
                }
                To::Mirror(..) => {
                    DONE[4].fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Ok(Some(Taken { own: Some(own), _held: held, steps: chosen.iter().map(|&i| steps[i]).collect(), copied }))
    }

    /// A list of ours open for recording, and its buffer.
    fn own_list(&self) -> Option<Own> {
        let pooled = POOL.lock().unwrap_or_else(|e| e.into_inner()).pop();
        if let Some(own) = pooled {
            own.list.reset();
            return Some(own);
        }
        let between = self.uav_buffer(BETWEEN)?;
        name(&between, || "table constant buffers made on the card".into());
        let list = self.new_list().ok()?;
        Some(Own { list, between })
    }
}

/// One step on the card: its source (buffer, offset), its place in our
/// buffer and bytes, and for a copy its destination (buffer, offset).
type Seq<'a> = (&'a ID3D12Resource, u64, u64, u64, Option<(&'a ID3D12Resource, u64)>);

/// Records into `list` (open) the steps `seq` one after the other, in the
/// submission's order: each copies its source into its place in `between`
/// after every earlier write (the flights' shaders', and the step before's),
/// and a copy then copies it on into its destination. Every buffer is left
/// in the common state, `between` readable as constants, as the submission's
/// lists (in the same call: no decay in between) expect them.
fn record(list: &ID3D12GraphicsCommandList, between: &ID3D12Resource, seq: &[Seq]) {
    let common = D3D12_RESOURCE_STATE_COMMON;
    let (source, dest) = (D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
    let all_uav = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }),
        },
    };
    // SAFETY: an open list; live buffers, and the callers' ranges lie in
    // them. A buffer starts each step in the common state (first use in the
    // call, or put back there by the step before) and is promoted by its
    // first use; `between` starts as a copy's destination (promoted from
    // common by the first step) and is one again after each step.
    unsafe {
        for (n, &(from, offset, at, len, out)) in seq.iter().enumerate() {
            list.ResourceBarrier(std::slice::from_ref(&all_uav));
            list.CopyBufferRegion(between, at, from, offset, len);
            let mut after = vec![transition(between, dest, source)];
            match out {
                Some((to, _)) if to.as_raw() == from.as_raw() => after.push(transition(from, source, dest)),
                Some(_) | None => after.push(transition(from, source, common)),
            }
            list.ResourceBarrier(&after);
            let mut done = Vec::new();
            if let Some((to, to_offset)) = out {
                list.CopyBufferRegion(to, to_offset, between, at, len);
                done.push(transition(to, dest, common));
            }
            done.push(transition(
                between,
                source,
                if n + 1 == seq.len() { D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER } else { dest },
            ));
            list.ResourceBarrier(&done);
        }
    }
}

/// For the frame-rate line.
pub(super) fn log(frames: f64) {
    if !on() {
        return;
    }
    let d: Vec<f64> = DONE.iter().map(|d| d.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    #[cfg(feature = "diagnostics")]
    let why: String = std::mem::take(&mut *REFUSED.lock().unwrap_or_else(|e| e.into_inner()))
        .into_iter()
        .map(|(w, n)| format!("{w}: {:.2}", n as f64 / frames))
        .collect::<Vec<_>>()
        .join(", ");
    #[cfg(not(feature = "diagnostics"))]
    let why = String::new();
    tracing::info!(
        "steps made on the card instead of waiting for flights, per frame (VICTORY_CARD_STEPS={}): {:.2} table constant \
         buffers, {:.2} title buffer copies, {:.2} other host copies of their destinations brought up to date, {:.1} KB; \
         {:.2} submissions refused and waited ({why})",
        stage(),
        d[0],
        d[1],
        d[4],
        d[2] / 1024.0,
        d[3],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy(dst: usize, src: usize, len: usize, asked: bool) -> Does {
        Does::Copy { dst, src, len, asked }
    }

    fn fill(src: usize, len: usize, asked: bool) -> Does {
        Does::Fill { src, len, asked, slot: Some((1, 2, 3)) }
    }

    /// Only what is asked for is taken, all of it; steps in none of their
    /// ways change nothing.
    #[test]
    fn fills_apart_from_the_rest_are_taken() {
        let does = [
            copy(1000, 5000, 100, false),
            fill(2000, 64, true),
            Does::Refresh { shadow: 1, from: 0, to: 9000 },
            copy(3000, 6000, 100, true),
            fill(1000, 64, false),
            Does::Read { from: 3100, to: 4000, asked: false },
        ];
        assert_eq!(plan(&does, 2), Ok(vec![1, 3]));
        assert!(plan(&does, 1).is_err(), "a copy without stage 2");
        assert_eq!(plan(&[fill(0, 64, true), fill(32, 64, true)], 1), Ok(vec![0, 1]));
        assert_eq!(plan(&[fill(0, 64, false)], 1), Ok(vec![]));
        // Ends that touch are apart.
        assert_eq!(plan(&[fill(1000, 100, true), copy(1100, 9000, 5, false), copy(900, 9000, 100, false)], 1), Ok(vec![0]));
    }

    /// Whatever the CPU would do in another order, or cannot leave out,
    /// refuses the lot, wherever in the submission it stands.
    #[test]
    fn what_stands_in_the_way_refuses() {
        let taken = fill(1000, 100, true);
        let refused = |other: Does| {
            assert!(plan(&[taken, other], 1).is_err(), "{other:?} after");
            assert!(plan(&[other, taken], 1).is_err(), "{other:?} before");
        };
        for asked in [false, true] {
            // A copy that writes what it reads.
            refused(copy(1099, 9000, 1, asked));
            refused(copy(900, 9000, 101, asked));
        }
        refused(Does::Run);
        refused(Does::Read { from: 1, to: 2, asked: true });
        // Stage 2: what reads a copy made on the card later comes along.
        let chain = [copy(1000, 5000, 100, true), fill(1050, 16, false), copy(7000, 1020, 10, false)];
        assert_eq!(plan(&chain, 2), Ok(vec![0, 1, 2]));
        // Read before the copy: left to the CPU, which reads it first.
        assert_eq!(plan(&[fill(1050, 16, false), copy(1000, 5000, 100, true)], 2), Ok(vec![1]));
        // A later read the card cannot make.
        assert!(plan(&[copy(1000, 5000, 100, true), Does::Read { from: 1000, to: 1001, asked: false }], 2).is_err());
        assert!(plan(&[copy(1000, 5000, 100, true), Does::Fill { src: 1000, len: 4, asked: false, slot: None }], 2).is_err());
        refused(copy(5000, 6000, 10, true));
        refused(Does::Fill { src: 1, len: 1, asked: true, slot: None });
        // Stage 2: what a copy made on the card writes, nothing else of the
        // submission may write or read.
        let copied = copy(1000, 5000, 100, true);
        let refused2 = |other: Does| {
            assert!(plan(&[copied, other], 2).is_err(), "{other:?} after");
            assert!(plan(&[other, copied], 2).is_err(), "{other:?} before");
        };
        // Copies left to the CPU that write what it reads or writes.
        refused2(copy(5099, 9000, 1, false));
        refused2(copy(999, 9000, 2, false));
        // A later texture read of what it writes.
        assert!(plan(&[copied, Does::Read { from: 900, to: 1001, asked: false }], 2).is_err());
        assert!(plan(&[copy(1000, 1050, 100, true)], 2).is_err(), "into itself");
        // Two copies made on the card, in order, are both taken.
        assert_eq!(plan(&[copied, copy(1000, 6000, 100, true)], 2), Ok(vec![0, 1]));
    }

    /// What no host copy holds, or two do, is refused.
    #[test]
    fn writers_without_one_host_copy_are_refused() {
        let writers = [Writer { shadow: None, from: 100, to: 108, copy: false }];
        assert!(holder(&writers, 104, 120).is_err());
    }

    #[test]
    fn a_refresh_of_another_host_copy_is_seen() {
        let does = [Does::Refresh { shadow: 7, from: 100, to: 200 }];
        assert!(!elsewhere(&does, 7, 100, 200));
        assert!(elsewhere(&does, 8, 199, 300));
        assert!(!elsewhere(&does, 8, 200, 300));
    }
}
