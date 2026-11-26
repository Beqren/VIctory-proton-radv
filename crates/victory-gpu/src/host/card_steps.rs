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
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_STEPS").is_some_and(|v| v == "1"))
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
    Read { asked: bool },
    /// Anything, anywhere.
    Run,
    /// Nothing the plan minds (refreshes, our own host work, shader writes).
    Other,
}

fn meet(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// The steps of `does` (by place) to be made on the card: every one that is
/// asked for, or why not. Our list makes them all in front of the
/// submission's lists, so that is right only if every asked step can be
/// made so and no step of the submission writes on the CPU what one of them
/// reads.
pub(super) fn plan(does: &[Does]) -> Result<Vec<usize>, &'static str> {
    let mut asked = Vec::new();
    for (i, d) in does.iter().enumerate() {
        match *d {
            Does::Run => return Err("a step of the title's runs"),
            Does::Read { asked: true } => return Err("a texture reads it"),
            Does::Copy { asked: true, .. } => return Err("a title buffer copy"),
            Does::Fill { asked: true, slot: None, .. } => return Err("a fill without a descriptor slot of its own"),
            Does::Fill { asked: true, .. } => asked.push(i),
            _ => {}
        }
    }
    for &i in &asked {
        let Does::Fill { src, len, .. } = does[i] else {
            continue;
        };
        for d in does {
            if let Does::Copy { dst, len: n, .. } = *d
                && meet((dst, dst + n), (src, src + len))
            {
                return Err("a title copy of the submission writes what it reads");
            }
        }
    }
    Ok(asked)
}

/// Title memory something in the air writes: the host copy it goes through
/// (`None`: none, the bytes reach title memory only when the flight lands),
/// from, to.
pub(super) struct Writer {
    shadow: Option<Arc<Shadow>>,
    from: usize,
    to: usize,
}

/// What the line's flights write.
fn writers(items: &[Late]) -> Vec<Writer> {
    let mut out = Vec::new();
    for item in items {
        match item {
            Late::Run(flight) => {
                for (shadow, ranges) in &flight.back {
                    out.extend(ranges.iter().map(|&(from, to)| Writer { shadow: Some(shadow.clone()), from, to }));
                }
                out.extend(flight.texture_back_ranges.iter().map(|&(from, to)| Writer { shadow: None, from, to }));
            }
            Late::Write(at, _, bytes) => out.push(Writer { shadow: None, from: *at, to: *at + *bytes }),
            Late::Signal(..) => {}
        }
    }
    out
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
static DONE: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// What [`Host::card_steps`] took off a submission: the list that does it,
/// for the submission to run first, and the steps the CPU leaves out (their
/// list by address, their place in it). Its flight holds it; when the
/// flight has landed (and is dropped) the list and buffer go back to the
/// pool.
pub(super) struct Taken {
    own: Option<Own>,
    steps: std::collections::HashSet<(usize, usize)>,
}

impl Taken {
    pub(super) fn list(&self) -> Option<&HostList> {
        self.own.as_ref().map(|o| &o.list)
    }

    /// Whether step `i` of `list` is made by this.
    pub(super) fn takes(&self, list: &HostList, i: usize) -> bool {
        self.steps.contains(&(std::ptr::from_ref(list) as usize, i))
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
    DONE[2].fetch_add(1, Ordering::Relaxed);
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
        let (written, writers) = {
            let line = line();
            // Flights being landed are out of the line: who writes what is
            // not known for them here.
            if line.settling {
                return Err("flights are being landed");
            }
            if line.written.is_empty() {
                return Ok(None);
            }
            (line.written.clone(), writers(&line.line))
        };
        // Sent again after a submission made some of it on the card: its
        // slots go back to its upload memory, on the CPU (`submit`).
        if runs.iter().any(|l| l.deferred().card_taken) {
            return Err("a list sent again after its constant buffers were made on the card");
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
                        Does::Read { asked: touches(&written, at, at + len) }
                    }
                    Step::Run(_) => Does::Run,
                    Step::Refresh(..) | Step::GpuWrite(..) | Step::Host(_) => Does::Other,
                });
                steps.push((std::ptr::from_ref::<HostList>(list) as usize, i));
            }
        }
        let chosen = plan(&does)?;
        if chosen.is_empty() {
            return Ok(None);
        }
        // (from, src, len, offset in ours, slot)
        let mut ops = Vec::with_capacity(chosen.len());
        let mut at = 0u64;
        for &i in &chosen {
            let Does::Fill { src, len, slot: Some((heap, index, _)), .. } = does[i] else {
                continue;
            };
            if len == 0 {
                return Err("nothing to read");
            }
            if at + len as u64 > BETWEEN {
                return Err("too much at once");
            }
            let from = holder(&writers, src, src + len)?;
            if !mapped(src, src + len) {
                return Err("not all of what it reads is mapped");
            }
            ops.push((from, src, len, at, (heap, index)));
            at = (at + len as u64).next_multiple_of(256);
        }
        let own = self.own_list().ok_or("no host list or buffer for it")?;
        for (from, src, len, ..) in &ops {
            from.refresh(&own.list, *src as u64, *len as u64);
        }
        let reads: Vec<(&ID3D12Resource, u64, u64, u64)> =
            ops.iter().map(|(from, src, len, at, _)| (&from.buffer, from.offset(*src as u64), *at, *len as u64)).collect();
        #[cfg(feature = "diagnostics")]
        own.list.note_op(|| format!("{} table constant buffers made on the card, {at} bytes (VICTORY_CARD_STEPS)", ops.len()));
        record(&own.list.list, &own.between, &reads);
        own.list.close();
        // SAFETY: a live buffer.
        let gpu = unsafe { own.between.GetGPUVirtualAddress() };
        for (_, _, len, at, (heap, index)) in &ops {
            DONE[0].fetch_add(1, Ordering::Relaxed);
            DONE[1].fetch_add(*len as u64, Ordering::Relaxed);
            // (A heap that is gone: nothing can read the slot.)
            crate::descriptor::constant_slot(*heap, *index, gpu + at, *len as u32);
        }
        // Their slots show our buffer from now on, also if they are sent
        // again (review finding): such a submission puts them back first.
        for list in runs {
            if chosen.iter().any(|&i| steps[i].0 == std::ptr::from_ref::<HostList>(list) as usize) {
                list.deferred().card_taken = true;
            }
        }
        Ok(Some(Taken { own: Some(own), steps: chosen.iter().map(|&i| steps[i]).collect() }))
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

/// Records into `list` (open): `reads` copied into `between` (source,
/// offset in it, offset in `between`, bytes), after every earlier write;
/// `between` is left readable as constants.
fn record(list: &ID3D12GraphicsCommandList, between: &ID3D12Resource, reads: &[(&ID3D12Resource, u64, u64, u64)]) {
    // SAFETY: an open list; live buffers, and the callers' ranges lie in
    // them. The buffers start in the common state (the list is the first of
    // its call) and the sources are promoted by their first use.
    unsafe {
        // What earlier lists' shaders wrote, first.
        list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }),
            },
        }]);
        for &(buffer, offset, at, len) in reads {
            list.CopyBufferRegion(between, at, buffer, offset, len);
        }
        // The sources back to the common state: the submission's lists run
        // in the same call and may write them (no decay in between; review
        // finding).
        let mut barriers = vec![transition(between, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER)];
        let mut sources: Vec<&ID3D12Resource> = Vec::new();
        for &(buffer, ..) in reads {
            if !sources.iter().any(|b| b.as_raw() == buffer.as_raw()) {
                sources.push(buffer);
            }
        }
        barriers.extend(sources.iter().map(|b| transition(b, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON)));
        list.ResourceBarrier(&barriers);
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
        "table constant buffers made on the card instead of waiting for flights, per frame (VICTORY_CARD_STEPS=1): {:.2}, \
         {:.1} KB; {:.2} submissions refused and waited ({why})",
        d[0],
        d[1] / 1024.0,
        d[2],
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
        let does = [copy(1000, 5000, 100, false), fill(2000, 64, true), Does::Other, fill(3000, 64, true), fill(1000, 64, false), Does::Read { asked: false }];
        assert_eq!(plan(&does), Ok(vec![1, 3]));
        assert_eq!(plan(&[fill(0, 64, true), fill(32, 64, true)]), Ok(vec![0, 1]));
        assert_eq!(plan(&[fill(0, 64, false)]), Ok(vec![]));
        // Ends that touch are apart.
        assert_eq!(plan(&[fill(1000, 100, true), copy(1100, 9000, 5, false), copy(900, 9000, 100, false)]), Ok(vec![0]));
    }

    /// Whatever the CPU would do in another order, or cannot leave out,
    /// refuses the lot, wherever in the submission it stands.
    #[test]
    fn what_stands_in_the_way_refuses() {
        let taken = fill(1000, 100, true);
        let refused = |other: Does| {
            assert!(plan(&[taken, other]).is_err(), "{other:?} after");
            assert!(plan(&[other, taken]).is_err(), "{other:?} before");
        };
        for asked in [false, true] {
            // A copy that writes what it reads.
            refused(copy(1099, 9000, 1, asked));
            refused(copy(900, 9000, 101, asked));
        }
        refused(Does::Run);
        refused(Does::Read { asked: true });
        refused(copy(5000, 6000, 10, true));
        refused(Does::Fill { src: 1, len: 1, asked: true, slot: None });
    }

    /// What no host copy holds, or two do, is refused.
    #[test]
    fn writers_without_one_host_copy_are_refused() {
        let writers = [Writer { shadow: None, from: 100, to: 108 }];
        assert!(holder(&writers, 104, 120).is_err());
    }
}
