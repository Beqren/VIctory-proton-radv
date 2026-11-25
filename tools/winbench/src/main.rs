//! Times, in isolation, the Windows memory primitives the layer's write tracking
//! (crates/victory-gpu/src/watch.rs) leans on: GetWriteWatch with reset, VirtualProtect,
//! a write fault taken through a vectored handler (the guard path), VirtualQuery, mapping
//! and unmapping views, committing and decommitting. Each case runs alone and then while
//! four more threads do the same on regions of their own, because Wine takes one lock for
//! most of these. Under Wine a frame of the title spends seconds in them; on Windows the
//! same code runs at 2-3 frames a second, so the point is to see which ones Wine makes
//! dear.
//!
//! usage: winbench all | <section>... (ww split fail prot guard query map commit)
//! Prints one markdown table row per measurement as soon as it is measured.

#[cfg(not(windows))]
fn main() {
    eprintln!("winbench times Windows memory calls: build it for x86_64-pc-windows-msvc and run it under Windows or Proton");
}

#[cfg(windows)]
fn main() {
    bench::main();
}

#[cfg(windows)]
mod bench {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        CloseHandle, EXCEPTION_ACCESS_VIOLATION, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
        NTSTATUS,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, GetWriteWatch, MEM_COMMIT, MEM_DECOMMIT,
        MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
        MapViewOfFile, PAGE_READONLY, PAGE_READWRITE, UnmapViewOfFile, VirtualAlloc,
        VirtualFree, VirtualProtect, VirtualQuery,
    };

    const PAGE: usize = 4096;
    const KIB: usize = 1024;
    const MIB: usize = 1024 * KIB;
    const GIB: usize = 1024 * MIB;
    const MEM_WRITE_WATCH: u32 = 0x20_0000;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    /// The other threads of a contended run (the layer has several recording threads).
    const OTHERS: usize = 4;
    /// Contended runs keep each thread's region at or under this, so five threads fit in memory.
    const CONTENDED_MAX: usize = 256 * MIB;
    /// Time spent repeating one measurement, after warming up.
    const BUDGET: Duration = Duration::from_millis(300);
    const MIN_REPS: usize = 3;
    const MAX_REPS: usize = 5000;

    // ---------------------------------------------------------------- memory helpers

    struct Region {
        base: *mut u8,
        len: usize,
        view: Option<HANDLE>,
    }
    // SAFETY: the region is owned by one case, which one thread uses at a time.
    unsafe impl Send for Region {}

    impl Region {
        fn private(len: usize, watched: bool) -> Region {
            let kind = MEM_RESERVE | MEM_COMMIT | if watched { MEM_WRITE_WATCH } else { 0 };
            // SAFETY: a fresh allocation.
            let base = unsafe { VirtualAlloc(null(), len, kind, PAGE_READWRITE) } as *mut u8;
            assert!(!base.is_null(), "VirtualAlloc of {len} bytes failed: {}", last());
            Region { base, len, view: None }
        }
        /// A view of a pagefile-backed section, as the layer maps the title's heaps.
        fn view(len: usize) -> Region {
            let section = section(len);
            // SAFETY: a valid section handle.
            let v = unsafe { MapViewOfFile(section, FILE_MAP_ALL_ACCESS, 0, 0, len) };
            assert!(!v.Value.is_null(), "MapViewOfFile of {len} bytes failed: {}", last());
            Region { base: v.Value as *mut u8, len, view: Some(section) }
        }
        fn pages(&self) -> usize {
            self.len / PAGE
        }
        fn at(&self, page: usize) -> *mut u8 {
            // SAFETY: callers stay inside the region.
            unsafe { self.base.add(page * PAGE) }
        }
        fn touch(&self, step: usize) {
            for p in (0..self.pages()).step_by(step) {
                // SAFETY: committed read-write memory of ours.
                unsafe { self.at(p).write_volatile(1) };
            }
        }
        fn protect(&self, first: usize, pages: usize, prot: u32) {
            let mut old = 0;
            // SAFETY: inside our region.
            let ok = unsafe { VirtualProtect(self.at(first).cast(), pages * PAGE, prot, &mut old) };
            assert!(ok != 0, "VirtualProtect failed: {}", last());
        }
    }

    impl Drop for Region {
        fn drop(&mut self) {
            // SAFETY: we own the allocation or view.
            unsafe {
                match self.view {
                    Some(section) => {
                        UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.base.cast() });
                        CloseHandle(section);
                    }
                    None => {
                        VirtualFree(self.base.cast(), 0, MEM_RELEASE);
                    }
                }
            }
        }
    }

    fn section(len: usize) -> HANDLE {
        // SAFETY: an anonymous pagefile-backed section.
        let h = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                null(),
                PAGE_READWRITE,
                (len as u64 >> 32) as u32,
                len as u32,
                null(),
            )
        };
        assert!(!h.is_null(), "CreateFileMappingW of {len} bytes failed: {}", last());
        h
    }

    fn last() -> u32 {
        // SAFETY: no preconditions.
        unsafe { GetLastError() }
    }

    /// GetWriteWatch with reset over `len` bytes at `base`; the pages found and the result.
    fn get_write_watch(base: *mut u8, len: usize, buf: &mut [*mut c_void]) -> (usize, u32) {
        let mut count = buf.len();
        let mut gran = 0u32;
        // SAFETY: the buffer holds `count` entries.
        let r = unsafe {
            GetWriteWatch(
                WRITE_WATCH_FLAG_RESET,
                base.cast(),
                len,
                buf.as_mut_ptr(),
                &mut count,
                &mut gran,
            )
        };
        (if r == 0 { count } else { 0 }, r)
    }

    // ---------------------------------------------------------------- the runner

    /// One repetition's figures.
    #[derive(Clone, Copy, Default)]
    struct Sample {
        /// Nanoseconds per call of the primitive measured.
        ns: f64,
        /// A second figure some cases report (see the case's `extra`).
        aux: f64,
    }

    trait Case: Send {
        fn rep(&mut self) -> Sample;
        /// Pages each call covers, for ns per page.
        fn pages(&self) -> usize;
        /// Words for the extra column given the median second figure.
        fn extra(&self, _aux: f64) -> String {
            String::new()
        }
    }

    #[derive(Clone, Copy)]
    struct Line<'a> {
        name: &'a str,
        size: usize,
        dirty: &'a str,
    }

    fn size_text(n: usize) -> String {
        if n >= GIB && n.is_multiple_of(GIB) {
            format!("{} GiB", n / GIB)
        } else if n >= MIB && n.is_multiple_of(MIB) {
            format!("{} MiB", n / MIB)
        } else {
            format!("{} KiB", n / KIB)
        }
    }

    fn median(v: &mut [f64]) -> f64 {
        v.sort_by(|a, b| a.total_cmp(b));
        v[v.len() / 2]
    }

    /// Runs the case built by `make(0)` on this thread, with `others` threads looping their
    /// own `make(i)` meanwhile, and prints one row.
    fn run(line: Line, others: usize, make: &(dyn Fn(usize) -> Box<dyn Case> + Sync)) {
        let stop = AtomicBool::new(false);
        let bg_reps = AtomicU64::new(0);
        let ready = Barrier::new(others + 1);
        let mut main_case = make(0);
        let (reps, ns, aux) = std::thread::scope(|s| {
            for i in 1..=others {
                let (stop, bg_reps, ready) = (&stop, &bg_reps, &ready);
                s.spawn(move || {
                    let mut c = make(i);
                    c.rep();
                    ready.wait();
                    while !stop.load(Ordering::Relaxed) {
                        c.rep();
                        bg_reps.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
            for _ in 0..2 {
                main_case.rep();
            }
            ready.wait();
            let start = Instant::now();
            let mut ns = Vec::new();
            let mut aux = Vec::new();
            while ns.len() < MIN_REPS || (start.elapsed() < BUDGET && ns.len() < MAX_REPS) {
                let s = main_case.rep();
                ns.push(s.ns);
                aux.push(s.aux);
            }
            stop.store(true, Ordering::Relaxed);
            let n = ns.len();
            (n, median(&mut ns), median(&mut aux))
        });
        let pages = main_case.pages().max(1);
        let mut extra = main_case.extra(aux);
        if others > 0 {
            let b = bg_reps.load(Ordering::Relaxed);
            extra = format!("{extra}{}others did {b} reps", if extra.is_empty() { "" } else { "; " });
        }
        println!(
            "| {} | {} | {} | {} | {} | {:.2} | {:.2} | {} |",
            line.name,
            others + 1,
            size_text(line.size),
            line.dirty,
            reps,
            ns / 1000.0,
            ns / pages as f64,
            extra
        );
    }

    /// Runs alone, then contended when the size allows it.
    fn both(line: Line, make: &(dyn Fn(usize) -> Box<dyn Case> + Sync)) {
        run(line, 0, make);
        if line.size <= CONTENDED_MAX {
            run(line, OTHERS, make);
        } else {
            println!(
                "| {} | {} | {} | {} | - | - | - | contended run skipped above {} a thread |",
                line.name,
                OTHERS + 1,
                size_text(line.size),
                line.dirty,
                size_text(CONTENDED_MAX)
            );
        }
    }

    // ---------------------------------------------------------------- cases

    /// GetWriteWatch with reset over one watched region, `step`-th pages dirtied between calls.
    struct WriteWatch {
        r: Region,
        buf: Vec<*mut c_void>,
        /// Dirty every `step`-th page before each call; 0 = none.
        step: usize,
        /// Pieces the region is asked as (1 = one call).
        pieces: usize,
        found: usize,
    }
    // SAFETY: the buffer holds addresses only.
    unsafe impl Send for WriteWatch {}

    impl WriteWatch {
        fn new(len: usize, step: usize, pieces: usize) -> WriteWatch {
            let r = Region::private(len, true);
            r.touch(1);
            let mut buf = vec![null_mut(); r.pages()];
            get_write_watch(r.base, r.len, &mut buf);
            WriteWatch { r, buf, step, pieces, found: 0 }
        }
    }

    impl Case for WriteWatch {
        fn rep(&mut self) -> Sample {
            // The first write to a page after a reset is a cost too: under Wine's
            // userfaultfd path it is a kernel fault, under the signal path a Wine handler.
            let mut wrote = 0;
            let t = Instant::now();
            if self.step > 0 {
                for p in (0..self.r.pages()).step_by(self.step) {
                    // SAFETY: committed read-write memory of ours.
                    unsafe { self.r.at(p).write_volatile(2) };
                    wrote += 1;
                }
            }
            let write_ns = t.elapsed().as_nanos() as f64;
            let piece = self.r.len / self.pieces;
            let t = Instant::now();
            let mut found = 0;
            for i in 0..self.pieces {
                // SAFETY: inside the region.
                let at = unsafe { self.r.base.add(i * piece) };
                let (n, res) = get_write_watch(at, piece, &mut self.buf);
                assert!(res == 0, "GetWriteWatch failed: {}", last());
                found += n;
            }
            let ns = t.elapsed().as_nanos() as f64;
            self.found = found;
            Sample {
                ns,
                aux: if wrote > 0 { write_ns / wrote as f64 } else { 0.0 },
            }
        }
        fn pages(&self) -> usize {
            self.r.pages()
        }
        fn extra(&self, aux: f64) -> String {
            let mut s = format!("found {}", self.found);
            if self.pieces > 1 {
                s += &format!("; time is for all {} calls", self.pieces);
            }
            if self.step > 0 {
                s += &format!("; first write {aux:.0} ns a page");
            }
            s
        }
    }

    fn ww() {
        for &len in &[MIB, 16 * MIB, 256 * MIB, GIB] {
            for &(step, dirty) in &[(0, "none"), (100, "1%"), (1, "all")] {
                both(
                    Line { name: "GetWriteWatch+reset", size: len, dirty },
                    &move |_| Box::new(WriteWatch::new(len, step, 1)),
                );
            }
        }
    }

    fn split() {
        for &len in &[256 * MIB, GIB] {
            for &(step, dirty) in &[(0, "none"), (100, "1%")] {
                for &pieces in &[1, 64] {
                    let name = if pieces == 1 { "split: 1 call" } else { "split: 64 adjacent calls" };
                    both(
                        Line { name, size: len, dirty },
                        &move |_| Box::new(WriteWatch::new(len, step, pieces)),
                    );
                }
            }
        }
    }

    /// GetWriteWatch where it must fail: unwatched private memory or a view.
    struct Fail {
        r: Region,
        buf: Vec<*mut c_void>,
        err: (u32, u32),
    }
    // SAFETY: the buffer holds addresses only.
    unsafe impl Send for Fail {}

    impl Case for Fail {
        fn rep(&mut self) -> Sample {
            let t = Instant::now();
            let (_, r) = get_write_watch(self.r.base, self.r.len, &mut self.buf);
            let ns = t.elapsed().as_nanos() as f64;
            self.err = (r, if r != 0 { last() } else { 0 });
            Sample { ns, aux: 0.0 }
        }
        fn pages(&self) -> usize {
            self.r.pages()
        }
        fn extra(&self, _: f64) -> String {
            format!("returned {} error {}", self.err.0, self.err.1)
        }
    }

    fn fail() {
        for &len in &[2 * MIB, 256 * MIB] {
            both(Line { name: "GetWriteWatch on unwatched private", size: len, dirty: "all" }, &move |_| {
                let r = Region::private(len, false);
                r.touch(1);
                let buf = vec![null_mut(); r.pages()];
                Box::new(Fail { r, buf, err: (0, 0) })
            });
            both(Line { name: "GetWriteWatch on a view", size: len, dirty: "all" }, &move |_| {
                let r = Region::view(len);
                r.touch(1);
                let buf = vec![null_mut(); r.pages()];
                Box::new(Fail { r, buf, err: (0, 0) })
            });
        }
    }

    /// VirtualProtect read-only then read-write over the first `n` pages (two calls a rep).
    struct Prot {
        r: Region,
        n: usize,
    }

    impl Case for Prot {
        fn rep(&mut self) -> Sample {
            let t = Instant::now();
            self.r.protect(0, self.n, PAGE_READONLY);
            self.r.protect(0, self.n, PAGE_READWRITE);
            Sample { ns: t.elapsed().as_nanos() as f64 / 2.0, aux: 0.0 }
        }
        fn pages(&self) -> usize {
            self.n
        }
    }

    fn prot() {
        for &n in &[1usize, 256, 65536] {
            for view in [false, true] {
                let name = if view { "VirtualProtect RW<->RO, view" } else { "VirtualProtect RW<->RO, private" };
                both(Line { name, size: n * PAGE, dirty: "all" }, &move |_| {
                    let r = if view { Region::view(n * PAGE) } else { Region::private(n * PAGE, false) };
                    r.touch(1);
                    Box::new(Prot { r, n })
                });
            }
        }
    }

    // ---------------------------------------------------------------- the guard path

    /// Guarded ranges the handler opens: (start, end) pairs, 0 when free.
    static GUARDED: [(AtomicUsize, AtomicUsize); 16] =
        [const { (AtomicUsize::new(0), AtomicUsize::new(0)) }; 16];
    static FAULTS: AtomicU64 = AtomicU64::new(0);

    unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || r.NumberParameters < 2
            || r.ExceptionInformation[0] != 1
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let at = r.ExceptionInformation[1];
        let ours = GUARDED.iter().any(|(s, e)| {
            let s = s.load(Ordering::Acquire);
            s != 0 && at >= s && at < e.load(Ordering::Acquire)
        });
        if !ours {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let mut old = 0;
        // SAFETY: a page of a region we guard.
        let ok = unsafe { VirtualProtect((at & !(PAGE - 1)) as *const c_void, PAGE, PAGE_READWRITE, &mut old) };
        if ok == 0 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        FAULTS.fetch_add(1, Ordering::Relaxed);
        EXCEPTION_CONTINUE_EXECUTION
    }

    /// Guards a view read-only in one call (untimed), then times single writes to each page,
    /// each taking a fault the handler answers by making the page writable again.
    struct Guard {
        r: Region,
        slot: usize,
    }

    impl Guard {
        fn new(len: usize) -> Guard {
            let r = Region::view(len);
            r.touch(1);
            let start = r.base as usize;
            let slot = GUARDED
                .iter()
                .position(|(s, _)| s.compare_exchange(0, usize::MAX, Ordering::AcqRel, Ordering::Relaxed).is_ok())
                .expect("free guard slot");
            GUARDED[slot].1.store(start + len, Ordering::Release);
            GUARDED[slot].0.store(start, Ordering::Release);
            Guard { r, slot }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            GUARDED[self.slot].1.store(0, Ordering::Release);
            GUARDED[self.slot].0.store(0, Ordering::Release);
        }
    }

    impl Case for Guard {
        fn rep(&mut self) -> Sample {
            let t = Instant::now();
            self.r.protect(0, self.r.pages(), PAGE_READONLY);
            let protect_ns = t.elapsed().as_nanos() as f64;
            let t = Instant::now();
            for p in 0..self.r.pages() {
                // SAFETY: our view; the handler makes the page writable on the fault.
                unsafe { self.r.at(p).write_volatile(3) };
            }
            Sample { ns: t.elapsed().as_nanos() as f64 / self.r.pages() as f64, aux: protect_ns }
        }
        fn pages(&self) -> usize {
            1
        }
        fn extra(&self, aux: f64) -> String {
            format!(
                "per fault; guarding all {} pages in one call took {:.0} us",
                self.r.pages(),
                aux / 1000.0
            )
        }
    }

    fn guard() {
        // SAFETY: the handler only touches our guard table and protections.
        unsafe { AddVectoredExceptionHandler(1, Some(on_write)) };
        for &len in &[256 * KIB, 16 * MIB] {
            both(Line { name: "guard fault (VEH + VirtualProtect RW), view", size: len, dirty: "all" }, &move |_| {
                Box::new(Guard::new(len))
            });
        }
    }

    // ---------------------------------------------------------------- queries, views, commits

    /// VirtualQuery at every page of a region, uniform or with every other page read-only.
    struct Query {
        r: Region,
        regions: usize,
    }

    impl Case for Query {
        fn rep(&mut self) -> Sample {
            // SAFETY: plain data.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            let t = Instant::now();
            let mut regions = 0;
            let mut next = 0usize;
            for p in 0..self.r.pages() {
                let at = self.r.at(p);
                // SAFETY: a valid out pointer.
                let n = unsafe { VirtualQuery(at.cast(), &mut info, size_of_val(&info)) };
                assert!(n != 0);
                if at as usize >= next {
                    regions += 1;
                    next = info.BaseAddress as usize + info.RegionSize;
                }
            }
            let ns = t.elapsed().as_nanos() as f64 / self.r.pages() as f64;
            self.regions = regions;
            Sample { ns, aux: 0.0 }
        }
        fn pages(&self) -> usize {
            1
        }
        fn extra(&self, _: f64) -> String {
            format!("per call, one call a page; {} regions seen", self.regions)
        }
    }

    fn query() {
        let len = 256 * MIB;
        for frag in [false, true] {
            let name = if frag {
                "VirtualQuery page by page, every other page RO"
            } else {
                "VirtualQuery page by page, uniform"
            };
            both(Line { name, size: len, dirty: "-" }, &move |_| {
                let r = Region::private(len, false);
                r.touch(1);
                if frag {
                    for p in (0..r.pages()).step_by(2) {
                        r.protect(p, 1, PAGE_READONLY);
                    }
                }
                Box::new(Query { r, regions: 0 })
            });
        }
    }

    /// MapViewOfFile then UnmapViewOfFile of a whole section, optionally touching every page
    /// in between (untimed). `ns` is the map, `aux` the unmap.
    struct Map {
        section: HANDLE,
        len: usize,
        touch: bool,
    }
    // SAFETY: a handle, usable from any thread.
    unsafe impl Send for Map {}

    impl Drop for Map {
        fn drop(&mut self) {
            // SAFETY: our handle.
            unsafe { CloseHandle(self.section) };
        }
    }

    impl Case for Map {
        fn rep(&mut self) -> Sample {
            let t = Instant::now();
            // SAFETY: a valid section handle.
            let v = unsafe { MapViewOfFile(self.section, FILE_MAP_ALL_ACCESS, 0, 0, self.len) };
            let map_ns = t.elapsed().as_nanos() as f64;
            assert!(!v.Value.is_null(), "MapViewOfFile failed: {}", last());
            if self.touch {
                for p in 0..self.len / PAGE {
                    // SAFETY: inside the view.
                    unsafe { (v.Value as *mut u8).add(p * PAGE).write_volatile(4) };
                }
            }
            let t = Instant::now();
            // SAFETY: the view just mapped.
            unsafe { UnmapViewOfFile(v) };
            Sample { ns: map_ns, aux: t.elapsed().as_nanos() as f64 }
        }
        fn pages(&self) -> usize {
            self.len / PAGE
        }
        fn extra(&self, aux: f64) -> String {
            format!("time is MapViewOfFile; UnmapViewOfFile {:.2} us", aux / 1000.0)
        }
    }

    fn map() {
        for &len in &[2 * MIB, 256 * MIB] {
            for touch in [false, true] {
                let name = if touch {
                    "MapViewOfFile/Unmap, every page touched"
                } else {
                    "MapViewOfFile/Unmap, untouched"
                };
                both(Line { name, size: len, dirty: "-" }, &move |_| {
                    Box::new(Map { section: section(len), len, touch })
                });
            }
        }
    }

    /// VirtualAlloc(MEM_COMMIT) then VirtualFree(MEM_DECOMMIT) of a reserved range, every page
    /// touched in between (untimed). `ns` is the commit, `aux` the decommit.
    struct Commit {
        base: *mut u8,
        len: usize,
    }
    // SAFETY: a reservation only this case uses.
    unsafe impl Send for Commit {}

    impl Drop for Commit {
        fn drop(&mut self) {
            // SAFETY: our reservation.
            unsafe { VirtualFree(self.base.cast(), 0, MEM_RELEASE) };
        }
    }

    impl Case for Commit {
        fn rep(&mut self) -> Sample {
            let t = Instant::now();
            // SAFETY: inside our reservation.
            let p = unsafe { VirtualAlloc(self.base.cast(), self.len, MEM_COMMIT, PAGE_READWRITE) };
            let commit_ns = t.elapsed().as_nanos() as f64;
            assert!(!p.is_null(), "commit failed: {}", last());
            for i in 0..self.len / PAGE {
                // SAFETY: committed just now.
                unsafe { self.base.add(i * PAGE).write_volatile(5) };
            }
            let t = Instant::now();
            // SAFETY: inside our reservation.
            let ok = unsafe { VirtualFree(self.base.cast(), self.len, MEM_DECOMMIT) };
            assert!(ok != 0, "decommit failed: {}", last());
            Sample { ns: commit_ns, aux: t.elapsed().as_nanos() as f64 }
        }
        fn pages(&self) -> usize {
            self.len / PAGE
        }
        fn extra(&self, aux: f64) -> String {
            format!("time is MEM_COMMIT; MEM_DECOMMIT (pages touched) {:.2} us", aux / 1000.0)
        }
    }

    fn commit() {
        let len = 2 * MIB;
        both(Line { name: "VirtualAlloc commit / VirtualFree decommit", size: len, dirty: "-" }, &move |_| {
            // SAFETY: a fresh reservation.
            let base = unsafe { VirtualAlloc(null(), len, MEM_RESERVE, PAGE_READWRITE) } as *mut u8;
            assert!(!base.is_null());
            Box::new(Commit { base, len })
        });
    }

    // ---------------------------------------------------------------- main

    pub fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let all = args.is_empty() || args.iter().any(|a| a == "all");
        let want = |s: &str| all || args.iter().any(|a| a == s);
        let sections: [(&str, fn()); 8] = [
            ("ww", ww),
            ("split", split),
            ("fail", fail),
            ("prot", prot),
            ("guard", guard),
            ("query", query),
            ("map", map),
            ("commit", commit),
        ];
        println!("| case | threads | size | dirtied | reps | median us/call | ns/page | notes |");
        println!("|---|---|---|---|---|---|---|---|");
        let start = Instant::now();
        for (name, f) in sections {
            if want(name) {
                f();
            }
        }
        if FAULTS.load(Ordering::Relaxed) > 0 {
            println!("\nguard faults taken: {}", FAULTS.load(Ordering::Relaxed));
        }
        println!("total {:.1} s", start.elapsed().as_secs_f64());
    }
}
