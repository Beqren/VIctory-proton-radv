//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and
//! the title's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! The default budget is half a second per title frame. A pass the title
//! runs only once and whose pipeline takes longer is lost.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queue_state: AtomicU8,
    made: OnceLock<Option<HostPipeline>>,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw or dispatch has requested it yet.
    key: String,
    used: AtomicBool,
    /// Why it is not made, if it is not ([`left_out`]).
    left_out: Option<&'static str>,
    /// When a worker began it ([`slow`]).
    begun: OnceLock<Instant>,
    /// The worker that was making it ended in the middle ([`watch`]): it has
    /// no host pipeline and never will. Kept apart from `made`, whose
    /// initialisation that worker left running for ever.
    died: AtomicBool,
    /// A dispatch's wait for it, or its drop, has been logged ([`ready`]).
    noted: AtomicBool,
    /// When the title created it: how long before its first use the
    /// record's early start had ([`ready`]).
    created: Instant,
    /// Started early from the record ([`early`]).
    warmed: AtomicBool,
}

/// Why a pipeline with these shaders is not made, if it is not.
fn left_out(shaders: &[Arc<Shader>]) -> Option<&'static str> {
    if without_structures(shaders) {
        Some("shades ray-tracing hits, left out")
    } else if too_dear(shaders) {
        Some("too dear to compile, left out")
    } else {
        None
    }
}

/// Whether some compute shader of `shaders` holds `name`.
fn named(shaders: &[Arc<Shader>], name: &[u8]) -> bool {
    shaders
        .iter()
        .any(|s| s.stage == "compute" && s.container.windows(name.len()).any(|w| w == name))
}

/// Whether a shader is left out for what it costs the host's driver:
/// `CS_ScreenProbes_ActivateRays` of the title
/// (39f0e9aeb156a61c, 66,501 instructions, 9,948 branches). As translated
/// before the clean-up passes (`emit::shader_opt`), AMD's took a core and
/// 0.8 GB more every 10 s for it and did not finish (RX 7800 XT: 47 GB after
/// 8 minutes); cleaned up, it takes 85 s and 1.3 GB, once.
/// `VICTORY_ACTIVATE_RAYS=0` leaves it out.
fn too_dear(shaders: &[Arc<Shader>]) -> bool {
    static OUT: OnceLock<bool> = OnceLock::new();
    let out = *OUT.get_or_init(|| std::env::var_os("VICTORY_ACTIVATE_RAYS").is_some_and(|v| v == "0"));
    out && named(shaders, b"CS_ScreenProbes_ActivateRays")
}

/// Whether a shader only shades what the console's ray-tracing structures
/// were hit by: about 1,000 compute programs of the title, all bound
/// at the world's first frame, each 5 to 9 s of AMD's compiler. They stood in
/// the queue before the passes that make the picture, and with the structure
/// builder off there is no hit to shade.
/// `VICTORY_RT_RESOLVE=1` makes them, as does the builder being on.
fn without_structures(shaders: &[Arc<Shader>]) -> bool {
    static MADE: OnceLock<bool> = OnceLock::new();
    let on = |name| std::env::var_os(name).is_some_and(|v| v == "1");
    let made = *MADE.get_or_init(|| on("VICTORY_RT_RESOLVE") || on("VICTORY_BVH_BUILDER"));
    !made && named(shaders, b"CS_RaytracingResolve")
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    /// Whether it is a compute pipeline.
    fn computes(&self) -> bool {
        matches!(self.shaders.as_slice(), [only] if only.stage == "compute")
    }

    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(graphics.as_ref().map(|g| g.describe()).unwrap_or_default().as_bytes());
        let left_out = left_out(&shaders);
        Pending {
            shaders,
            graphics,
            left_out,
            begun: OnceLock::new(),
            died: AtomicBool::new(false),
            noted: AtomicBool::new(false),
            created: Instant::now(),
            warmed: AtomicBool::new(false),
            queue_state: AtomicU8::new(NOT_QUEUED),
            made: OnceLock::new(),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
        }
    }

    fn make(&self) {
        let begun = Instant::now();
        let _ = self.begun.set(begun);
        let mut translating = Duration::ZERO;
        self.made.get_or_init(|| {
            // The translation first, timed apart from the host's compile
            // (vkd3d-proton's DXIL to SPIR-V and the driver's).
            for s in &self.shaders {
                let at = Instant::now();
                let _ = s.translated();
                translating += at.elapsed();
            }
            HostPipeline::new(&self.shaders, self.graphics.as_deref())
        });
        let host = begun.elapsed().saturating_sub(translating);
        let early = self.warmed.load(Ordering::Relaxed) && !self.used.load(Ordering::Relaxed);
        MADE_TIMES[usize::from(early)].add(translating, host);
        tracing::info!(
            shaders = %shader_set(&self.shaders),
            translate_ms = translating.as_secs_f64() * 1e3,
            host_ms = host.as_secs_f64() * 1e3,
            early,
            "host pipeline made"
        );
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None if p.died.load(Ordering::Relaxed) => Some(format!("its worker ended {:?}", p.shaders)),
        None => Some(p.left_out.map_or_else(
            || format!("being made {:?}", p.shaders),
            str::to_owned,
        )),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Time spent making pipelines: how many, microseconds translating and in
/// the host's compile, the longest of each.
pub(crate) struct MadeTimes([AtomicU64; 5]);

impl MadeTimes {
    const fn new() -> Self {
        MadeTimes([const { AtomicU64::new(0) }; 5])
    }

    fn add(&self, translating: Duration, host: Duration) {
        let (t, h) = (translating.as_micros() as u64, host.as_micros() as u64);
        self.0[0].fetch_add(1, Ordering::Relaxed);
        self.0[1].fetch_add(t, Ordering::Relaxed);
        self.0[2].fetch_add(h, Ordering::Relaxed);
        self.0[3].fetch_max(t, Ordering::Relaxed);
        self.0[4].fetch_max(h, Ordering::Relaxed);
    }

    /// Pipelines; ms translating and in the host; the longest of each (ms).
    pub(crate) fn get(&self) -> (u64, f64, f64, f64, f64) {
        let ms = |i: usize| self.0[i].load(Ordering::Relaxed) as f64 / 1e3;
        (self.0[0].load(Ordering::Relaxed), ms(1), ms(2), ms(3), ms(4))
    }
}

/// `[0]`: pipelines a draw or dispatch asked for before they were made;
/// `[1]`: started early from the record and made before any asked.
pub(crate) static MADE_TIMES: [MadeTimes; 2] = [const { MadeTimes::new() }; 2];

/// Microseconds the title's threads have waited for pipelines since the
/// start ([`wait`], [`wait_long`], [`now`]), in every build: the frame
/// meter takes each frame's share of it.
pub(crate) static WAITED_US: AtomicU64 = AtomicU64::new(0);

/// First uses of title pipelines by how long after the title created them:
/// under 1 s, 1 to 10 s, over 10 s (what an early start from the record had
/// to make them); then how many of all were made already.
pub(crate) static FIRST_USES: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Whether the title has presented a world frame (500 draws or more): until
/// then a frame may wait [`LOADING_FRAME`] for graphics pipelines.
static IN_WORLD: AtomicBool = AtomicBool::new(false);

/// A frame of `draws` draws was presented; whether the world has begun.
pub(crate) fn frame_drew(draws: u64) -> bool {
    if draws >= 500 && !IN_WORLD.swap(true, Ordering::Relaxed) {
        tracing::info!(draws, "a world frame: from here on draws wait for their pipelines within the frame's budget only");
    }
    IN_WORLD.load(Ordering::Relaxed)
}

/// Waiting allowed per title frame, over all threads.
fn wait_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(500);
        tracing::info!(millis, "host pipeline wait budget per frame");
        Duration::from_millis(millis)
    })
}

/// Whether the host's driver has been at `p` for longer than most take
/// (one of the title's takes 85 s): no frame waits for it.
fn slow(p: &Pending) -> bool {
    // A pass a title runs once (it unpacks a model, say) is lost if its
    // pipeline is not there; a draw comes again in the next frame.
    let most = if p.computes() { 10 } else { 3 };
    p.begun.get().is_some_and(|at| at.elapsed() > Duration::from_secs(most))
}

/// What a frame may wait while a world loads: pipelines are asked for by
/// the hundred then, and a pass the title runs once is lost if its
/// pipeline is not there. All such waits of a run together are bounded;
/// the title's GPU watchdog bounds one frame's (30 s).
const LOADING_FRAME: Duration = Duration::from_secs(4);
/// The same for compute pipelines, waited for apart: the passes that unpack
/// a world's models are dispatched once, and in a run whose pipelines were
/// all still to be loaded 130 of them were dropped, and with them walls,
/// floor, ceiling and faces (they never came back).
const LOADING_FRAME_COMPUTE: Duration = Duration::from_secs(12);

/// `VICTORY_PIPELINE_GATE_S`: the bound on all of them (0: none of them).
fn loading_left(spent: Duration) -> Duration {
    static TOTAL: OnceLock<Duration> = OnceLock::new();
    let total = *TOTAL.get_or_init(|| {
        let seconds = std::env::var("VICTORY_PIPELINE_GATE_S")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(120);
        Duration::from_secs(seconds)
    });
    total.saturating_sub(spent)
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    if p.left_out.is_some() {
        return None;
    }
    // A still-pending pipeline was requested by the title too. Prewarm it
    // on the next launch, even if this run never got to use it.
    if !p.used.load(Ordering::Relaxed) && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
        let age = p.created.elapsed();
        let bucket = if age < Duration::from_secs(1) { 0 } else if age < Duration::from_secs(10) { 1 } else { 2 };
        FIRST_USES[bucket].fetch_add(1, Ordering::Relaxed);
        if p.made.get().is_some() {
            FIRST_USES[3].fetch_add(1, Ordering::Relaxed);
        }
    }
    if p.made.get().is_none() {
        if p.died.load(Ordering::Relaxed) {
            return None;
        }
        start(p, REQUESTED);
        let started = Instant::now();
        let waits = p.computes() && compute_waits(p);
        if waits {
            wait_long(p);
        } else {
            wait(p, crate::frame::current());
        }
        // Once per pipeline: a dispatch that had to wait for its pipeline,
        // and one dropped for want of it (a pass the title runs once is
        // then lost for the session).
        if p.computes() && !p.noted.swap(true, Ordering::Relaxed) {
            let hash = p.shaders.first().map_or("", |s| s.hash.as_str());
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            if matches!(p.made.get(), Some(Some(_))) {
                tracing::info!(hash, ms, unbounded = waits, "a dispatch waited for its compute pipeline");
            } else {
                tracing::info!(hash, ms, unbounded = waits, "a dispatch dropped: its compute pipeline is not made");
            }
        }
    }
    p.made.get()?.as_ref()
}

/// The longest a dispatch waits for its pipeline outside the frame's
/// budget ([`compute_waits`]): well inside the title's GPU watchdog (30 s a
/// frame). The ones that take longer (AMD's compiler takes 5 to 9 s for
/// some of the title's programs, one took 85 s) are dropped as before.
const COMPUTE_WAIT_MOST: Duration = Duration::from_secs(20);

/// Whether a dispatch of compute pipeline `p` waits for it outside the
/// frame's budget (`VICTORY_COMPUTE_WAIT`: `0` none, `all`, or shader hash
/// prefixes separated by commas). A dropped draw loses one frame's
/// picture; a dropped dispatch the title issues once loses its output for
/// the session. The head blend-shape mapping (CS_ComputeBlendshapeMappings,
/// 61e54e90adccaf6a) runs once per head at the world's first frame, while
/// hundreds of pipelines are being made: dropped there, its maps stayed at
/// the title's 0xffffffff fill, every vertex of the blend-shape program
/// (970c4fbb3b017c0e) went to one point and the culling kept no meshlet of
/// any head (runs skin-1 to skin-17, blend-1 to blend-3). Run blend-5
/// (only that program waiting) brought the heads back; run blend-6 (all,
/// the default) waited 679 times, 657 ms over the run (longest 19 ms),
/// where before 335 compute programs had their first dispatch dropped
/// (98 of them `scattering_transmittance_system`). The wait is in the
/// recording thread before the dispatch is recorded, so it keeps its
/// place in the list.
fn compute_waits(p: &Pending) -> bool {
    static WHICH: OnceLock<Option<Vec<String>>> = OnceLock::new();
    let which = WHICH.get_or_init(|| {
        match std::env::var("VICTORY_COMPUTE_WAIT").ok().as_deref().map(str::trim) {
            Some("0") => Some(Vec::new()),
            Some("all") | None => None,
            Some(list) => Some(list.split(',').map(|h| h.trim().to_ascii_lowercase()).filter(|h| !h.is_empty()).collect()),
        }
    });
    match which {
        None => true,
        Some(list) => p.shaders.iter().any(|s| list.iter().any(|h| s.hash.starts_with(h.as_str()))),
    }
}

/// Waits for `p` until it is made, its worker ended, or
/// [`COMPUTE_WAIT_MOST`] passed.
/// [`COMPUTE_WAIT_MOST`] is for all such waits of a frame together (review
/// finding: two waits one after the other could pass the title's watchdog).
fn wait_long(p: &Pending) {
    static SPENT: Mutex<(u64, Duration)> = Mutex::new((u64::MAX, Duration::ZERO));
    let frame = crate::frame::current();
    let most = {
        let spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
        if spent.0 == frame { COMPUTE_WAIT_MOST.saturating_sub(spent.1) } else { COMPUTE_WAIT_MOST }
    };
    let started = Instant::now();
    wait_until(p, started, most);
    let mut spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
    if spent.0 != frame {
        *spent = (frame, Duration::ZERO);
    }
    spent.1 += started.elapsed();
}

/// Waits for `p` until it is made, its worker ended, or `most` passed
/// since `started`.
fn wait_until(p: &Pending, started: Instant, most: Duration) {
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() && !p.died.load(Ordering::Relaxed) {
        let rest = most.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done
            .wait_timeout(held, rest.min(Duration::from_millis(250)))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    drop(held);
    WAITED_US.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::PipelineWaitUs, started.elapsed().as_micros() as u64);
}

/// Has `p` made if it is not made yet and waits for it, however long it
/// takes; whether it has a host pipeline.
///
/// A worker makes it (this thread only where there are no workers): a
/// thread that joined a worker in `made`'s initialisation would wait for
/// ever if that worker ended there, and a program that ends the thread
/// making it would end the title's thread here. This way [`watch`] sees the
/// worker gone, marks the pipeline and the wait below ends.
pub(crate) fn now(p: &Arc<Pending>) -> bool {
    if p.left_out.is_some() {
        return false;
    }
    if p.made.get().is_none() && !p.died.load(Ordering::Relaxed) {
        start(p, REQUESTED);
        let started = Instant::now();
        let (lock, done) = &DONE;
        let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
        while p.made.get().is_none() && !p.died.load(Ordering::Relaxed) {
            held = done
                .wait_timeout(held, Duration::from_millis(250))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        WAITED_US.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
    p.made.get().is_some_and(|made| made.is_some())
}

/// Translates the containers of compute programs and makes their host
/// pipelines, for the caches on disk; whether it has one (`None`: left out).
pub(crate) fn warm(shaders: Vec<Arc<Shader>>) -> Option<bool> {
    let p = Pending::new(shaders, None);
    if p.left_out.is_some() {
        return None;
    }
    Some(HostPipeline::new(&p.shaders, None).is_some())
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
///
/// This is the warm-up from the record (`VICTORY_PIPELINE_WARM`, on by
/// default; `0` turns it off). The title creates its pipelines during its
/// own load (some 60,000 in its first 20 s, see the module `pipeline`),
/// before the scene that first draws with them, and a host pipeline
/// depends on nothing but what the creation hands over (the shader
/// containers and the graphics state, `HostPipeline::new`). So the key
/// known at creation finds a pipeline of the record, and the workers make
/// it at the lowest priority, below draws' requests; a draw that asks for
/// it first joins it (the same `Pending`, promoted in the queue).
/// `VICTORY_PIPELINE_WARM_MAX` caps how many are started (default 8192).
pub(crate) fn early(p: &Arc<Pending>) {
    static ON: OnceLock<(bool, u64)> = OnceLock::new();
    let (on, most) = *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_PIPELINE_WARM").is_none_or(|v| v != "0");
        let most = std::env::var("VICTORY_PIPELINE_WARM_MAX").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(8192);
        tracing::info!(on, most, "pipeline warm-up from the record (VICTORY_PIPELINE_WARM, VICTORY_PIPELINE_WARM_MAX)");
        (on, most)
    });
    if on
        && p.left_out.is_none()
        && !p.shaders.is_empty()
        && super::library::wanted_early(&p.key)
        && super::library::EARLY.fetch_add(1, Ordering::Relaxed) < most
    {
        p.warmed.store(true, Ordering::Relaxed);
        start(p, EARLY);
    }
}

/// How the title's recording threads share a frame's waiting budget
/// (`VICTORY_PIPELINE_WAIT_SHARED`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sharing {
    /// `0`: a budget per thread, counted when a wait ends. Each of the
    /// title's five recording threads began its first wait of a frame with
    /// nothing counted yet and waited the whole half second, 2.5 s of
    /// waiting in every frame in which pipelines were being made (runs
    /// 34-49: "2504 ms waiting for pipelines" in each such frame).
    PerThread,
    /// The default (`take`): one budget, and a thread that waits takes all that is left
    /// of it, giving back what it did not use when it is done. Meanwhile
    /// every other thread finds nothing left and drops its draw or
    /// dispatch at once. At the world's first frame that dropped the head
    /// blend-shape mapping pass (CS_ComputeBlendshapeMappings,
    /// 61e54e90adccaf6a), whose pipeline was made 1.5 ms after its first
    /// bind (run blend-3): the title runs it once per head, so the maps
    /// stayed at the title's 0xffffffff fill, every vertex of
    /// 970c4fbb3b017c0e read source vertex 0xffff (out of its buffer:
    /// zeros) and was put at one point, and the culling kept no meshlet of
    /// any head (runs skin-1 to skin-17, blend-1 to blend-3).
    Take,
    /// `overlap`: one budget of wall time for all threads. Threads that
    /// wait at the same time spend it together (the frame waits as long as
    /// the longest of them), so the frame waits at most the budget and no
    /// thread finds it taken by another's wait. Not the default yet: its
    /// one title run (blend-4) ran the three mapping passes at the world's
    /// first frame (frame 871), then the card faulted (gfxhub page fault,
    /// ring gfx timeout after a 2 s submission of compute and mesh lists)
    /// before a world frame was shown; whether a pass that now runs caused
    /// it is not known.
    Overlap,
}

fn sharing() -> Sharing {
    static MODE: OnceLock<Sharing> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_PIPELINE_WAIT_SHARED").ok().as_deref().map(str::trim) {
        Some("0") => Sharing::PerThread,
        Some("overlap") => Sharing::Overlap,
        _ => Sharing::Take,
    })
}

/// What a frame has waited for pipelines ([`wait`]).
#[derive(Clone, Copy, Debug)]
struct FrameBudget {
    frame: u64,
    /// Waited in the frame so far (with [`Sharing::Take`]: also what the
    /// threads waiting now have taken).
    spent: Duration,
    /// [`Sharing::Overlap`]: the wait going on now, since when and how many
    /// threads are in it.
    open: Option<(Instant, u32)>,
}

impl FrameBudget {
    const fn new() -> Self {
        FrameBudget { frame: u64::MAX, spent: Duration::ZERO, open: None }
    }

    /// A thread of frame `frame` begins to wait at `now`: how long it may.
    fn begin(&mut self, mode: Sharing, frame: u64, budget: Duration, now: Instant) -> Duration {
        if self.frame != frame {
            *self = FrameBudget { frame, ..FrameBudget::new() };
        }
        match mode {
            Sharing::PerThread => budget.saturating_sub(self.spent),
            Sharing::Take => {
                let left = budget.saturating_sub(self.spent);
                self.spent += left;
                left
            }
            Sharing::Overlap => {
                let going = self.open.map_or(Duration::ZERO, |(since, _)| now.saturating_duration_since(since));
                let left = budget.saturating_sub(self.spent + going);
                if !left.is_zero() {
                    self.open = Some(match self.open {
                        Some((since, n)) => (since, n + 1),
                        None => (now, 1),
                    });
                }
                left
            }
        }
    }

    /// The thread that [`Self::begin`] gave `left` stops waiting at `now`,
    /// having waited `waited`. A frame that began meanwhile has its own
    /// budget.
    fn end(&mut self, mode: Sharing, frame: u64, left: Duration, waited: Duration, now: Instant) {
        if self.frame != frame || left.is_zero() {
            return;
        }
        match mode {
            Sharing::PerThread => self.spent += waited,
            Sharing::Take => self.spent = self.spent.saturating_sub(left.saturating_sub(waited)),
            Sharing::Overlap => {
                if let Some((since, n)) = self.open {
                    if n <= 1 {
                        self.spent += now.saturating_duration_since(since);
                        self.open = None;
                    } else {
                        self.open = Some((since, n - 1));
                    }
                }
            }
        }
    }
}

/// Waits for `p` while frame `frame` has budget left.
fn wait(p: &Pending, frame: u64) {
    let budget = wait_budget();
    if budget.is_zero() || slow(p) {
        return;
    }
    // What was waited past the budget since the start, in milliseconds; the
    // second of each is the compute pipelines'.
    static LOADINGS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
    let compute = p.computes();
    let past_total = &LOADINGS[usize::from(compute)];
    let loading = loading_left(Duration::from_millis(past_total.load(Ordering::Relaxed)));
    // A loading frame's longer wait is for graphics pipelines only until
    // the world's first frame. In the world it let every frame that met new
    // pipelines wait up to 4 s, one pipeline after the other: a recording
    // thread queues a pipeline only when it records its draw, then waits
    // for it, so the workers made them one at a time. In the owner's
    // session (loss-1808) a scene cut brought about 50 mesh pipelines in
    // frames 1400-1401, each made in 20 to 300 ms, and the frame took
    // 3,557 ms. The run's total bound never closed: it counts only each
    // wait's part past the half second. `VICTORY_PIPELINE_WORLD_WAIT=1`:
    // in the world too, as before.
    // shortcut: a pass the title draws once at a scene cut can now be lost
    // (a draw is otherwise there again in the next frame).
    static WORLD_WAIT: OnceLock<bool> = OnceLock::new();
    let world_wait =
        *WORLD_WAIT.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_WORLD_WAIT").is_some_and(|v| v == "1"));
    let loading_frame = if compute {
        LOADING_FRAME_COMPUTE
    } else if world_wait || !IN_WORLD.load(Ordering::Relaxed) {
        LOADING_FRAME
    } else {
        Duration::ZERO
    };
    let budget = budget.max(loading_frame.min(loading));
    // The frame the budget belongs to, and what was waited of it (or is
    // taken by a thread waiting now).
    static SPENTS: [Mutex<FrameBudget>; 2] = [const { Mutex::new(FrameBudget::new()) }; 2];
    let frame_spent = &SPENTS[usize::from(compute)];
    let mode = sharing();
    let started = Instant::now();
    let left = frame_spent.lock().unwrap_or_else(|e| e.into_inner()).begin(mode, frame, budget, started);
    if left.is_zero() {
        return;
    }
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() && !slow(p) && !p.died.load(Ordering::Relaxed) {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        // Woken now and then to see whether it has become a slow one.
        let rest = rest.min(Duration::from_millis(250));
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let waited = started.elapsed().min(left);
    let past = waited.saturating_sub(wait_budget());
    past_total.fetch_add(past.as_millis() as u64, Ordering::Relaxed);
    frame_spent
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .end(mode, frame, left, waited, Instant::now());
    WAITED_US.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::PipelineWaitUs,
        started.elapsed().as_micros() as u64,
    );
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const MAKING: u8 = 3;

struct Queue {
    early: VecDeque<Arc<Pending>>,
    requested: VecDeque<Arc<Pending>>,
}

impl Queue {
    const fn new() -> Self {
        Self {
            early: VecDeque::new(),
            requested: VecDeque::new(),
        }
    }

    /// Queues an unrequested pipeline or promotes one queued at creation.
    /// Returns whether it is new to the queue, or `None` if nothing changed.
    fn push(&mut self, p: &Arc<Pending>, priority: u8) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        if priority == REQUESTED {
            self.requested.push_back(p.clone());
        } else {
            self.early.push_back(p.clone());
        }
        Some(prev == NOT_QUEUED)
    }

    fn pop(&mut self) -> Option<Arc<Pending>> {
        let p = self.requested.pop_front().or_else(|| {
            // An early entry can have been promoted and claimed already.
            std::iter::from_fn(|| self.early.pop_front())
                .find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)
        })?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some(p)
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static AVAILABLE: Condvar = Condvar::new();

/// Start or promote work for a draw without waiting behind the entire early
/// list. Queue changes and worker claims happen under the same lock.
fn start(p: &Arc<Pending>, priority: u8) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    if ends_workers(&p.shaders) {
        p.died.store(true, Ordering::Relaxed);
        return;
    }
    if !workers() {
        if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
            p.make();
        }
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(new) = queue.push(p, priority) {
        if new {
            PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        AVAILABLE.notify_one();
    }
}

fn next() -> Arc<Pending> {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(p) = queue.pop() {
            return p;
        }
        queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

/// What a worker is making now, for [`watch`].
type Slot = Arc<Mutex<Option<Arc<Pending>>>>;

/// The shaders, as [`shader_set`] names them, of pipelines that ended the
/// worker making them: a pipeline of the same shaders (in another state,
/// say) is not tried again in this run. By the whole set, not by each
/// shader: a mesh shader that one pixel shader's pipeline died with is in
/// other pipelines that are fine.
static ENDED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The stages and hashes of a pipeline's shaders, in their order.
fn shader_set(shaders: &[Arc<Shader>]) -> String {
    let named: Vec<String> = shaders.iter().map(|s| format!("{} {}", s.stage, s.hash)).collect();
    named.join(", ")
}

/// Whether a pipeline of these shaders ended a worker before.
fn ends_workers(shaders: &[Arc<Shader>]) -> bool {
    let ended = ENDED.lock().unwrap_or_else(|e| e.into_inner());
    !ended.is_empty() && ended.contains(&shader_set(shaders))
}

/// Gives `p` up: it has no host pipeline and those waiting for it go on.
fn give_up(p: &Pending) {
    p.died.store(true, Ordering::Relaxed);
    let (lock, done) = &DONE;
    let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
    done.notify_all();
}

fn worker(i: usize, slot: Slot) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("victory-pipelines-{i}"))
        .spawn(move || {
            // Below the title's own threads (loading, recording).
            // SAFETY: a plain call on our own thread.
            unsafe {
                use windows::Win32::System::Threading::*;
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
            }
            loop {
                let p = next();
                // (Queued, it may be, before its shaders were known to end
                // the worker that makes them.)
                if ends_workers(&p.shaders) {
                    give_up(&p);
                } else {
                    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(p.clone());
                    p.make();
                    *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
                if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                    drained();
                } else {
                    super::library::save(false);
                }
            }
        })
}

/// Whether the thread has ended, by its handle: a thread ended from outside
/// its own code never reports through the standard library.
fn ended(thread: &std::thread::JoinHandle<()>) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::WaitForSingleObject;
    // SAFETY: the handle is the join handle's own, open while it lives.
    unsafe { WaitForSingleObject(HANDLE(thread.as_raw_handle().cast()), 0) == WAIT_OBJECT_0 }
}

/// Looks for workers that ended while making a pipeline, gives the pipeline
/// up and starts a worker in the place of each.
///
/// A worker never ends by itself, and on Windows a fault in the host's
/// pipeline creation ends the process. Under Wine a stack that overflows
/// inside the host call ends only the thread, without a word: vkd3d-proton's
/// DXIL converter recurses without end on three of the title's compute
/// programs (559d9f0184e15fe3, a1b20bc47e3c9597, e8c962358d862e3f; Proton
/// Experimental of 2026-11-12 and GE-Proton11-14; a 64 MiB stack overflowed
/// as the 2 MiB one did). Their pipelines then stayed "being made" for the
/// rest of the run with a worker lost for each.
fn watch(threads: Vec<(std::thread::JoinHandle<()>, Slot)>) {
    // (`None`: no worker could be started in the place of one that ended.)
    let mut threads: Vec<(Option<std::thread::JoinHandle<()>>, Slot)> =
        threads.into_iter().map(|(thread, slot)| (Some(thread), slot)).collect();
    loop {
        std::thread::sleep(Duration::from_millis(250));
        for (i, (thread, slot)) in threads.iter_mut().enumerate() {
            if !thread.as_ref().is_some_and(ended) {
                continue;
            }
            let making = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(p) = making {
                let shaders = shader_set(&p.shaders);
                tracing::warn!(
                    shaders,
                    worker = i,
                    "a pipeline worker ended while making a host pipeline (under Wine: a stack overflow in the host's shader converter); the pipeline is given up and its draws or dispatches are dropped"
                );
                {
                    let mut ended = ENDED.lock().unwrap_or_else(|e| e.into_inner());
                    if !ended.contains(&shaders) {
                        ended.push(shaders);
                    }
                }
                give_up(&p);
                if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                    drained();
                }
            } else {
                tracing::warn!(worker = i, "a pipeline worker ended");
            }
            *thread = match worker(i, slot.clone()) {
                Ok(new) => Some(new),
                Err(e) => {
                    tracing::warn!(error = %e, worker = i, "no pipeline worker in its place: one fewer from here on");
                    None
                }
            };
        }
    }
}

/// Starts the workers (half the logical processors, 2 to 8) on first use.
fn workers() -> bool {
    static RUNNING: OnceLock<bool> = OnceLock::new();
    *RUNNING.get_or_init(|| {
        let mut threads = Vec::new();
        let workers = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8));
        for i in 0..workers {
            let slot = Slot::default();
            match worker(i, slot.clone()) {
                Ok(thread) => threads.push((thread, slot)),
                Err(e) => tracing::warn!(error = %e, "no pipeline worker"),
            }
        }
        let running = !threads.is_empty();
        if running {
            let watching = std::thread::Builder::new()
                .name("victory-pipelines-watch".into())
                .spawn(move || watch(threads));
            if let Err(e) = watching {
                tracing::warn!(error = %e, "pipeline workers are not watched");
            }
        }
        running
    })
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        let (asked, asked_tr, asked_host, _, _) = MADE_TIMES[0].get();
        let (early, early_tr, early_host, _, _) = MADE_TIMES[1].get();
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            made_early_before_asked = early,
            early_translate_ms = early_tr,
            early_host_ms = early_host,
            made_when_asked = asked,
            asked_translate_ms = asked_tr,
            asked_host_ms = asked_host,
            card_mb_in_use = crate::host::host().and_then(|h| h.video_memory()).map_or(0, |m| m[1] >> 20),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn pending_pipeline_is_recorded_for_the_next_launch() {
        let mut p = Pending::new(Vec::new(), None);
        p.key = "pending-pipeline-test".to_owned();
        // Simulate a worker that has taken the job but cannot finish it yet.
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);

        assert!(!super::super::library::wanted_early(&p.key));
        assert!(ready(&p).is_none());
        assert!(p.made.get().is_none());
        assert!(super::super::library::wanted_early(&p.key));
    }

    /// Two recording threads wait in one frame: the second is not left
    /// without budget while the first waits (that dropped the head
    /// blend-shape mapping pass at the world's first frame), and the frame
    /// as a whole waits no longer than the budget.
    #[test]
    fn a_second_thread_waits_while_the_first_does() {
        let ms = Duration::from_millis;
        let budget = ms(500);
        let t0 = Instant::now();

        let o = Sharing::Overlap;
        let mut b = FrameBudget::new();
        let first = b.begin(o, 7, budget, t0);
        assert_eq!(first, budget);
        let second = b.begin(o, 7, budget, t0 + ms(100));
        assert_eq!(second, ms(400), "the second thread waits until the first's deadline");
        b.end(o, 7, first, ms(300), t0 + ms(300));
        b.end(o, 7, second, ms(400), t0 + ms(500));
        assert!(b.begin(o, 7, budget, t0 + ms(600)).is_zero(), "the frame waited its budget");
        // A new frame has its own.
        assert_eq!(b.begin(o, 8, budget, t0 + ms(600)), budget);

        // Waits one after the other add up.
        let mut b = FrameBudget::new();
        let first = b.begin(Sharing::Overlap, 1, budget, t0);
        b.end(Sharing::Overlap, 1, first, ms(200), t0 + ms(200));
        assert_eq!(b.begin(Sharing::Overlap, 1, budget, t0 + ms(1000)), ms(300));

        // The default (`take`): the second thread finds nothing left.
        let mut b = FrameBudget::new();
        let first = b.begin(Sharing::Take, 7, budget, t0);
        assert!(b.begin(Sharing::Take, 7, budget, t0 + ms(100)).is_zero());
        b.end(Sharing::Take, 7, first, ms(300), t0 + ms(300));
        assert_eq!(b.begin(Sharing::Take, 7, budget, t0 + ms(300)), ms(200));
    }

    /// A pipeline of the record is started when the title creates it, and
    /// a draw that asks for it later joins that start: it is made once.
    #[test]
    fn a_draw_joins_a_pipeline_started_from_the_record() {
        let shader = super::super::shader("compute", b"a container of the join test, which does not translate");
        let mut p = Pending::new(vec![shader], None);
        p.key = "join-test-key".to_owned();
        let p = Arc::new(p);
        super::super::library::note_used(&p.key);
        early(&p);
        assert!(p.warmed.load(Ordering::Relaxed), "started from the record");
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        let begun = *p.begun.get().expect("a worker made it");
        assert!(ready(&p).is_none(), "no host pipeline for a shader that does not translate");
        assert_eq!(p.queue_state.load(Ordering::Relaxed), MAKING, "not queued again");
        assert_eq!(*p.begun.get().unwrap(), begun, "made once");
        // One not in the record is not started at creation.
        let other = Arc::new(Pending::new(vec![super::super::shader("compute", b"another one")], None));
        early(&other);
        assert_eq!(other.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
    }

    #[test]
    fn draws_promote_early_pipelines_without_claiming_them_twice() {
        let earlier = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&earlier, EARLY), Some(true));
        assert_eq!(queue.push(&promoted, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        assert_eq!(queue.push(&promoted, REQUESTED), None);

        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &requested));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &earlier));
        assert!(queue.pop().is_none());
    }
}
