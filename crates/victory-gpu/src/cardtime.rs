//! The card's own clock around every submission (`VICTORY_CARD_TIME=1`,
//! diagnostics; off by default): a timestamp before a submission's first
//! list and one after its last, read back once the submission has finished,
//! and the moment the processor sent it. The per-second line says how long
//! the card was busy with our work in a frame, how long it stood idle
//! between our submissions, and how long a submission waited in the queue
//! behind earlier ones before the card began it: whether the card or the
//! processor paces the frame.
//!
//! Two tiny lists a submission, from a ring; a ring entry is used again only
//! once the submission it stamped has finished (`FINISHED`), else that
//! submission goes unstamped (counted).

use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

/// Submissions the ring holds.
const RING: usize = 1024;

/// Whether submissions are stamped (`VICTORY_CARD_TIME=1`).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_TIME").is_some_and(|v| v == "1"))
}

struct Entry {
    begin: (ID3D12CommandAllocator, ID3D12GraphicsCommandList),
    end: (ID3D12CommandAllocator, ID3D12GraphicsCommandList),
    /// The submission stamped (0: none), whether its stamps were read, and
    /// the processor's clock (QPC) when it was sent.
    submission: u64,
    read: bool,
    sent: i64,
}

struct Ring {
    heap: ID3D12QueryHeap,
    back: ID3D12Resource,
    stamps: *const u64,
    entries: Vec<Option<Entry>>,
    next: usize,
    /// Since the last line: (card begin, card end, sent) of finished
    /// submissions, in card ticks and QPC.
    spans: Vec<(u64, u64, i64)>,
    skipped: u64,
}

// SAFETY: the mapped pointer is of a read-back buffer the ring owns; all
// use is under the ring's lock.
unsafe impl Send for Ring {}

static RING_STATE: Mutex<Option<Ring>> = Mutex::new(None);

fn make_ring(device: &ID3D12Device) -> Option<Ring> {
    let mut heap: Option<ID3D12QueryHeap> = None;
    let mut back: Option<ID3D12Resource> = None;
    let readback = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: (2 * RING * 8) as u64,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    // SAFETY: plain object creation and a persistent map of a read-back
    // buffer.
    unsafe {
        device
            .CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: (2 * RING) as u32, NodeMask: 0 },
                &mut heap,
            )
            .ok()?;
        device
            .CreateCommittedResource(&readback, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut back)
            .ok()?;
        let back = back?;
        let mut data = std::ptr::null_mut();
        back.Map(0, None, Some(&mut data)).ok()?;
        Some(Ring {
            heap: heap?,
            back,
            stamps: data.cast::<u64>(),
            entries: (0..RING).map(|_| None).collect(),
            next: 0,
            spans: Vec::new(),
            skipped: 0,
        })
    }
}

fn pair(device: &ID3D12Device) -> Option<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)> {
    // SAFETY: plain object creation; the list is closed for its first reset.
    unsafe {
        let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).ok()?;
        let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).ok()?;
        list.Close().ok()?;
        Some((allocator, list))
    }
}

fn qpc() -> i64 {
    let mut now = 0i64;
    // SAFETY: an out parameter.
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut now) };
    now
}

/// Reads the stamps of the finished submissions not read yet.
fn harvest(ring: &mut Ring, finished: u64) {
    for (i, slot) in ring.entries.iter_mut().enumerate() {
        let Some(entry) = slot else {
            continue;
        };
        if entry.submission == 0 || entry.read || entry.submission > finished {
            continue;
        }
        entry.read = true;
        // SAFETY: inside the mapped read-back buffer; the submission that
        // resolved into it has finished.
        let (a, b) = unsafe { (ring.stamps.add(2 * i).read_volatile(), ring.stamps.add(2 * i + 1).read_volatile()) };
        if b >= a && a != 0 {
            ring.spans.push((a, b, entry.sent));
        }
    }
}

/// For submission `submission`, about to be sent by the submitting thread
/// (under the submission lock): the list to run first and the list to run
/// last, or none if stamping is off or the ring has no free entry.
pub(crate) fn stamps(device: &ID3D12Device, submission: u64, finished: u64) -> Option<(ID3D12CommandList, ID3D12CommandList)> {
    if !on() {
        return None;
    }
    let mut state = RING_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.is_none() {
        *state = make_ring(device);
    }
    let ring = state.as_mut()?;
    harvest(ring, finished);
    let i = ring.next;
    if let Some(entry) = &ring.entries[i]
        && entry.submission > finished
    {
        ring.skipped += 1;
        return None;
    }
    if ring.entries[i].is_none() {
        ring.entries[i] = Some(Entry { begin: pair(device)?, end: pair(device)?, submission: 0, read: true, sent: 0 });
    }
    ring.next = (i + 1) % RING;
    let (heap, back) = (ring.heap.clone(), ring.back.clone());
    let entry = ring.entries[i].as_mut()?;
    // SAFETY: lists of ours whose last use has finished; our heap and
    // read-back buffer.
    unsafe {
        entry.begin.0.Reset().ok()?;
        entry.begin.1.Reset(&entry.begin.0, None).ok()?;
        entry.begin.1.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, (2 * i) as u32);
        entry.begin.1.Close().ok()?;
        entry.end.0.Reset().ok()?;
        entry.end.1.Reset(&entry.end.0, None).ok()?;
        entry.end.1.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, (2 * i + 1) as u32);
        entry.end.1.ResolveQueryData(&heap, D3D12_QUERY_TYPE_TIMESTAMP, (2 * i) as u32, 2, &back, (2 * i * 8) as u64);
        entry.end.1.Close().ok()?;
    }
    entry.submission = submission;
    entry.read = false;
    entry.sent = qpc();
    use windows::core::Interface;
    Some((entry.begin.1.cast().ok()?, entry.end.1.cast().ok()?))
}

/// With the frame line: the card's account over `frames` frames of `ms`
/// each, from the submissions finished since the last line.
pub(crate) fn line(queue: &ID3D12CommandQueue, frames: f64, finished: u64) {
    if !on() {
        return;
    }
    let (mut spans, skipped) = {
        let mut state = RING_STATE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(ring) = state.as_mut() else {
            return;
        };
        harvest(ring, finished);
        (std::mem::take(&mut ring.spans), std::mem::take(&mut ring.skipped))
    };
    if spans.is_empty() {
        return;
    }
    // SAFETY: plain queries.
    let (frequency, calibration) = unsafe {
        let f = queue.GetTimestampFrequency().unwrap_or(1).max(1);
        let (mut gpu, mut cpu) = (0u64, 0u64);
        let ok = queue.GetClockCalibration(&mut gpu, &mut cpu).is_ok();
        (f, ok.then_some((gpu, cpu)))
    };
    let mut qpf = 0i64;
    // SAFETY: an out parameter.
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceFrequency(&mut qpf) };
    let ms = |ticks: u64| ticks as f64 * 1e3 / frequency as f64;
    spans.sort_unstable_by_key(|s| s.0);
    let (mut busy, mut idle, mut longest) = (0u64, 0u64, 0u64);
    let mut reach = spans[0].0;
    for &(a, b, _) in &spans {
        if a > reach {
            idle += a - reach;
        }
        busy += b.saturating_sub(a.max(reach));
        reach = reach.max(b);
        longest = longest.max(b - a);
    }
    // How long submissions waited between being sent and the card beginning
    // them (card ticks to QPC by the calibration).
    let mut waited = Vec::new();
    if let Some((gpu, cpu)) = calibration
        && qpf > 0
    {
        for &(a, _, sent) in &spans {
            let card_ms = (a as f64 - gpu as f64) * 1e3 / frequency as f64;
            let sent_ms = (sent as f64 - cpu as f64) * 1e3 / qpf as f64;
            waited.push(card_ms - sent_ms);
        }
        waited.sort_by(f64::total_cmp);
    }
    let pick = |q: f64| waited.get(((waited.len() as f64 - 1.0) * q) as usize).copied().unwrap_or(0.0);
    tracing::info!(
        "the card's own clock per frame (VICTORY_CARD_TIME=1): busy with our submissions {:.1} ms, idle between them {:.1} ms, \
         {:.1} submissions (the longest {:.1} ms; {} unstamped); a submission began on the card after it was sent: median \
         {:.2} ms, 90th percentile {:.2} ms, most {:.2} ms",
        ms(busy) / frames,
        ms(idle) / frames,
        spans.len() as f64 / frames,
        ms(longest),
        skipped,
        pick(0.5),
        pick(0.9),
        pick(1.0)
    );
}
