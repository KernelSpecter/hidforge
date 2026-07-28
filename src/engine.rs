//! The macro engine: one dedicated thread that owns all timing.
//!
//! Design rules, all of them load-bearing:
//!   * The hot loop never allocates and never locks. Telemetry goes out through
//!     atomics only, so the UI can read it at any time without stalling us.
//!   * Deadlines are absolute and re-anchored every iteration, so a preemption
//!     cannot permanently shift the phase.
//!   * Time-critical priority and core pinning are applied only while a job is
//!     running, and dropped the moment it stops. On a weak machine, leaving them
//!     on is worse than not having them.
//!   * When idle the thread blocks on the command channel: exactly 0 % CPU.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use crate::clock::{self, Rng, Waiter, ms_to_ticks, ticks_to_ns};
use crate::inject::{self, Btn};

/// Histogram covers 10 us .. 100 ms across log-spaced buckets. Percentiles are
/// read straight off the cumulative counts, so one array gives median, p95 and
/// p99 for free.
///
/// 512 buckets over 4 decades is ~1.9 % per bucket. 128 was visibly too coarse:
/// a true 10.000 ms period landed in a 9.306 ms bucket and got reported as
/// 107 cps instead of 100. Percentiles are still bucket-quantised, which is why
/// the headline rate is derived from the exact mean instead.
pub const NB: usize = 512;
const LO_NS: f64 = 10_000.0;
const HI_NS: f64 = 100_000_000.0;

#[inline]
fn bucket_of(ns: f64) -> usize {
    if ns <= LO_NS {
        return 0;
    }
    let t = (ns / LO_NS).ln() / (HI_NS / LO_NS).ln();
    ((t * NB as f64) as usize).min(NB - 1)
}

/// Lower edge of a bucket, in nanoseconds -- used for axis labels.
pub fn bucket_ns(i: usize) -> f64 {
    LO_NS * (HI_NS / LO_NS).powf(i as f64 / NB as f64)
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    /// Fire a button repeatedly. The performance showcase.
    AutoClick,
    /// Same loop, but inject a harmless zero-delta move instead of a click, so
    /// the achievable rate can be measured without spraying clicks.
    Benchmark,
    /// Replay a recorded step list.
    Sequence,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub mode: Mode,
    pub btn: Btn,
    pub cps: f64,
    pub hold_ms: f64,
    pub jitter_pct: f64,
    /// Stop automatically after this many events (used by the benchmark) or after
    /// this many passes (for a sequence).
    pub limit: Option<u64>,
    /// Steps for `Mode::Sequence`. Empty otherwise.
    pub steps: Vec<crate::macros::Step>,
    /// Divides recorded delays. 1.0 = replay as recorded.
    pub speed: f64,
}

impl Job {
    pub fn click(btn: Btn, cps: f64, hold_ms: f64) -> Self {
        Self {
            mode: Mode::AutoClick,
            btn,
            cps,
            hold_ms,
            jitter_pct: 0.0,
            limit: None,
            steps: Vec::new(),
            speed: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Opts {
    pub time_critical: bool,
    pub pin_core: bool,
    pub raise_timer_period: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            time_critical: true,
            pin_core: true,
            // Off by default: the high-resolution waitable timer does not need
            // it, and raising it system-wide costs laptop battery.
            raise_timer_period: false,
        }
    }
}

pub enum Cmd {
    Start(Job, Opts),
    Stop,
    Quit,
}

pub struct Tel {
    pub running: AtomicBool,
    pub events: AtomicU64,
    pub hist: [AtomicU32; NB],
    pub min_ns: AtomicU64,
    pub max_ns: AtomicU64,
    pub sum_ns: AtomicU64,
    pub count: AtomicU64,
    /// Percent of wall time the waiter spent spinning, x100.
    pub spin_pct: AtomicU32,
    /// Current learned spin margin in microseconds.
    pub margin_us: AtomicU32,
    pub started: AtomicI64,
}

impl Tel {
    fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            events: AtomicU64::new(0),
            hist: [const { AtomicU32::new(0) }; NB],
            min_ns: AtomicU64::new(u64::MAX),
            max_ns: AtomicU64::new(0),
            sum_ns: AtomicU64::new(0),
            count: AtomicU64::new(0),
            spin_pct: AtomicU32::new(0),
            margin_us: AtomicU32::new(0),
            started: AtomicI64::new(0),
        }
    }

    pub fn reset(&self) {
        self.events.store(0, Ordering::Relaxed);
        for b in &self.hist {
            b.store(0, Ordering::Relaxed);
        }
        self.min_ns.store(u64::MAX, Ordering::Relaxed);
        self.max_ns.store(0, Ordering::Relaxed);
        self.sum_ns.store(0, Ordering::Relaxed);
        self.count.store(0, Ordering::Relaxed);
        self.started.store(clock::qpc(), Ordering::Relaxed);
    }

    #[inline]
    fn record(&self, ns: i64) {
        if ns <= 0 {
            return;
        }
        let u = ns as u64;
        self.hist[bucket_of(ns as f64)].fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(u, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.min_ns.fetch_min(u, Ordering::Relaxed);
        self.max_ns.fetch_max(u, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Snap {
        let mut hist = [0u32; NB];
        let mut total = 0u64;
        for (i, b) in self.hist.iter().enumerate() {
            hist[i] = b.load(Ordering::Relaxed);
            total += hist[i] as u64;
        }
        let pct = |p: f64| -> f64 {
            if total == 0 {
                return 0.0;
            }
            let target = (total as f64 * p) as u64;
            let mut acc = 0u64;
            for (i, &c) in hist.iter().enumerate() {
                acc += c as u64;
                if acc >= target {
                    return bucket_ns(i) / 1e6;
                }
            }
            bucket_ns(NB - 1) / 1e6
        };
        let count = self.count.load(Ordering::Relaxed);
        let min = self.min_ns.load(Ordering::Relaxed);
        Snap {
            hist,
            total,
            events: self.events.load(Ordering::Relaxed),
            min_ms: if min == u64::MAX {
                0.0
            } else {
                min as f64 / 1e6
            },
            max_ms: self.max_ns.load(Ordering::Relaxed) as f64 / 1e6,
            mean_ms: if count == 0 {
                0.0
            } else {
                self.sum_ns.load(Ordering::Relaxed) as f64 / count as f64 / 1e6
            },
            p50_ms: pct(0.50),
            p95_ms: pct(0.95),
            p99_ms: pct(0.99),
            spin_pct: self.spin_pct.load(Ordering::Relaxed) as f32 / 100.0,
            margin_ms: self.margin_us.load(Ordering::Relaxed) as f64 / 1000.0,
            running: self.running.load(Ordering::Relaxed),
        }
    }
}

/// `min_ms` / `p50_ms` are part of the telemetry surface and shown by some views;
/// allowed rather than removed so the snapshot stays a complete picture.
#[allow(dead_code)]
#[derive(Clone)]
pub struct Snap {
    pub hist: [u32; NB],
    pub total: u64,
    pub events: u64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub spin_pct: f32,
    pub margin_ms: f64,
    pub running: bool,
}

impl Snap {
    pub fn cps(&self, p: f64) -> f64 {
        if p <= 0.0 { 0.0 } else { 1000.0 / p }
    }
}

pub struct Engine {
    tx: Sender<Cmd>,
    pub tel: Arc<Tel>,
}

impl Engine {
    pub fn start() -> Self {
        let (tx, rx) = channel();
        let tel = Arc::new(Tel::new());
        let t2 = tel.clone();
        std::thread::Builder::new()
            .name("hidforge-engine".into())
            .spawn(move || worker(rx, t2))
            .expect("spawn engine thread");
        Self { tx, tel }
    }

    pub fn run(&self, job: Job, opts: Opts) {
        self.tel.reset();
        let _ = self.tx.send(Cmd::Start(job, opts));
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
    }
}

/// What the worker should do after a job ends.
enum Next {
    Idle,
    Run(Job, Opts),
    Quit,
}

fn worker(rx: Receiver<Cmd>, tel: Arc<Tel>) {
    let mut waiter = Waiter::new();
    let mut rng = Rng::new();
    let mut pending: Option<(Job, Opts)> = None;

    loop {
        // Idle: block. This is why the app costs nothing when not clicking.
        // A Start that arrived *during* the previous job is carried over here
        // rather than dropped -- otherwise pressing ARM while the benchmark is
        // still running would silently do nothing, having already zeroed the
        // telemetry.
        let (job, opts) = match pending.take() {
            Some(x) => x,
            None => match rx.recv() {
                Ok(Cmd::Start(j, o)) => (j, o),
                Ok(Cmd::Stop) => continue,
                Ok(Cmd::Quit) | Err(_) => return,
            },
        };

        // Privileges go on only for the duration of the job.
        let _period = if opts.raise_timer_period {
            Some(clock::TimerPeriod::acquire())
        } else {
            None
        };
        if opts.pin_core {
            clock::pin_to_core(clock::preferred_core());
        }
        clock::set_time_critical(opts.time_critical);
        tel.running.store(true, Ordering::Relaxed);
        waiter.reset_cost();

        let next = run_job(&job, &rx, &tel, &mut waiter, &mut rng);

        tel.running.store(false, Ordering::Relaxed);
        clock::set_time_critical(false);
        if opts.pin_core {
            clock::unpin(clock::core_count());
        }

        match next {
            Next::Idle => {}
            Next::Run(j, o) => {
                tel.reset();
                pending = Some((j, o));
            }
            Next::Quit => return,
        }
    }
}

/// Replay a recorded step list.
///
/// Delays accumulate onto one absolute timeline rather than being waited
/// individually, so a macro's total length stays honest even if an individual
/// step is late -- the same reasoning as the click loop.
fn run_sequence(job: &Job, rx: &Receiver<Cmd>, tel: &Tel, waiter: &mut Waiter) -> Next {
    use crate::macros::Step;

    if job.steps.is_empty() {
        return Next::Idle;
    }
    let speed = job.speed.clamp(0.05, 100.0);
    // Pick a sane spin margin from the shortest real delay in the macro.
    let shortest = job
        .steps
        .iter()
        .filter_map(|s| match s {
            Step::Delay { ms } => Some(*ms / speed),
            _ => None,
        })
        .fold(f64::INFINITY, f64::min);
    waiter.set_period(ms_to_ticks(if shortest.is_finite() { shortest } else { 8.0 }).max(1));

    let mut passes: u64 = 0;
    let mut clock_base = clock::qpc();
    let mut prev_pass = clock_base;

    loop {
        for step in &job.steps {
            match rx.try_recv() {
                Ok(Cmd::Stop) => return Next::Idle,
                Ok(Cmd::Quit) => return Next::Quit,
                Ok(Cmd::Start(j, o)) => return Next::Run(j, o),
                Err(TryRecvError::Disconnected) => return Next::Quit,
                Err(TryRecvError::Empty) => {}
            }

            match *step {
                Step::Delay { ms } => {
                    clock_base += ms_to_ticks(ms / speed);
                    waiter.wait_until(clock_base);
                }
                Step::Button { btn, down } => {
                    inject::mouse(crate::macros::btn_of(btn), down);
                }
                Step::Key { vk, scan, down } => {
                    // Prefer scancode: more games honour it than a bare VK.
                    if scan != 0 {
                        inject::key_scan(scan, down);
                    } else {
                        inject::key_vk(vk, down);
                    }
                }
                Step::Wheel { delta } => inject::wheel(delta),
                Step::Move { dx, dy } => inject::move_rel(dx, dy),
            }
        }

        passes += 1;
        tel.events.store(passes, Ordering::Relaxed);
        let now = clock::qpc();
        tel.record(ticks_to_ns(now - prev_pass));
        prev_pass = now;
        // Never let the timeline fall behind reality, or a long macro would try to
        // "catch up" as a burst.
        if clock_base < now {
            clock_base = now;
        }

        if let Some(lim) = job.limit
            && passes >= lim
        {
            return Next::Idle;
        }
    }
}

/// Returns when the job is told to stop, hits its limit, or the channel dies.
fn run_job(job: &Job, rx: &Receiver<Cmd>, tel: &Tel, waiter: &mut Waiter, rng: &mut Rng) -> Next {
    if job.mode == Mode::Sequence {
        return run_sequence(job, rx, tel, waiter);
    }

    let cps = job.cps.clamp(0.01, 100_000.0);
    let period = ms_to_ticks(1000.0 / cps).max(1);
    let hold = ms_to_ticks(job.hold_ms.max(0.0));
    let jitter = (job.jitter_pct / 100.0).clamp(0.0, 0.95);
    let dry = job.mode == Mode::Benchmark;
    waiter.set_period(period);

    let mut deadline = clock::qpc() + period;
    let mut prev = clock::qpc();
    let mut n: u64 = 0;
    let mut tick: u32 = 0;

    loop {
        match rx.try_recv() {
            Ok(Cmd::Stop) => return Next::Idle,
            Ok(Cmd::Quit) => return Next::Quit,
            // Do not drop it: hand it back so the worker starts it immediately.
            Ok(Cmd::Start(j, o)) => return Next::Run(j, o),
            Err(TryRecvError::Disconnected) => return Next::Quit,
            Err(TryRecvError::Empty) => {}
        }

        waiter.wait_until(deadline);

        if dry {
            // Same two SendInput calls a click costs, but nothing observable.
            inject::nop();
            inject::nop();
        } else {
            inject::mouse(job.btn, true);
            if hold > 0 {
                waiter.wait_until(clock::qpc() + hold);
            }
            inject::mouse(job.btn, false);
        }

        let now = clock::qpc();
        tel.record(ticks_to_ns(now - prev));
        prev = now;
        n += 1;
        tel.events.store(n, Ordering::Relaxed);

        // Publish waiter cost occasionally -- no point paying for it every click.
        tick = tick.wrapping_add(1);
        if tick.is_multiple_of(64) {
            tel.spin_pct.store(
                (waiter.spin_fraction() * 100.0 * 100.0) as u32,
                Ordering::Relaxed,
            );
            tel.margin_us
                .store((waiter.margin_ms() * 1000.0) as u32, Ordering::Relaxed);
            waiter.reset_cost();
        }

        if let Some(lim) = job.limit
            && n >= lim
        {
            return Next::Idle;
        }

        let mut p = period;
        if jitter > 0.0 {
            p = ((period as f64) * (1.0 + jitter * rng.bipolar())) as i64;
            p = p.max(1);
        }
        deadline += p;
        // Fell behind (a long preemption, or the requested rate is simply not
        // achievable): re-anchor rather than accumulating an ever-growing debt
        // that would then be "repaid" as a burst.
        if deadline < now {
            deadline = now + p;
        }
    }
}
