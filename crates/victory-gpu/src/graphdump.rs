//! Diagnostics: the face network's buffers as the card holds them, to files
//! (`VICTORY_GRAPH_DUMP=<first frame>,<full frames>,<folder>`).
//!
//! What a probe at record time reads from title memory is what earlier
//! submissions left, not what one dispatch of a list gives the next: the
//! face network (65 `compute_graph_ops` programs, about a thousand
//! dispatches a frame through one 24 MB arena) can only be checked
//! dispatch by dispatch from copies made on the card, in the list, between
//! its dispatches. In frames `first .. first + full`, before each network
//! dispatch: the arena's used part (a view of 16 MB or more: the prefix up
//! to the last piece title memory holds anything in, at least 1 MB), and
//! every other buffer view the first time the frame binds it; in frame
//! `first + full` only the first-time copies (the last full frame's final
//! state of everything the network wrote). For the head extract
//! (970c4fbb3b017c0e), the blend-shape mapping (61e54e90adccaf6a) and the
//! head skinning (36e010b53acffd77), in all those frames: each view before
//! and after the dispatch. The copies land in CPU-visible buffers and are
//! written, with an index of what each is, a few frames after the window
//! (`frame-<n>.bin`, `index.txt`).

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::HostList;

/// Room for every copy of the window; past it copies are left out (and
/// counted).
const CAP: u64 = 3_200 << 20;
const CHUNK: u64 = 64 << 20;

pub(crate) struct Window {
    pub(crate) first: u64,
    pub(crate) full: u64,
    /// Passes after the full ones with first-sight copies only
    /// (`VICTORY_GRAPH_DUMP_LIGHT`, default 8).
    pub(crate) light: u64,
    folder: PathBuf,
}

pub(crate) fn window() -> Option<&'static Window> {
    static W: OnceLock<Option<Window>> = OnceLock::new();
    W.get_or_init(|| {
        let v = std::env::var("VICTORY_GRAPH_DUMP").ok()?;
        let mut parts = v.splitn(3, ',');
        let first = parts.next()?.trim().parse().ok()?;
        let full = parts.next()?.trim().parse().ok()?;
        let folder = PathBuf::from(parts.next()?.trim());
        let _ = std::fs::create_dir_all(&folder);
        let light = std::env::var("VICTORY_GRAPH_DUMP_LIGHT").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(8);
        tracing::info!(first, full, light, folder = %folder.display(), "face network dump window (VICTORY_GRAPH_DUMP)");
        Some(Window { first, full, light, folder })
    })
    .as_ref()
}

struct Chunk {
    buffer: ID3D12Resource,
    data: usize,
    used: u64,
}

#[derive(Default)]
struct State {
    chunks: Vec<Chunk>,
    /// (frame, chunk, offset, len) of each copy, in index order.
    copies: Vec<(u64, usize, u64, u64)>,
    index: Vec<String>,
    total: u64,
    left_out: u64,
    /// Per frame: the title buffers copied at first sight.
    seen: Vec<(u64, u64, u64)>,
    /// Title memory read on the CPU: (pass, label, address, bytes).
    cpu: Vec<(u64, String, u64, Vec<u8>)>,
    /// The frame the window ended in.
    ended: Option<u64>,
    flushed: bool,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut s = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(s.get_or_insert_with(State::default))
}

/// Title memory read at record time, written to `cpu.bin`.
pub(crate) fn cpu(pass: u64, label: &str, address: u64, bytes: Vec<u8>) {
    with(|s| s.cpu.push((pass, label.to_owned(), address, bytes)));
}

/// The window's passes are over (at frame `frame`).
pub(crate) fn finish(frame: u64) {
    with(|s| {
        s.ended.get_or_insert(frame);
    });
}

/// A line of the index (dispatch records, notes).
pub(crate) fn note(line: String) {
    with(|s| s.index.push(line));
}

/// Whether `frame` already copied title buffer `address`+`len` at first
/// sight; marks it copied.
pub(crate) fn first_sight(frame: u64, address: u64, len: u64) -> bool {
    with(|s| {
        if s.seen.contains(&(frame, address, len)) {
            false
        } else {
            s.seen.push((frame, address, len));
            true
        }
    })
}

/// Records a copy of title memory `address`+`len` as the card holds it at
/// this point of `list` (after every dispatch recorded before it), labelled
/// `label` in the index. False if it has no host copy or the room is used.
pub(crate) fn copy(list: &HostList, frame: u64, address: u64, len: u64, label: &str) -> bool {
    let Some(shadow) = crate::shadow::find(address, len.max(1)) else {
        note(format!("nocopy frame={frame} {label} addr={address:#x} len={len:#x}"));
        return false;
    };
    let len = len.min(shadow.left(address));
    if len == 0 {
        return false;
    }
    let Some(host) = crate::host::host() else { return false };
    // SAFETY: a live buffer.
    let base = unsafe { shadow.buffer.GetGPUVirtualAddress() };
    let from = shadow.gpu(address) - base;
    let placed = with(|s| {
        if s.total + len > CAP {
            s.left_out += len;
            return None;
        }
        if s.chunks.last().is_none_or(|c| c.used + len > CHUNK.max(len)) {
            let size = CHUNK.max(len.next_multiple_of(1 << 20));
            let (buffer, data) = host.shared_buffer(size)?;
            s.chunks.push(Chunk { buffer, data: data as usize, used: 0 });
        }
        let i = s.chunks.len() - 1;
        let c = &mut s.chunks[i];
        let at = c.used;
        c.used = (at + len).next_multiple_of(256);
        s.total += len;
        let n = s.copies.len();
        s.copies.push((frame, i, at, len));
        s.index.push(format!(
            "copy {n} frame={frame} {label} addr={address:#x} len={len:#x} shadow={}",
            shadow.id()
        ));
        Some((c.buffer.clone(), at))
    });
    let Some((to, at)) = placed else { return false };
    let uav = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        ..Default::default()
    };
    let (written, read) = (D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
    // SAFETY: an open list; live buffers; the ranges lie in them.
    // shortcut: the title buffer's copy is taken to be in the unordered
    // access state (vkd3d-proton turns buffer transitions into memory
    // barriers whatever the states named).
    unsafe {
        list.list.ResourceBarrier(&[uav, crate::host::transition(&shadow.buffer, written, read)]);
        list.list.CopyBufferRegion(&to, at, &shadow.buffer, from, len);
        list.list.ResourceBarrier(&[crate::host::transition(&shadow.buffer, read, written)]);
    }
    list.keep(&to);
    true
}

/// At the present of `frame`: once the window is three frames past, every
/// submission settled, the copies go to files.
pub(crate) fn flush(frame: u64) {
    let Some(w) = window() else { return };
    watch_present(frame);
    let _ = w.first;
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = guard.as_mut() else { return };
    if s.flushed || s.ended.is_none_or(|e| frame < e + 4) {
        return;
    }
    s.flushed = true;
    use std::io::Write;
    let mut files: std::collections::BTreeMap<u64, (std::fs::File, u64)> = std::collections::BTreeMap::new();
    let mut index = String::new();
    let mut n = 0usize;
    for line in &s.index {
        if let Some(rest) = line.strip_prefix("copy ") {
            let (f, chunk, at, len) = s.copies[n];
            n += 1;
            let entry = files.entry(f).or_insert_with(|| {
                let path = w.folder.join(format!("frame-{f}.bin"));
                (std::fs::File::create(path).expect("dump file"), 0)
            });
            // SAFETY: the chunk's mapping holds `at + len` bytes, written by
            // the card in a submission that has settled.
            let bytes = unsafe { std::slice::from_raw_parts((s.chunks[chunk].data as *const u8).add(at as usize), len as usize) };
            let _ = entry.0.write_all(bytes);
            index.push_str(&format!("copy {rest} file=frame-{f}.bin at={:#x}\n", entry.1));
            entry.1 += len;
        } else {
            index.push_str(line);
            index.push('\n');
        }
    }
    let mut cpu = Vec::new();
    for (pass, label, address, bytes) in &s.cpu {
        index.push_str(&format!("cpu frame={pass} {label} addr={address:#x} len={:#x} at={:#x}\n", bytes.len(), cpu.len()));
        cpu.extend_from_slice(bytes);
    }
    let _ = std::fs::write(w.folder.join("cpu.bin"), cpu);
    index.push_str(&format!("total={:#x} left_out={:#x}\n", s.total, s.left_out));
    let _ = std::fs::write(w.folder.join("index.txt"), index);
    tracing::info!(total = s.total, left_out = s.left_out, copies = s.copies.len(), "face network dump written (VICTORY_GRAPH_DUMP)");
    s.chunks.clear();
}

/// Who writes the face controls (the Formula dispatch's input, three
/// buffers in turn): the ranges watched (the controls, and the sources of
/// copies into them), each logged write, and per frame which 64-byte
/// pieces of the controls' title memory changed since the last present.
static WATCH: Mutex<Vec<(u64, u64, &'static str)>> = Mutex::new(Vec::new());

fn overlaps(a: u64, n: u64, b: u64, m: u64) -> bool {
    a < b + m && b < a + n
}

fn watching() -> bool {
    let Some(w) = window() else { return false };
    let f = crate::queue::presented() + 1;
    f >= w.first && f < w.first + 16
}

/// The Formula dispatch reads the controls at `address`.
pub(crate) fn watch_controls(address: u64, len: u64) {
    let mut w = WATCH.lock().unwrap_or_else(|e| e.into_inner());
    if !w.iter().any(|&(a, _, _)| a == address) {
        w.push((address, len, "controls"));
        tracing::info!(address = format_args!("{address:#x}"), len, "face controls watched");
    }
}

/// A copy recorded into title memory `dst` from `src` (`kind`: which
/// command): logged where it writes a watched range; its source is watched
/// from then on.
pub(crate) fn watch_copy(kind: &str, dst: u64, src: u64, len: u64) {
    if !watching() {
        return;
    }
    let mut w = WATCH.lock().unwrap_or_else(|e| e.into_inner());
    let hit = w.iter().find(|&&(a, n, _)| overlaps(dst, len, a, n)).map(|&(a, _, what)| (a, what));
    if let Some((a, what)) = hit {
        let line = format!(
            "watch copy frame={} {kind} dst={dst:#x} src={src:#x} len={len:#x} into {what} {a:#x}",
            crate::queue::presented() + 1
        );
        tracing::info!("{line}");
        note(line);
        if !w.iter().any(|&(b, m, _)| b == src && m == len) {
            w.push((src, len, "copy source"));
        }
    }
}

/// A dispatch's buffer view `address`+`len`: logged where it touches a
/// watched range.
pub(crate) fn watch_view(program: &str, label: &str, address: u64, len: u64) {
    if !watching() {
        return;
    }
    let w = WATCH.lock().unwrap_or_else(|e| e.into_inner());
    for &(a, n, what) in w.iter() {
        if overlaps(address, len, a, n) {
            let line = format!(
                "watch view frame={} program={program} {label} {address:#x}+{len:#x} touches {what} {a:#x}+{n:#x}",
                crate::queue::presented() + 1
            );
            note(line);
        }
    }
}

/// At a present: which pieces of each watched range's title memory changed
/// since the last.
pub(crate) fn watch_present(frame: u64) {
    if !watching() {
        return;
    }
    static LAST: Mutex<Vec<(u64, Vec<u64>)>> = Mutex::new(Vec::new());
    let ranges: Vec<(u64, u64, &str)> = WATCH.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    for (a, n, what) in ranges {
        let mut b = vec![0u8; n as usize];
        if !crate::host::read_into(a as *const u8, &mut b) {
            continue;
        }
        let hashes: Vec<u64> = b
            .chunks(64)
            .map(|c| c.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| (h ^ u64::from(x)).wrapping_mul(0x0100_0000_01b3)))
            .collect();
        let changed = match last.iter().find(|(b, _)| *b == a) {
            Some((_, old)) => hashes.iter().zip(old).filter(|(x, y)| x != y).count(),
            None => usize::MAX,
        };
        note(format!("watch present frame={frame} {what} {a:#x}+{n:#x} pieces changed {changed} of {}", hashes.len()));
        match last.iter_mut().find(|(b, _)| *b == a) {
            Some(e) => e.1 = hashes,
            None => last.push((a, hashes)),
        }
    }
}
