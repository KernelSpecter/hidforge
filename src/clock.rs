//! Timing primitives.
//!
//! Everything here exists to answer one question: how do you hold a period of
//! a fraction of a millisecond without burning a whole CPU core?
//!
//! Measured on this machine (see `spike/bench_timing.py`):
//!   * default `Sleep` granularity          15.6 ms   -- useless
//!   * `timeBeginPeriod(1)` + `Sleep(1)`     1.54 ms   -- still too coarse
//!   * high-resolution waitable timer        ~1.07 ms median, ~0 % CPU
//!   * raw QPC spin                          0.50 ms +/- 0.007 ms, one core pinned
//!
//! Neither of the last two is good enough alone: the timer is imprecise, the
//! spin is precise but costs 100 % of a core. `Waiter` combines them -- it
//! sleeps on the timer until shortly before the deadline, then spins only the
//! remaining sliver. The margin is *learned* from observed timer overshoot
//! rather than hardcoded, so a slow machine widens it automatically instead of
//! missing deadlines.

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, GetCurrentThread,
    SetThreadAffinityMask, SetThreadPriority, SetWaitableTimer, THREAD_PRIORITY_NORMAL,
    THREAD_PRIORITY_TIME_CRITICAL, WaitForSingleObject,
};
use windows::core::PCWSTR;

const TIMER_ALL_ACCESS: u32 = 0x001F_0003;
const INFINITE: u32 = 0xFFFF_FFFF;

/// QPC ticks per second. Read once; the value cannot change while running.
pub fn qpf() -> i64 {
    use std::sync::OnceLock;
    static F: OnceLock<i64> = OnceLock::new();
    *F.get_or_init(|| {
        let mut v = 0i64;
        unsafe { QueryPerformanceFrequency(&mut v).ok() };
        if v == 0 { 10_000_000 } else { v }
    })
}

#[inline(always)]
pub fn qpc() -> i64 {
    let mut v = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

#[inline]
pub fn ticks_to_ns(t: i64) -> i64 {
    (t as i128 * 1_000_000_000i128 / qpf() as i128) as i64
}

#[inline]
pub fn ticks_to_ms(t: i64) -> f64 {
    t as f64 * 1000.0 / qpf() as f64
}

#[inline]
pub fn ms_to_ticks(ms: f64) -> i64 {
    (ms * qpf() as f64 / 1000.0) as i64
}

/// Hybrid sleep/spin waiter with a self-tuning spin margin.
pub struct Waiter {
    timer: Option<HANDLE>,
    /// How far before the deadline to stop sleeping and start spinning.
    margin: i64,
    /// Decaying high-water mark of observed timer overshoot.
    overshoot: i64,
    min_margin: i64,
    max_margin: i64,
    /// Ticks actually spent spinning -- lets the UI show the real CPU cost.
    pub spun: i64,
    pub slept: i64,
}

impl Waiter {
    pub fn new() -> Self {
        // The HIGH_RESOLUTION flag needs Win10 1803+. Falling back to a plain
        // timer still works; the adaptive margin just grows to compensate.
        let timer = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        }
        .ok()
        .or_else(|| {
            unsafe { CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS) }.ok()
        });

        Self {
            timer,
            margin: ms_to_ticks(0.30),
            overshoot: 0,
            min_margin: ms_to_ticks(0.05),
            max_margin: ms_to_ticks(3.0),
            spun: 0,
            slept: 0,
        }
    }

    pub fn margin_ms(&self) -> f64 {
        ticks_to_ms(self.margin)
    }

    /// Cap the spin margin for a given loop period.
    ///
    /// Without this the learned margin (driven by timer overshoot, ~0.5 ms on a
    /// typical machine) can equal or exceed a short period, so the loop spins
    /// 100 % of the time and the timer never gets used at all. Capping at half
    /// the period guarantees the sleep path still does real work at high rates.
    pub fn set_period(&mut self, period: i64) {
        self.max_margin = (period / 2).clamp(self.min_margin, ms_to_ticks(3.0));
        self.margin = self.margin.min(self.max_margin);
    }

    /// Fraction of wall time spent spinning since the last `reset_cost`.
    pub fn spin_fraction(&self) -> f32 {
        let total = self.spun + self.slept;
        if total <= 0 {
            0.0
        } else {
            (self.spun as f64 / total as f64) as f32
        }
    }

    pub fn reset_cost(&mut self) {
        self.spun = 0;
        self.slept = 0;
    }

    /// Block until `deadline` (an absolute QPC value).
    ///
    /// Absolute deadlines are deliberate: sleeping a fixed delta each iteration
    /// lets every preemption permanently shift the phase, and the error
    /// accumulates. Re-anchoring on an absolute target makes each late wake-up
    /// self-correcting.
    pub fn wait_until(&mut self, deadline: i64) {
        loop {
            let now = qpc();
            let remain = deadline - now;
            if remain <= 0 {
                return;
            }

            let sleep_for = remain - self.margin;
            // Only hand off to the timer if the sleep is long enough that the
            // timer can plausibly land before the deadline. Asking it for less
            // than the overshoot it historically adds just guarantees an
            // overshoot -- which is exactly how a 0.1 ms period ends up running
            // at 0.5 ms. Below that threshold, pure spin is the only option.
            if sleep_for >= self.min_useful_sleep() {
                if !self.sleep_ticks(sleep_for) {
                    // No timer available: fall through to spinning.
                    self.spin_until(deadline);
                    return;
                }
                let after = qpc();
                self.slept += after - now;

                // How much longer than asked did the timer actually take?
                let over = (after - now) - sleep_for;
                self.learn(over);
            } else {
                self.spin_until(deadline);
                return;
            }
        }
    }

    /// Shortest sleep worth asking the timer for: a high-resolution waitable
    /// timer lands no finer than ~0.5 ms in practice, so anything below that is
    /// better spun.
    ///
    /// Deliberately a fixed granularity floor, not the learned overshoot. Those
    /// are different quantities, and conflating them is a trap: `overshoot` is a
    /// high-water mark, so one unlucky sleep at a 100 ms period would raise the
    /// threshold above the entire period of a 1 ms loop and force it to spin
    /// 100 % forever. The learned value belongs in the margin; the gate belongs
    /// here.
    fn min_useful_sleep(&self) -> i64 {
        ms_to_ticks(0.5)
    }

    /// Widen the margin toward observed overshoot, and let it decay back down
    /// so one unlucky preemption does not inflate it permanently.
    fn learn(&mut self, over: i64) {
        let decayed = self.overshoot - self.overshoot / 32;
        self.overshoot = if over > decayed { over } else { decayed };
        let want = self.overshoot + self.overshoot / 4 + self.min_margin;
        self.margin = want.clamp(self.min_margin, self.max_margin);
    }

    fn sleep_ticks(&self, ticks: i64) -> bool {
        let Some(t) = self.timer else { return false };
        // Waitable timers take 100 ns units; QPF is not guaranteed to be 10 MHz.
        let hundred_ns = (ticks as i128 * 10_000_000i128 / qpf() as i128) as i64;
        if hundred_ns <= 0 {
            return true;
        }
        let due = -hundred_ns;
        unsafe {
            if SetWaitableTimer(t, &due, 0, None, None, false).is_err() {
                return false;
            }
            let _ = WaitForSingleObject(t, INFINITE);
        }
        true
    }

    #[inline]
    fn spin_until(&mut self, deadline: i64) {
        let start = qpc();
        while qpc() < deadline {
            // PAUSE: cuts power draw and frees the sibling hyperthread.
            std::hint::spin_loop();
        }
        self.spun += qpc() - start;
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        if let Some(t) = self.timer {
            unsafe {
                let _ = CloseHandle(t);
            }
        }
    }
}

// --------------------------------------------------------------- thread setup

/// Raise the calling thread to time-critical priority. Only worth doing while
/// a macro is actually running -- left on permanently it is rude to the rest of
/// the system, which matters most on the weak machines we care about.
pub fn set_time_critical(on: bool) {
    unsafe {
        let _ = SetThreadPriority(
            GetCurrentThread(),
            if on {
                THREAD_PRIORITY_TIME_CRITICAL
            } else {
                THREAD_PRIORITY_NORMAL
            },
        );
    }
}

/// Pin the calling thread to one core. Avoids core 0, which the kernel and
/// most drivers already contend for.
pub fn pin_to_core(core: usize) {
    let mask = 1usize << core;
    unsafe {
        SetThreadAffinityMask(GetCurrentThread(), mask);
    }
}

pub fn unpin(cores: usize) {
    let mask = if cores >= usize::BITS as usize {
        usize::MAX
    } else {
        (1usize << cores) - 1
    };
    unsafe {
        SetThreadAffinityMask(GetCurrentThread(), mask);
    }
}

pub fn core_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Preferred core for the engine: the last one, to stay away from core 0.
pub fn preferred_core() -> usize {
    core_count().saturating_sub(1)
}

/// RAII guard for the global timer resolution. Off by default -- the
/// high-resolution waitable timer does not need it, and raising it system-wide
/// costs battery on laptops.
pub struct TimerPeriod(bool);

impl TimerPeriod {
    pub fn acquire() -> Self {
        let ok = unsafe { timeBeginPeriod(1) } == 0;
        Self(ok)
    }
}

impl Drop for TimerPeriod {
    fn drop(&mut self) {
        if self.0 {
            unsafe { timeEndPeriod(1) };
        }
    }
}

// ------------------------------------------------------------------- rng

/// xorshift64* -- we need cheap jitter, not cryptography, and this avoids
/// pulling in a dependency for four lines of code.
pub struct Rng(u64);

impl Rng {
    pub fn new() -> Self {
        Self((qpc() as u64) | 1)
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [-1.0, 1.0].
    #[inline]
    pub fn bipolar(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}
