//! Textures shaders write that the title's own code then reads on the CPU.
//! On the console the CPU and the GPU share memory, so what a compute pass
//! stores into a texture is in title memory for the CPU to read. Here it stays
//! in the texture's host copy. The title's ocean is one: its wave passes write
//! a 256x256 height map (R32_FLOAT) and a displacement map (R32G32_FLOAT), and
//! its CPU code samples both, which is what boats and swimmers float on; it
//! read memory nothing had written (fork fxceless/victory-fork, run rw1 with
//! `VICTORY_READ_WATCH=1`: about 30 places in the title read those kinds),
//! boats sank, and the owner's first mission after the prologue failed at
//! once with everything falling to the bottom of the water (2026-11-25).
//!
//! Ported from fxceless/victory-fork pull request 999 (merge f5c03f3: commits
//! 895c0b8, Geckomacabre, and 04df435, the per-allocation tickets), the same
//! code as avatarmanz/VIctory-7900XTX 23925a4, re-done against this tree:
//! the texels go into title memory through the copy-back's guarded write
//! (`crate::shadow::put_back`: guards opened, raced writes retried), not a
//! plain kernel copy; they land in [`crate::host`]'s `read_backs`, which runs
//! only after the device was found alive; bytes that did not change are not
//! written (and so tell nothing to the guards or to other host copies).
//!
//! For textures of the kinds in `VICTORY_TEXTURE_BACK` (comma-separated
//! `<width>x<height>f<format>`; unset or `1`: the four kinds below; `0`:
//! none), a list in which shaders write one through unordered access copies
//! it into a read-back buffer of ours when it closes, and once the list has
//! run the texels are tiled into the texture's title memory in the console's
//! layout, as the console's GPU would have left them. Until that flight has
//! landed the texture's memory counts as written by it (the line's `written`
//! ranges), so nothing of ours reads it in between.
//!
//! The loop back: once the texels are in title memory, the guards see that
//! memory written, as for any write of the copy-back. The texture's own host
//! copy is not uploaded from it again: a title texture shaders write keeps
//! its unordered access on the host, and only textures without render
//! target, depth or unordered access are filled from title memory
//! (`crate::texture::title_written`). So what the next frame's shaders find
//! is what the last frame's left, not a copy a frame old. (fxceless's
//! version relies on the same rule; it writes with `copy_mapped`.)
//!
//! shortcut: by kind, not by texture: textures of those kinds the title never
//! reads are copied back too (about a megabyte per such list). The copy shows
//! what the list's last write left; a title read in between sees the frame
//! before, as it would before the console's GPU has run. Only writes through
//! a view `crate::draw` sees as unordered access are noticed.
//!
//! shortcut: every remap of title memory takes the tickets' lock (a list of
//! the few textures copied back), and a landing holds it across its guarded
//! write, so a remap may wait for one landing (a stall, not a deadlock:
//! nothing in the landing waits for a lock a remap holds).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use windows::Win32::Graphics::Direct3D12::*;

use crate::object::This;

/// The kinds copied back by default: (width, height, format), those the
/// fork's read watch found the title reading.
const DEFAULT: &str = "256x256f41,256x256f16,64x64f16,128x128f36";

/// The kinds copied back: (width, height, format).
fn kinds() -> &'static [(u64, u32, u32)] {
    static KINDS: OnceLock<Vec<(u64, u32, u32)>> = OnceLock::new();
    KINDS.get_or_init(|| {
        let list = match std::env::var("VICTORY_TEXTURE_BACK").as_deref() {
            Ok("1") | Err(_) => DEFAULT.to_owned(),
            Ok("0") => String::new(),
            Ok(spec) => spec.to_owned(),
        };
        let kinds = parse(&list);
        if kinds.is_empty() {
            tracing::info!("no textures shaders write are copied back into title memory (VICTORY_TEXTURE_BACK=0)");
        } else {
            tracing::info!(
                ?kinds,
                "textures of these kinds that shaders write are copied back into title memory (VICTORY_TEXTURE_BACK; =0 turns it off)"
            );
        }
        kinds
    })
}

fn parse(list: &str) -> Vec<(u64, u32, u32)> {
    list.split(',')
        .filter_map(|k| {
            let (size, format) = k.trim().split_once('f')?;
            let (w, h) = size.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?, format.parse().ok()?))
        })
        .collect()
}

/// Whether title texture `resource` is of a kind copied back.
pub(crate) fn wanted(resource: This) -> bool {
    let kinds = kinds();
    !kinds.is_empty() && crate::resource::texture_kind(resource).is_some_and(|k| kinds.contains(&k))
}

/// An outstanding write to a specific allocation. Mapping and resource
/// changes invalidate only the writes that could touch the replaced memory
/// (fork 04df435).
pub(crate) struct Ticket {
    owner: usize,
    at: usize,
    end: usize,
    valid: AtomicBool,
}

static TICKETS: Mutex<Vec<Weak<Ticket>>> = Mutex::new(Vec::new());
static TRACKING: AtomicBool = AtomicBool::new(false);

fn track(owner: usize, at: usize, end: usize) -> Arc<Ticket> {
    let ticket = Arc::new(Ticket { owner, at, end, valid: AtomicBool::new(true) });
    let mut tickets = TICKETS.lock().unwrap_or_else(|e| e.into_inner());
    tickets.retain(|weak| weak.strong_count() != 0);
    tickets.push(Arc::downgrade(&ticket));
    TRACKING.store(true, Ordering::Relaxed);
    ticket
}

fn invalidate(matches: impl Fn(&Ticket) -> bool) {
    if !TRACKING.load(Ordering::Relaxed) {
        return;
    }
    TICKETS.lock().unwrap_or_else(|e| e.into_inner()).retain(|weak| {
        let Some(ticket) = weak.upgrade() else { return false };
        if matches(&ticket) {
            ticket.valid.store(false, Ordering::Release);
        }
        true
    });
}

/// The title released resource `owner` (its address).
pub(crate) fn released(owner: usize) {
    invalidate(|ticket| ticket.owner == owner);
}

/// A texture was made at title memory `at`.
pub(crate) fn replaced(at: usize) {
    invalidate(|ticket| ticket.at == at);
}

/// The runtime mapped or unmapped title memory `[at, at + len)`.
pub(crate) fn remapped(at: usize, len: usize) {
    let end = at.saturating_add(len);
    invalidate(|ticket| at < ticket.end && ticket.at < end);
}

/// A title texture a list's shaders write, to be copied back.
pub(crate) struct Target {
    pub(crate) copy: Arc<crate::resource::HostTexture>,
    layout: victory_formats::texture::Layout,
    /// The title's format.
    format: u32,
    ticket: Arc<Ticket>,
}

impl Target {
    pub(crate) fn new(resource: This) -> Option<Self> {
        // The host copy, held apart from the title resource's lifetime.
        let copy = crate::resource::host_copy(resource)?;
        let (layout, at, size) = crate::resource::title_texture(resource)?;
        let (_, _, format) = crate::resource::texture_kind(resource)?;
        first_seen(resource, at, size, &layout);
        // A layout the formats crate cannot tile (the 64 KiB XOR modes: one
        // 256x256 R32_FLOAT texture in SW_64KB_R_X in run water-2, beside
        // 45 it can) is not copied at all: logged once above.
        if !layout.addressable() {
            return None;
        }
        let end = at.checked_add(size).filter(|&end| at != 0 && end > at)?;
        let ticket = track(resource as usize, at, end);
        Some(Self { copy, layout, format, ticket })
    }

    pub(crate) fn owner(&self) -> usize {
        self.ticket.owner
    }
}

/// Logs each texture the read-back takes, once (by its title memory and
/// kind), in every build: the owner's play sessions are the test.
fn first_seen(resource: This, at: usize, size: usize, layout: &victory_formats::texture::Layout) {
    static SEEN: Mutex<Option<std::collections::HashSet<(usize, u64, u32, u32)>>> = Mutex::new(None);
    let Some((width, height, format)) = crate::resource::texture_kind(resource) else {
        return;
    };
    let new = SEEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .insert((at, width, height, format));
    if new {
        TEXTURES.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            width,
            height,
            format = format_args!("{format} ({})", format_name(format)),
            memory = format_args!("{at:#x}"),
            bytes = size,
            mips = layout.desc.mips,
            swizzle = layout.desc.swizzle.0,
            tileable = layout.addressable(),
            "a texture shaders write is copied back into title memory (VICTORY_TEXTURE_BACK)"
        );
    }
}

fn format_name(format: u32) -> &'static str {
    match format {
        16 => "R32G32_FLOAT",
        36 => "R16G16_UINT",
        41 => "R32_FLOAT",
        _ => "?",
    }
}

/// The read-back buffer and the allocation the texels belong to.
pub(crate) struct Back {
    buffer: ID3D12Resource,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    mips: u32,
    layout: victory_formats::texture::Layout,
    ticket: Arc<Ticket>,
    /// With `crate::host::fetch_mark`: where in `buffer` the list writes
    /// its mark behind the copies, and the mark.
    mark: Option<(u64, u32)>,
}

impl Back {
    /// The title memory it writes: address, end.
    pub(crate) fn range(&self) -> (usize, usize) {
        (self.ticket.at, self.ticket.end)
    }
}

/// Since the start: textures taken (distinct), copies recorded, time
/// recording them (us), copies landed, bytes written into title memory,
/// copies landed that changed nothing (not written), copies whose
/// allocation went meanwhile, time landing (us).
static TEXTURES: AtomicU64 = AtomicU64::new(0);
static STATS: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
const RECORDED: usize = 0;
const RECORD_US: usize = 1;
const LANDED: usize = 2;
const BYTES: usize = 3;
const UNCHANGED: usize = 4;
const STALE: usize = 5;
const LAND_US: usize = 6;
/// Copies landed that could not be: title memory not all readable (partly
/// mapped), or the read-back not mapped, or the texels not tiled.
const FAILED: usize = 7;
/// Textures not recorded: the host copy's format is not the title's (a
/// stand-in of another texel size, `crate::resource::target_format`), or
/// it has more than one subresource.
const OTHER_FORMAT: usize = 8;
/// Copies whose mark was not behind them: not written.
const UNMARKED: usize = 9;

/// Records the copy of a list's target (already in COPY_SOURCE) into a
/// read-back buffer.
pub(crate) fn record(list: &ID3D12GraphicsCommandList, target: Target) -> Option<Back> {
    let host = crate::host::host()?;
    record_texture(&host.device, list, &target.copy.texture, target.layout, target.format, target.ticket)
}

/// [`record`] for host texture `texture` (in COPY_SOURCE) of console layout
/// `layout`, going to the memory of `ticket`.
fn record_texture(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    layout: victory_formats::texture::Layout,
    format: u32,
    ticket: Arc<Ticket>,
) -> Option<Back> {
    if !ticket.valid.load(Ordering::Acquire) {
        return None;
    }
    let started = std::time::Instant::now();
    // SAFETY: a live texture the list holds.
    let desc = unsafe { texture.GetDesc() };
    if desc.Format.0 as u32 != format {
        STATS[OTHER_FORMAT].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mips = u32::from(desc.MipLevels.max(1));
    let subresources = mips * u32::from(desc.DepthOrArraySize.max(1));
    // One subresource only: an unordered view writes one mip (and maybe one
    // slice), and the others' card contents (never uploaded from title
    // memory: a texture shaders write is the host's own) would go over the
    // title's texels (review finding). Every texture taken in the owner's
    // session of 2026-11-25 had one mip and one slice.
    if subresources != 1 {
        STATS[OTHER_FORMAT].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let n = subresources as usize;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n];
    let mut rows = vec![0u32; n];
    let mut total = 0u64;
    // SAFETY: out arrays of `n` entries.
    unsafe {
        device.GetCopyableFootprints(
            &desc,
            0,
            subresources,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        )
    };
    // The last footprint's advertised extent can omit row-end padding; the
    // tiler reads a pitched slice, so the mapped buffer must include it.
    for (footprint, &rows) in footprints.iter().zip(&rows) {
        let format = footprint.Footprint;
        let pitched = u64::from(format.RowPitch)
            .checked_mul(u64::from(rows))?
            .checked_mul(u64::from(format.Depth))?;
        total = total.max(footprint.Offset.checked_add(pitched)?);
    }
    // The mark behind the copies, as the relay's fetch has one
    // (`crate::host::fetch_mark`): the card may finish a list's fence on a
    // lost device before the loss is reported, and then the buffer holds
    // nothing it copied (zeros), which would be tiled into title memory.
    let mut mark = crate::host::fetch_mark().then(|| {
        static SENT: AtomicU64 = AtomicU64::new(0);
        (total.next_multiple_of(4), crate::host::mark_value(SENT.fetch_add(1, Ordering::Relaxed) + 1))
    });
    let buffer = read_back_buffer(device, total + if mark.is_some() { 8 } else { 0 })?;
    for (i, footprint) in footprints.iter().enumerate() {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *footprint },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
        };
        // SAFETY: an open list; the texture is in the copy-source state.
        unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    if let Some((at, value)) = mark {
        use windows::core::Interface;
        // SAFETY: our buffer, in the copy destination state; the mark lies
        // past every footprint.
        unsafe {
            let write = [D3D12_WRITEBUFFERIMMEDIATE_PARAMETER { Dest: buffer.GetGPUVirtualAddress() + at, Value: value }];
            // Without the interface the copies are recorded all the same,
            // and the buffer must outlive the list: kept, without a mark
            // (review finding: returning here dropped a buffer the list
            // names).
            match list.cast::<ID3D12GraphicsCommandList2>() {
                Ok(list) => {
                    list.WriteBufferImmediate(1, write.as_ptr(), Some([D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT].as_ptr()))
                }
                Err(_) => mark = None,
            }
        }
    }
    STATS[RECORDED].fetch_add(1, Ordering::Relaxed);
    STATS[RECORD_US].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    report();
    Some(Back { buffer, footprints, rows, mips, layout, ticket, mark })
}

/// A read-back buffer of ours (not shader-visible), in the copy
/// destination state.
fn read_back_buffer(device: &ID3D12Device, size: u64) -> Option<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(1),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation of a read-back buffer.
    let made = unsafe {
        device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut buffer)
    };
    if let Err(e) = made {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!(error = %e, size, "no read-back buffer for a texture copied back"));
        return None;
    }
    buffer
}

/// Once the list has run (and the device was found alive: the caller,
/// `crate::host`'s `read_backs`): tiles the texels into the same title
/// allocation, through the copy-back's guarded write. Bytes as they are
/// already are not written.
pub(crate) fn land(back: &Back) {
    let started = std::time::Instant::now();
    land_now(back);
    STATS[LAND_US].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    report();
}

fn land_now(back: &Back) {
    // Held across the write: a release or remap of the allocation waits for
    // it, and one before it makes the ticket stale. Nothing here waits for a
    // lock a remap holds while it takes this one (`crate::watch`'s
    // `title_memory_remapped` calls [`remapped`] before it takes the guards;
    // the write's own reading of the mapping lock is only tried).
    let _tickets = TICKETS.lock().unwrap_or_else(|e| e.into_inner());
    if !back.ticket.valid.load(Ordering::Acquire) {
        let skipped = STATS[STALE].fetch_add(1, Ordering::Relaxed) + 1;
        if skipped.is_power_of_two() {
            tracing::info!(skipped, "texture read-backs skipped: their title allocation changed");
        }
        return;
    }
    let (at, end) = back.range();
    let size = end - at;
    let mut read = vec![0u8; size];
    // The title's bytes the subresources do not cover are kept.
    if !crate::host::read_into(at as *const u8, &mut read) {
        STATS[FAILED].fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut tiled = read.clone();
    let mut data = std::ptr::null_mut();
    // SAFETY: our read-back buffer, written by the list that has run.
    unsafe {
        if back.buffer.Map(0, None, Some(&mut data)).is_err() || data.is_null() {
            STATS[FAILED].fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    if let Some((at, value)) = back.mark {
        // SAFETY: inside the mapped buffer (made 8 bytes past the mark).
        let seen = unsafe { std::ptr::read_volatile(data.cast::<u8>().add(at as usize).cast::<u32>()) };
        if seen != value {
            // SAFETY: mapped above.
            unsafe { back.buffer.Unmap(0, None) };
            let n = STATS[UNMARKED].fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::warn!(n, seen, value, "a texture read-back's mark was not behind its copies: not written (VICTORY_FETCH_MARK)");
            }
            return;
        }
    }
    let mut ok = true;
    for (i, (footprint, &rows)) in back.footprints.iter().zip(&back.rows).enumerate() {
        let (mip, slice) = (i as u32 % back.mips, i as u32 / back.mips);
        let f = footprint.Footprint;
        let pitch = f.RowPitch as usize;
        let len = pitch * rows as usize * f.Depth as usize;
        // SAFETY: the subresource's part of the mapped buffer (its size
        // covers every footprint's pitched extent: `record_texture`).
        let linear = unsafe { std::slice::from_raw_parts(data.cast::<u8>().add(footprint.Offset as usize), len) };
        if let Err(e) = back.layout.tile(&mut tiled, mip, slice, linear, pitch, pitch * rows as usize) {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(?e, mip, slice, "a texture copied back could not be tiled"));
            ok = false;
            break;
        }
    }
    // SAFETY: mapped above.
    unsafe { back.buffer.Unmap(0, None) };
    if !ok {
        STATS[FAILED].fetch_add(1, Ordering::Relaxed);
        return;
    }
    STATS[LANDED].fetch_add(1, Ordering::Relaxed);
    if tiled == read {
        STATS[UNCHANGED].fetch_add(1, Ordering::Relaxed);
        return;
    }
    // Only the pieces that changed are written: bytes the card left as the
    // title had them are not written back over a title write that may land
    // between the read above and the write.
    let mut written = 0;
    for (lo, hi) in changed_runs(&tiled, &read) {
        written += crate::shadow::put_back(at + lo, &tiled[lo..hi], &read[lo..hi], &[(0, hi - lo)]);
    }
    STATS[BYTES].fetch_add(written as u64, Ordering::Relaxed);
}

/// The runs of 64-byte pieces where `new` differs from `old` (equal
/// lengths), joined where they touch: (start, end) offsets.
fn changed_runs(new: &[u8], old: &[u8]) -> Vec<(usize, usize)> {
    const PIECE: usize = 64;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (i, (a, b)) in new.chunks(PIECE).zip(old.chunks(PIECE)).enumerate() {
        if a != b {
            let (lo, hi) = (i * PIECE, i * PIECE + a.len());
            match runs.last_mut() {
                Some(last) if last.1 == lo => last.1 = hi,
                _ => runs.push((lo, hi)),
            }
        }
    }
    runs
}

/// Every 10 s while read-backs happen, in every build: textures taken,
/// copies recorded and landed, bytes written (a frame), and the CPU time
/// both took.
fn report() {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    static NEXT_MS: AtomicU64 = AtomicU64::new(10_000);
    static LAST: Mutex<([u64; 10], u64)> = Mutex::new(([0; 10], 0));
    let now = START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64;
    let next = NEXT_MS.load(Ordering::Relaxed);
    if now < next || NEXT_MS.compare_exchange(next, now + 10_000, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    let stats: [u64; 10] = std::array::from_fn(|i| STATS[i].load(Ordering::Relaxed));
    let frame = crate::frame::current();
    let (d, frames) = {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let d: [u64; 10] = std::array::from_fn(|i| stats[i] - last.0[i]);
        let frames = frame.saturating_sub(last.1).max(1);
        *last = (stats, frame);
        (d, frames)
    };
    tracing::info!(
        "texture read-back (VICTORY_TEXTURE_BACK), last 10 s: {} textures taken so far; {} copies recorded ({} not: \
         host format differs or more than one subresource), {} landed ({} unchanged), {} stale, {} unmarked, {} failed (memory unreadable, not mapped or not \
         tiled); {} KiB written ({:.1} KiB a frame begun over {frames} frames begun); CPU {:.2} ms recording, \
         {:.2} ms landing ({:.3} ms a frame begun)",
        TEXTURES.load(Ordering::Relaxed),
        d[RECORDED],
        d[OTHER_FORMAT],
        d[LANDED],
        d[UNCHANGED],
        d[STALE],
        d[UNMARKED],
        d[FAILED],
        d[BYTES] >> 10,
        d[BYTES] as f64 / 1024.0 / frames as f64,
        d[RECORD_US] as f64 / 1e3,
        d[LAND_US] as f64 / 1e3,
        (d[RECORD_US] + d[LAND_US]) as f64 / 1e3 / frames as f64,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_readbacks_for_changed_allocations_are_invalidated() {
        let first = track(91, 0x1000, 0x2000);
        let next = track(92, 0x2000, 0x3000);
        let far = track(93, 0x4000, 0x5000);

        remapped(0x0f00, 0x200);
        assert!(!first.valid.load(Ordering::Acquire));
        assert!(next.valid.load(Ordering::Acquire));
        replaced(0x2000);
        assert!(!next.valid.load(Ordering::Acquire));
        assert!(far.valid.load(Ordering::Acquire));

        released(93);
        assert!(!far.valid.load(Ordering::Acquire));
        assert!(track(94, 0x2000, 0x3000).valid.load(Ordering::Acquire));
    }

    #[test]
    fn changed_runs_are_whole_pieces_joined() {
        let old = vec![0u8; 300];
        let mut new = old.clone();
        assert!(changed_runs(&new, &old).is_empty());
        new[5] = 1;
        new[70] = 1;
        new[299] = 1;
        assert_eq!(changed_runs(&new, &old), vec![(0, 128), (256, 300)]);
    }

    #[test]
    fn kinds_parse() {
        assert_eq!(parse(DEFAULT), vec![(256, 256, 41), (256, 256, 16), (64, 64, 16), (128, 128, 36)]);
        assert!(parse("").is_empty());
        assert_eq!(parse("32x16f2, junk"), vec![(32, 16, 2)]);
    }

    /// On the host's own GPU (skipped without one): for each kind copied
    /// back, in three console layouts, a texture with unordered access
    /// written by a compute shader with a pattern (each texel's x and y) is
    /// copied back by `record_texture` and `land` into "title memory": what
    /// lands there detiles to the pattern, the bytes after the texture's
    /// layout are untouched, a second landing of the same texels writes
    /// nothing, nor does one whose mark is missing or whose allocation was
    /// released; a texture with a mip chain is not taken at all.
    #[test]
    fn shader_written_textures_land_tiled_in_title_memory() {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::*;
        use windows::Win32::Graphics::Dxgi::Common::*;
        use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
        use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};
        // SAFETY: plain D3D12 objects of the test's own, waited for.
        unsafe {
            let mut device: Option<ID3D12Device> = None;
            if D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).is_err() {
                eprintln!("no D3D12 device: skipped");
                return;
            }
            let device = device.unwrap();
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
                .unwrap();
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
            list.Close().unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let event = CreateEventW(None, false, false, None).unwrap();
            let mut sent = 0u64;
            let mut run = |record: &dyn Fn(&ID3D12GraphicsCommandList)| {
                allocator.Reset().unwrap();
                list.Reset(&allocator, None).unwrap();
                record(&list);
                list.Close().unwrap();
                queue.ExecuteCommandLists(&[Some(windows::core::Interface::cast(&list).unwrap())]);
                sent += 1;
                queue.Signal(&fence, sent).unwrap();
                fence.SetEventOnCompletion(sent, event).unwrap();
                WaitForSingleObject(event, INFINITE);
                assert!(device.GetDeviceRemovedReason().is_ok());
            };
            // Root signature: one UAV table and the mip as a constant.
            let range = D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
                NumDescriptors: 1,
                ..Default::default()
            };
            let params = [
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                    Anonymous: D3D12_ROOT_PARAMETER_0 {
                        DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE { NumDescriptorRanges: 1, pDescriptorRanges: &range },
                    },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                    Anonymous: D3D12_ROOT_PARAMETER_0 {
                        Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: 0, RegisterSpace: 0, Num32BitValues: 1 },
                    },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
            ];
            let mut blob = None;
            D3D12SerializeRootSignature(
                &D3D12_ROOT_SIGNATURE_DESC { NumParameters: 2, pParameters: params.as_ptr(), ..Default::default() },
                D3D_ROOT_SIGNATURE_VERSION_1,
                &mut blob,
                None,
            )
            .unwrap();
            let blob = blob.unwrap();
            let root: ID3D12RootSignature = device
                .CreateRootSignature(0, std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()))
                .unwrap();
            // Texel (x, y) of mip m: R32: m*65536 + y*256 + x; R32G32: that
            // and its negation; R16G16_UINT: (x + 7m, y + 13m).
            let shaders = [
                (41u32, "RWTexture2D<float> t : register(u0);", "t[id.xy] = m * 65536.0 + id.y * 256.0 + id.x;"),
                (16, "RWTexture2D<float2> t : register(u0);", "float v = m * 65536.0 + id.y * 256.0 + id.x; t[id.xy] = float2(v, -v);"),
                (36, "RWTexture2D<uint2> t : register(u0);", "t[id.xy] = uint2(id.x + 7 * m, id.y + 13 * m);"),
            ];
            let expect = |format: u32, m: u32, x: u32, y: u32| -> Vec<u8> {
                let v = (m * 65536 + y * 256 + x) as f32;
                match format {
                    41 => v.to_le_bytes().to_vec(),
                    16 => [v.to_le_bytes(), (-v).to_le_bytes()].concat(),
                    _ => [((x + 7 * m) as u16).to_le_bytes(), ((y + 13 * m) as u16).to_le_bytes()].concat(),
                }
            };
            let pipeline = |format: u32| -> ID3D12PipelineState {
                let (_, decl, body) = shaders.iter().find(|s| s.0 == format).unwrap();
                let source = format!("{decl} cbuffer c : register(b0) {{ uint m; }}; [numthreads(8, 8, 1)] void main(uint3 id : SV_DispatchThreadID) {{ {body} }}\0");
                let mut code = None;
                let mut errors = None;
                let compiled = D3DCompile(
                    source.as_ptr().cast(),
                    source.len() - 1,
                    None,
                    None,
                    None,
                    windows::core::s!("main"),
                    windows::core::s!("cs_5_0"),
                    0,
                    0,
                    &mut code,
                    Some(&mut errors),
                );
                if let Err(e) = compiled {
                    let text = errors.map(|b| {
                        String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize())).into_owned()
                    });
                    panic!("test shader for format {format} did not compile: {e} {text:?}");
                }
                let code = code.unwrap();
                device
                    .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.GetBufferPointer(), BytecodeLength: code.GetBufferSize() },
                        ..Default::default()
                    })
                    .unwrap()
            };
            let heap: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 16,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .unwrap();
            let step = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
            let mut checked = 0;
            let mut skipped = 0;
            let mut refused = 0;
            for (width, height, format) in parse(DEFAULT) {
                let pso = pipeline(format);
                // The layouts the console's library picks for a texture
                // with unordered access (4 KiB S without a layout given,
                // `crate::resource::texture_layout`), 64 KiB S and its xor
                // form, linear, and the render target's 64 KiB R_X (which
                // the formats crate may not address for every format:
                // then a texture's upload fails the same way).
                for swizzle in [Swizzle::SW_4KB_S, Swizzle::SW_64KB_S, Swizzle::SW_64KB_S_X, Swizzle::SW_64KB_R_X, Swizzle::LINEAR] {
                    for mips in [1u32, 3] {
                        let what = format!("{width}x{height} f{format} swizzle {} mips {mips}", swizzle.0);
                        let layout = Texture::new(&TextureDesc {
                            dimension: Dimension::Tex2d,
                            width: width as u32,
                            height,
                            depth_or_array: 1,
                            mips,
                            format,
                            samples: 1,
                            swizzle,
                        })
                        .unwrap_or_else(|e| panic!("{what}: no layout ({e:?})"));
                        let [plane] = &layout.planes[..] else { panic!("{what}: planes") };
                        if !plane.layout.addressable() {
                            assert!(swizzle != Swizzle::SW_4KB_S && swizzle != Swizzle::LINEAR, "{what}: not addressable");
                            eprintln!("{what}: the formats crate cannot address this layout: skipped");
                            skipped += 1;
                            continue;
                        }
                        let size = layout.size as usize;
                        let texture = {
                            let mut t: Option<ID3D12Resource> = None;
                            device
                                .CreateCommittedResource(
                                    &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                                    D3D12_HEAP_FLAG_NONE,
                                    &D3D12_RESOURCE_DESC {
                                        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                                        Width: width,
                                        Height: height,
                                        DepthOrArraySize: 1,
                                        MipLevels: mips as u16,
                                        Format: DXGI_FORMAT(format as i32),
                                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                                        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                                        ..Default::default()
                                    },
                                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                    None,
                                    &mut t,
                                )
                                .unwrap();
                            t.unwrap()
                        };
                        for m in 0..mips {
                            let at = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: heap.GetCPUDescriptorHandleForHeapStart().ptr + m as usize * step };
                            device.CreateUnorderedAccessView(
                                &texture,
                                None,
                                Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                                    Format: DXGI_FORMAT(format as i32),
                                    ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 { Texture2D: D3D12_TEX2D_UAV { MipSlice: m, PlaneSlice: 0 } },
                                }),
                                at,
                            );
                        }
                        // "Title memory": the layout and a page after it,
                        // filled with a byte nothing writes.
                        let mut title = vec![0xcdu8; size + 4096];
                        let at = title.as_mut_ptr() as usize;
                        let ticket = track(0x7e57_0000 + checked, at, at + size);
                        let back = std::cell::RefCell::new(None);
                        run(&|list| {
                            list.SetDescriptorHeaps(&[Some(heap.clone())]);
                            list.SetComputeRootSignature(&root);
                            list.SetPipelineState(&pso);
                            for m in 0..mips {
                                let gpu = D3D12_GPU_DESCRIPTOR_HANDLE { ptr: heap.GetGPUDescriptorHandleForHeapStart().ptr + (m as usize * step) as u64 };
                                list.SetComputeRootDescriptorTable(0, gpu);
                                list.SetComputeRoot32BitConstant(1, m, 0);
                                list.Dispatch(((width as u32) >> m).div_ceil(8), (height >> m).div_ceil(8), 1);
                            }
                            list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                                Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                                    Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                                        pResource: std::mem::transmute_copy(&texture),
                                        Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                                        StateBefore: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                        StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE,
                                    }),
                                },
                                ..Default::default()
                            }]);
                            *back.borrow_mut() = record_texture(&device, list, &texture, plane.layout.clone(), format, ticket.clone());
                        });
                        if mips > 1 {
                            // More than one subresource: not taken, nothing
                            // written (the others may hold what no shader
                            // wrote).
                            assert!(back.into_inner().is_none(), "{what}: a texture of {mips} mips recorded");
                            assert!(title.iter().all(|&b| b == 0xcd));
                            refused += 1;
                            continue;
                        }
                        let back = back.into_inner().unwrap_or_else(|| panic!("{what}: nothing recorded"));
                        assert_eq!(back.range(), (at, at + size));
                        let bytes_before = STATS[BYTES].load(Ordering::Relaxed);
                        land(&back);
                        assert!(STATS[BYTES].load(Ordering::Relaxed) > bytes_before, "{what}: nothing written");
                        assert!(title[size..].iter().all(|&b| b == 0xcd), "{what}: bytes after the layout written");
                        let n = layout.planes[0].layout.desc.element_bytes as usize;
                        for m in 0..mips {
                            let (w, h) = (((width as u32) >> m).max(1), (height >> m).max(1));
                            let mut linear = vec![0u8; w as usize * h as usize * n];
                            plane
                                .layout
                                .detile(&title[..size], m, 0, &mut linear, w as usize * n, w as usize * h as usize * n)
                                .unwrap();
                            for y in 0..h {
                                for x in 0..w {
                                    let i = (y as usize * w as usize + x as usize) * n;
                                    assert_eq!(linear[i..i + n], expect(format, m, x, y)[..], "{what}: mip {m} texel ({x}, {y})");
                                }
                            }
                        }
                        // The same texels again: unchanged, nothing written.
                        let (bytes, unchanged) = (STATS[BYTES].load(Ordering::Relaxed), STATS[UNCHANGED].load(Ordering::Relaxed));
                        land(&back);
                        assert_eq!(STATS[BYTES].load(Ordering::Relaxed), bytes, "{what}: unchanged texels written again");
                        assert_eq!(STATS[UNCHANGED].load(Ordering::Relaxed), unchanged + 1);
                        // The mark not behind the copies (as on a device
                        // lost before the copies ran): nothing written.
                        title[..size].fill(0);
                        let (mark_at, _) = back.mark.expect("the mark is on by default");
                        let mut data = std::ptr::null_mut();
                        back.buffer.Map(0, None, Some(&mut data)).unwrap();
                        std::ptr::write_volatile(data.cast::<u8>().add(mark_at as usize).cast::<u32>(), 0);
                        back.buffer.Unmap(0, None);
                        let unmarked = STATS[UNMARKED].load(Ordering::Relaxed);
                        land(&back);
                        assert!(title[..size].iter().all(|&b| b == 0), "{what}: an unmarked read-back written");
                        assert_eq!(STATS[UNMARKED].load(Ordering::Relaxed), unmarked + 1);
                        // The allocation released meanwhile: not written.
                        released(0x7e57_0000 + checked);
                        land(&back);
                        assert!(title[..size].iter().all(|&b| b == 0), "{what}: a released allocation written");
                        checked += 1;
                        eprintln!("{what}: {size} bytes landed and checked");
                    }
                }
            }
            assert_eq!(checked + skipped + refused, 4 * 5 * 2);
            assert!(checked >= 4 * 3, "{checked} checked");
        }
    }
}
