//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and Grand
//! Theft Auto VI's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! Passes over the screen (no vertex input, no depth) have workers and a
//! longer budget of their own: the title fills its lighting tables with
//! such draws once, at the world's first frame, and behind a world's worth
//! of pipelines they waited minutes; a run that lost them had no diffuse
//! light at all.
//!
//! shortcut: the budgets (half a second of waiting per title frame, 20 s
//! for screen passes) are guesses; another pass the title runs only once
//! and whose pipeline takes longer is lost.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queued: AtomicBool,
    /// Promoted to urgent (a live draw/dispatch is waiting for it).
    urgent: AtomicBool,
    /// Claimed by a worker (so duplicate queue entries are skipped once).
    claimed: AtomicBool,
    made: OnceLock<Option<HostPipeline>>,
    /// Its worker was lost to it: it will not be made.
    lost: AtomicBool,
    /// A pass over the screen (see [`GraphicsState::screen_pass`]).
    screen: bool,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw has used it yet.
    key: String,
    used: AtomicBool,
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(graphics.as_ref().map(|g| g.describe()).unwrap_or_default().as_bytes());
        Pending {
            screen: graphics.as_ref().is_some_and(|g| g.screen_pass()),
            shaders,
            graphics,
            queued: AtomicBool::new(false),
            urgent: AtomicBool::new(false),
            claimed: AtomicBool::new(false),
            made: OnceLock::new(),
            lost: AtomicBool::new(false),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
        }
    }

    fn make(&self) {
        // Not `get_or_init`: a worker lost in it would leave it locked.
        let made = HostPipeline::new(&self.shaders, self.graphics.as_deref());
        let _ = self.made.set(made);
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
        None => Some("being made".into()),
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

/// Waiting allowed per title frame, over all threads, unless overridden by
/// `VICTORY_PIPELINE_WAIT_MS`.
fn wait_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(150);
        tracing::info!(millis, "host pipeline wait budget per frame");
        Duration::from_millis(millis)
    })
}

/// The same for passes over the screen (below the title's 30 s watchdog).
const SCREEN_BUDGET: Duration = Duration::from_secs(20);

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it as urgent on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    // A still-pending pipeline was requested by the title too. Prewarm it
    // on the next launch, even if this run never got to finish it.
    if !p.used.load(Ordering::Relaxed) && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    if p.made.get().is_none() {
        enqueue(p, true);
        if p.lost.load(Ordering::Relaxed) {
            return None;
        }
        // Also with a world's worth queued: passes the title runs only once
        // set up what later ones loop over.
        wait(p, crate::frame::current());
    }
    p.made.get()?.as_ref()
}

/// Queues `p` for the workers once, or promotes a queued background item to
/// the urgent queue when a live draw/dispatch begins waiting on it.
fn enqueue(p: &Arc<Pending>, urgent: bool) {
    let work = work_for(p.screen);
    if !work.has_workers.load(Ordering::Relaxed) {
        if !p.queued.swap(true, Ordering::AcqRel) && !p.claimed.swap(true, Ordering::AcqRel) {
            PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
            p.make();
            if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                drained();
            }
        }
        return;
    }
    let outcome = {
        let mut q = work.queue.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = q.enqueue(p, urgent);
        // Increment WAITING before unlocking so a worker cannot finish `p`
        // and underflow `WAITING` before this thread adds 1.
        if outcome == Enqueue::New {
            PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        outcome
    };
    match outcome {
        Enqueue::New => {
            if urgent {
                work.cv_urgent.notify_all();
            }
            work.cv_any.notify_one();
        }
        Enqueue::Promoted => {
            work.cv_urgent.notify_all();
            work.cv_any.notify_one();
        }
        Enqueue::Ignored => {}
    }
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
pub(crate) fn early(p: &Arc<Pending>) {
    if !p.shaders.is_empty() && super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        enqueue(p, false);
    }
}

/// Whether the frame's wait budget is shared across recording threads so
/// parallel threads do not wait the full budget each (`VICTORY_PIPELINE_WAIT_SHARED=0`
/// restores per-thread budgets).
fn shared_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_WAIT_SHARED").is_none_or(|v| v != "0"))
}

/// Waits for `p` while frame `frame` has budget of its kind left.
fn wait(p: &Pending, frame: u64) {
    static SPENT: [Mutex<(u64, Duration)>; 2] =
        [const { Mutex::new((u64::MAX, Duration::ZERO)) }; 2];
    let kind = usize::from(p.screen);
    let budget = if p.screen { SCREEN_BUDGET } else { wait_budget() };
    if budget.is_zero() {
        return;
    }
    let shared = shared_budget();
    let left = {
        let mut spent = SPENT[kind].lock().unwrap_or_else(|e| e.into_inner());
        if spent.0 != frame {
            *spent = (frame, Duration::ZERO);
        }
        let left = budget.saturating_sub(spent.1);
        if shared {
            spent.1 += left;
        }
        left
    };
    if left.is_zero() {
        return;
    }
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let waited = started.elapsed().min(left);
    let mut spent = SPENT[kind].lock().unwrap_or_else(|e| e.into_inner());
    if spent.0 == frame {
        if shared {
            spent.1 = spent.1.saturating_sub(left - waited);
        } else {
            spent.1 += waited;
        }
    }
}

#[derive(Clone, Copy)]
struct Role {
    screen: bool,
    urgent_only: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Enqueue {
    New,
    Promoted,
    Ignored,
}

/// Priority queues per pipeline kind: live graphics (`urgent_graphics`) run
/// ahead of live compute (`urgent_compute`) so heavy compute bursts do not
/// starve world geometry, and both preempt speculative `background` items.
struct WorkQueue {
    urgent_graphics: VecDeque<Arc<Pending>>,
    urgent_compute: VecDeque<Arc<Pending>>,
    background: VecDeque<Arc<Pending>>,
}

impl WorkQueue {
    const fn new() -> Self {
        Self {
            urgent_graphics: VecDeque::new(),
            urgent_compute: VecDeque::new(),
            background: VecDeque::new(),
        }
    }

    fn push_urgent(&mut self, p: &Arc<Pending>) {
        if p.graphics.is_some() {
            self.urgent_graphics.push_back(p.clone());
        } else {
            self.urgent_compute.push_back(p.clone());
        }
    }

    fn enqueue(&mut self, p: &Arc<Pending>, urgent: bool) -> Enqueue {
        if p.claimed.load(Ordering::Acquire) || p.made.get().is_some() {
            return Enqueue::Ignored;
        }
        if !p.queued.swap(true, Ordering::AcqRel) {
            p.urgent.store(urgent, Ordering::Relaxed);
            if urgent {
                self.push_urgent(p);
            } else {
                self.background.push_back(p.clone());
            }
            Enqueue::New
        } else if urgent && !p.urgent.swap(true, Ordering::Relaxed) {
            self.push_urgent(p);
            Enqueue::Promoted
        } else {
            Enqueue::Ignored
        }
    }

    fn pop(&mut self, urgent_only: bool) -> Option<Arc<Pending>> {
        while let Some(p) = self.urgent_graphics.pop_front() {
            if !p.claimed.swap(true, Ordering::AcqRel) {
                return Some(p);
            }
        }
        while let Some(p) = self.urgent_compute.pop_front() {
            if !p.claimed.swap(true, Ordering::AcqRel) {
                return Some(p);
            }
        }
        if urgent_only {
            return None;
        }
        while let Some(p) = self.background.pop_front() {
            if !p.claimed.swap(true, Ordering::AcqRel) {
                return Some(p);
            }
        }
        None
    }
}

struct Work {
    queue: Mutex<WorkQueue>,
    cv_urgent: Condvar,
    cv_any: Condvar,
    has_workers: AtomicBool,
}

struct ActiveWorker {
    id: u32,
    pending: Arc<Pending>,
    role: Role,
}

/// Queue and condvars by pipeline kind: `[0]` ordinary, `[1]` screen passes.
static WORK: [OnceLock<Work>; 2] = [const { OnceLock::new() }; 2];
static WORKERS_READY: [std::sync::Once; 2] = [std::sync::Once::new(), std::sync::Once::new()];

/// What the workers (by thread) are making.
static AT_WORK: Mutex<Vec<ActiveWorker>> = Mutex::new(Vec::new());

fn urgent_worker_count(screen: bool, total: usize) -> usize {
    if screen {
        usize::from(total >= 2)
    } else if total >= 6 {
        total / 3
    } else if total >= 3 {
        2
    } else {
        1
    }
}

/// Returns the queue for `screen`, spawning its worker pool on first use.
fn work_for(screen: bool) -> &'static Work {
    let idx = usize::from(screen);
    let work = WORK[idx].get_or_init(|| Work {
        queue: Mutex::new(WorkQueue::new()),
        cv_urgent: Condvar::new(),
        cv_any: Condvar::new(),
        has_workers: AtomicBool::new(false),
    });
    WORKERS_READY[idx].call_once(|| {
        let total = if screen {
            2
        } else {
            std::thread::available_parallelism().map_or(4, |n| n.get().clamp(4, 12))
        };
        let urgent_count = urgent_worker_count(screen, total);
        let mut any = false;
        for i in 0..total {
            let role = Role { screen, urgent_only: i < urgent_count };
            any |= worker(i + if screen { 50 } else { 0 }, role);
        }
        if any {
            work.has_workers.store(true, Ordering::Release);
        }
    });
    work
}

/// Starts worker `i` with `role`, returning whether the thread spawned.
fn worker(i: usize, role: Role) -> bool {
    let spawned = std::thread::Builder::new().name(format!("victory-pipelines-{i}")).spawn(move || {
        // Urgent-only workers stay at normal priority so live draws do not wait
        // behind background early prewarming; general workers yield slightly.
        if !role.urgent_only {
            // SAFETY: a plain call on our own thread.
            unsafe {
                use windows::Win32::System::Threading::*;
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
            }
        }
        let Some(work) = WORK[usize::from(role.screen)].get() else { return };
        loop {
            let p = {
                let mut q = work.queue.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(p) = q.pop(role.urgent_only) {
                        break p;
                    }
                    let cv = if role.urgent_only { &work.cv_urgent } else { &work.cv_any };
                    q = cv.wait(q).unwrap_or_else(|e| e.into_inner());
                }
            };
            // SAFETY: a plain call.
            let id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            AT_WORK.lock().unwrap_or_else(|e| e.into_inner()).push(ActiveWorker {
                id,
                pending: p.clone(),
                role,
            });
            p.make();
            AT_WORK.lock().unwrap_or_else(|e| e.into_inner()).retain(|w| w.id != id);
            if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                drained();
            } else if !role.urgent_only {
                let urgent_waiting = work
                    .queue
                    .try_lock()
                    .is_ok_and(|q| !q.urgent_graphics.is_empty() || !q.urgent_compute.is_empty());
                if !urgent_waiting {
                    super::library::save_periodic();
                }
            }
        }
    });
    match spawned {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(error = %e, "no pipeline worker");
            false
        }
    }
}

/// A worker is lost to a compile that crashed or never ends: another takes
/// its place (its pipeline stays unmade). With all of them lost, the rest of
/// a world's pipelines waited forever.
pub(super) fn worker_lost(id: u32) {
    static LOST: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(100);
    let mut lost = None;
    AT_WORK.lock().unwrap_or_else(|e| e.into_inner()).retain(|w| {
        if w.id == id {
            w.pending.lost.store(true, Ordering::Relaxed);
            lost = Some(w.role);
        }
        w.id != id
    });
    if let Some(role) = lost {
        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
            drained();
        }
        let _ = worker(LOST.fetch_add(1, Ordering::Relaxed), role);
    }
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
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
        assert!(p.queued.load(Ordering::Relaxed));
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn urgent_preempts_background_and_promotion_claims_once() {
        let mut q = WorkQueue::new();
        let bg_only = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let urgent = Arc::new(Pending::new(Vec::new(), None));

        assert!(q.enqueue(&bg_only, false) == Enqueue::New);
        assert!(q.enqueue(&promoted, false) == Enqueue::New);
        assert!(q.enqueue(&urgent, true) == Enqueue::New);
        // Promoting an already queued background item pushes it onto urgent.
        assert!(q.enqueue(&promoted, true) == Enqueue::Promoted);
        // Repeat calls do not duplicate entries.
        assert!(q.enqueue(&promoted, true) == Enqueue::Ignored);

        // Urgent-only worker drains urgent items in FIFO order and ignores background.
        let first = q.pop(true).expect("urgent item");
        assert!(Arc::ptr_eq(&first, &urgent));
        let second = q.pop(true).expect("promoted item");
        assert!(Arc::ptr_eq(&second, &promoted));
        assert!(q.pop(true).is_none());

        // General worker picks urgent first, then remaining un-claimed background items,
        // skipping the stale duplicate entry of `promoted`.
        let third = q.pop(false).expect("remaining background item");
        assert!(Arc::ptr_eq(&third, &bg_only));
        assert!(q.pop(false).is_none());
    }

    #[test]
    fn urgent_graphics_preempts_urgent_compute() {
        let mut q = WorkQueue::new();
        let bg = Arc::new(Pending::new(Vec::new(), None));
        let promoted_gfx = Arc::new(Pending::new(Vec::new(), Some(Box::default())));
        let compute_1 = Arc::new(Pending::new(Vec::new(), None));
        let compute_2 = Arc::new(Pending::new(Vec::new(), None));
        let gfx = Arc::new(Pending::new(Vec::new(), Some(Box::default())));

        assert!(q.enqueue(&bg, false) == Enqueue::New);
        assert!(q.enqueue(&promoted_gfx, false) == Enqueue::New);
        assert!(q.enqueue(&compute_1, true) == Enqueue::New);
        assert!(q.enqueue(&compute_2, true) == Enqueue::New);
        assert!(q.enqueue(&gfx, true) == Enqueue::New);
        assert!(q.enqueue(&promoted_gfx, true) == Enqueue::Promoted);

        assert!(Arc::ptr_eq(&q.pop(true).expect("graphics first"), &gfx));
        assert!(Arc::ptr_eq(
            &q.pop(true).expect("promoted graphics ahead of compute"),
            &promoted_gfx
        ));
        assert!(Arc::ptr_eq(&q.pop(true).expect("first compute"), &compute_1));
        assert!(Arc::ptr_eq(&q.pop(true).expect("second compute"), &compute_2));
        assert!(q.pop(true).is_none());
        assert!(Arc::ptr_eq(&q.pop(false).expect("background last"), &bg));
        assert!(q.pop(false).is_none());
    }

    #[test]
    fn pending_request_marks_pipeline_used_even_when_unmade() {
        let mut inner = Pending::new(Vec::new(), None);
        inner.key = format!("test-unmade-prewarm-{}", std::process::id());
        inner.claimed.store(true, Ordering::Relaxed);
        inner.lost.store(true, Ordering::Relaxed);
        let p = Arc::new(inner);

        assert!(ready(&p).is_none());
        assert!(p.made.get().is_none());
        assert!(p.used.load(Ordering::Relaxed));
        assert!(super::super::library::wanted_early(&p.key));
    }
}
