//! `hidforge.exe --bench` -- headless self-test of the engine.
//!
//! Exists so the timing core can be verified without touching the GUI, and so
//! there is a way to check a machine's real ceiling from a script. Injects only
//! zero-delta mouse moves, so it is safe to run at any rate: it costs exactly
//! what a click costs but nothing observable happens.

use std::sync::atomic::Ordering;
use std::thread::sleep;
use std::time::Duration;

use windows::Win32::Foundation::POINT;
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_ESCAPE, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

use crate::engine::{Engine, Job, Mode, Opts};
use crate::inject::{self, Btn};

fn console() {
    // The release binary is a windows-subsystem app with no console of its own.
    // Borrow the launching shell's so println! is visible.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[inline]
fn key_down(vk: u16) -> bool {
    (unsafe { GetAsyncKeyState(vk as i32) } as u16) & 0x8000 != 0
}

/// `--selftest` -- exercises the paths `--bench` deliberately does not.
///
/// The benchmark injects zero-delta moves, so it proves the timing loop and
/// nothing about actual clicking. This drives real button injection and checks
/// the OS agrees the button went down, which is the only way to catch a wrong
/// `MOUSEEVENTF_*` / `mouseData` encoding -- that failure mode is silent.
pub fn selftest() {
    console();
    println!("hidforge self-test\n");
    println!("This injects REAL clicks into whatever window has focus.");
    println!("Focus something harmless (an empty Notepad, or the desktop) now.");
    for i in (1..=4).rev() {
        println!("  starting in {i}...");
        sleep(Duration::from_secs(1));
    }
    println!();

    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut check = |name: &str, ok: bool, detail: String| {
        if ok {
            pass += 1;
            println!("  PASS  {name:<34} {detail}");
        } else {
            fail += 1;
            println!("  FAIL  {name:<34} {detail}");
        }
    };

    // --- A. Button encoding, verified against the OS key state ---------------
    println!("A. button injection (OS-observed state)");
    for (btn, vk, vname) in [
        (Btn::Left, VK_LBUTTON, "VK_LBUTTON"),
        (Btn::Right, VK_RBUTTON, "VK_RBUTTON"),
        (Btn::Middle, VK_MBUTTON, "VK_MBUTTON"),
        (Btn::X1, VK_XBUTTON1, "VK_XBUTTON1"),
        (Btn::X2, VK_XBUTTON2, "VK_XBUTTON2"),
    ] {
        inject::mouse(btn, true);
        sleep(Duration::from_millis(40));
        let seen_down = key_down(vk.0);
        inject::mouse(btn, false);
        sleep(Duration::from_millis(40));
        let seen_up = !key_down(vk.0);
        check(
            btn.name(),
            seen_down && seen_up,
            format!("{vname} down={seen_down} released={seen_up}"),
        );
    }
    // A right-click may have opened a context menu -- dismiss it.
    inject::key_vk(VK_ESCAPE.0, true);
    inject::key_vk(VK_ESCAPE.0, false);
    sleep(Duration::from_millis(80));

    // --- B. Relative motion --------------------------------------------------
    println!("\nB. relative motion");
    let mut before = POINT::default();
    let _ = unsafe { GetCursorPos(&mut before) };
    inject::move_rel(37, 23);
    sleep(Duration::from_millis(60));
    let mut after = POINT::default();
    let _ = unsafe { GetCursorPos(&mut after) };
    let moved = (after.x - before.x, after.y - before.y);
    check(
        "move_rel(37,23)",
        moved.0 != 0 && moved.1 != 0,
        format!("cursor moved by {moved:?}"),
    );
    let _ = unsafe { SetCursorPos(before.x, before.y) };

    // --- C. The injected-input filter the UI promises ------------------------
    // Load-bearing twice: without it a trigger bound to left-click retriggers
    // itself forever, and the CPS test counts our own output as human clicks.
    // NOTE: this check used to assert only "0 events surfaced", which passes both
    // when filtering works AND when the whole capture pipeline is dead. That
    // ambiguity hid a real bug. It now reports the raw WM_INPUT counter too, so
    // the two cases are distinguishable. Use `--probe` for the full picture.
    println!("\nC. injected events do not surface as input");
    let mut raw = crate::rawinput::RawInput::start(egui::Context::default());
    raw.set_capture(true);
    sleep(Duration::from_millis(500));
    let _ = raw.drain();
    let before = crate::rawinput::RAW_MSGS.load(Ordering::Relaxed);
    for _ in 0..15 {
        inject::mouse(Btn::Left, true);
        inject::mouse(Btn::Left, false);
        sleep(Duration::from_millis(12));
    }
    sleep(Duration::from_millis(300));
    let leaked = raw.drain().len();
    let after = crate::rawinput::RAW_MSGS.load(Ordering::Relaxed);
    check(
        "30 injected events not surfaced",
        leaked == 0,
        format!("surfaced {leaked}, wm_input {before}->{after}"),
    );
    println!(
        "        (wm_input MUST increase by ~30 here. If it does not, the pump is\n\
         \x20        starved and this check is passing vacuously -- that exact failure\n\
         \x20        hid a registration feedback loop once. Capture of REAL buttons is\n\
         \x20        still not proven here; run --probe and press them.)"
    );
    check(
        "capture pipeline is alive",
        after > before,
        format!("wm_input rose by {}", after.saturating_sub(before)),
    );
    raw.set_capture(false);

    // --- D. The real AutoClick path, including the hold wait ----------------
    println!("\nD. AutoClick mode end-to-end");
    let engine = Engine::start();
    let want = 40u64;
    engine.run(
        Job {
            limit: Some(want),
            ..Job::click(Btn::Left, 50.0, 2.0)
        },
        Opts::default(),
    );
    let start = crate::clock::qpc();
    while crate::clock::qpc() - start < crate::clock::ms_to_ticks(5000.0) {
        sleep(Duration::from_millis(20));
        if !engine.tel.running.load(Ordering::Relaxed)
            && engine.tel.events.load(Ordering::Relaxed) >= want
        {
            break;
        }
    }
    let s = engine.tel.snapshot();
    check(
        "40 clicks at 50 cps",
        s.events == want,
        format!("issued {} events", s.events),
    );
    check(
        "period ~20 ms with 2 ms hold",
        (s.mean_ms - 20.0).abs() < 3.0,
        format!("mean {:.3} ms", s.mean_ms),
    );

    // --- E. Recorded-sequence playback -------------------------------------
    // Types "hi" with 40 ms of delay per pass, three passes. Verifies the step
    // interpreter, scancode replay, and that the delay timeline is honoured.
    println!("\nE. macro sequence playback");
    use crate::macros::Step;
    let steps = vec![
        Step::Key {
            vk: 0x48,
            scan: 0x23,
            down: true,
        }, // H
        Step::Delay { ms: 20.0 },
        Step::Key {
            vk: 0x48,
            scan: 0x23,
            down: false,
        },
        Step::Delay { ms: 20.0 },
        Step::Key {
            vk: 0x49,
            scan: 0x17,
            down: true,
        }, // I
        Step::Key {
            vk: 0x49,
            scan: 0x17,
            down: false,
        },
    ];
    let t0 = crate::clock::qpc();
    engine.run(
        Job {
            mode: Mode::Sequence,
            limit: Some(3),
            steps,
            speed: 1.0,
            ..Job::click(Btn::Left, 1.0, 0.0)
        },
        Opts::default(),
    );
    while crate::clock::qpc() - t0 < crate::clock::ms_to_ticks(4000.0) {
        sleep(Duration::from_millis(10));
        if !engine.tel.running.load(Ordering::Relaxed)
            && engine.tel.events.load(Ordering::Relaxed) >= 3
        {
            break;
        }
    }
    let elapsed = crate::clock::ticks_to_ms(crate::clock::qpc() - t0);
    let s2 = engine.tel.snapshot();
    check(
        "3 passes completed",
        s2.events == 3,
        format!("{} passes", s2.events),
    );
    check(
        "delay timeline honoured (~120 ms)",
        elapsed > 100.0 && elapsed < 400.0,
        format!("took {elapsed:.1} ms"),
    );

    println!("\n{pass} passed, {fail} failed");
}

/// `--probe [seconds]` -- ground truth on the capture pipeline.
///
/// Prints the diagnostic counters at each stage so a silent capture layer can be
/// distinguished from a working filter, then logs every real signal so we can see
/// exactly what each physical button emits.
pub fn probe(secs: f64) {
    console();
    println!("hidforge raw input probe\n");

    let mut raw = crate::rawinput::RawInput::start(egui::Context::default());
    println!("enumerated {} devices", raw.devices.len());
    for d in &raw.devices {
        let extra = match d.kind {
            "Mouse" => format!("{} buttons", d.buttons),
            "Keyboard" => format!("{} keys", d.buttons),
            _ => format!("page {:#06x} usage {:#06x}", d.usage_page, d.usage),
        };
        println!("   {:<9} {:<14} {}", d.kind, d.label(), extra);
    }

    println!("\nstage 0 (before capture): {}", crate::rawinput::diag());
    raw.set_capture(true);
    sleep(Duration::from_millis(600));
    println!("stage 1 (capture on):     {}", crate::rawinput::diag());

    // Inject a few events. These are ours, so they must be DROPPED -- but they
    // still have to show up in wm_input. If wm_input stays 0 here, the pipeline
    // is dead and nothing about real buttons matters yet.
    for _ in 0..5 {
        inject::mouse(Btn::Left, true);
        inject::mouse(Btn::Left, false);
        sleep(Duration::from_millis(25));
    }
    sleep(Duration::from_millis(400));
    let d = crate::rawinput::diag();
    println!("stage 2 (10 injected):    {d}");
    let alive = crate::rawinput::RAW_MSGS.load(Ordering::Relaxed) > 0;
    println!(
        "\n  PIPELINE {}",
        if alive {
            "ALIVE -- WM_INPUT is arriving"
        } else {
            "DEAD -- no WM_INPUT at all; registration or window is the problem"
        }
    );
    let _ = raw.drain();

    // handle -> label, so every line says which physical device it came from.
    let names: std::collections::HashMap<i64, String> =
        raw.devices.iter().map(|d| (d.handle, d.label())).collect();
    let name_of = |h: i64| -> String {
        names
            .get(&h)
            .cloned()
            .unwrap_or_else(|| format!("handle {h:#x}"))
    };

    println!(
        "\nNow press things for {secs:.0} s -- every mouse button including the side\n\
         buttons and DPI button, the wheel, then a few keyboard keys.\n"
    );
    let start = crate::clock::qpc();
    let dur = crate::clock::ms_to_ticks(secs * 1000.0);
    let mut seen = 0usize;
    // Distinct signal -> (count, device). This is the table that answers "what
    // does button 6 actually emit"; the scrolling log alone is unreadable.
    let mut distinct: std::collections::BTreeMap<String, (usize, i64)> = Default::default();

    while crate::clock::qpc() - start < dur {
        for e in raw.drain() {
            seen += 1;
            let lbl = e.signal.label();
            let ent = distinct
                .entry(lbl.clone())
                .or_insert((0, e.signal.device()));
            ent.0 += 1;
            if e.down {
                println!(
                    "  [{:7.3}s] {:<30} {:<22} DOWN",
                    crate::clock::ticks_to_ms(e.qpc - start) / 1000.0,
                    lbl,
                    name_of(e.signal.device()),
                );
            }
        }
        sleep(Duration::from_millis(15));
    }

    println!("\nfinal: {}", crate::rawinput::diag());
    println!(
        "{seen} events surfaced, {} distinct signals\n",
        distinct.len()
    );

    println!("{:<32} {:>7}  DEVICE", "SIGNAL", "EVENTS");
    println!("{}", "-".repeat(78));
    let mut rows: Vec<_> = distinct.iter().collect();
    rows.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
    for (sig, (n, dev)) in rows {
        println!("{:<32} {:>7}  {}", sig, n, name_of(*dev));
    }

    if seen == 0 {
        println!(
            "\nNothing surfaced at all. If wm_input above is non-zero, events are\n\
             arriving but being classified out. If it is zero, registration never\n\
             took effect and no amount of pressing will help."
        );
    }
}

/// `--bench` -- headless timing sweep. Injects only zero-delta moves, so it is
/// safe to run at any rate. See `selftest` for the real click path.
pub fn run() {
    console();
    println!(
        "hidforge engine self-test\n\
         qpf {} Hz  ({:.1} ns/tick)   {} cores, engine pinned to #{}\n",
        crate::clock::qpf(),
        1e9 / crate::clock::qpf() as f64,
        crate::clock::core_count(),
        crate::clock::preferred_core()
    );

    let engine = Engine::start();
    let opts = Opts::default();

    println!(
        "{:>9}  {:>10}  {:>10}  {:>10}  {:>10}  {:>9}  {:>8}",
        "target", "mean", "p95", "p99", "worst", "achieved", "spin"
    );
    println!("{}", "-".repeat(80));

    for target in [10.0, 100.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0] {
        // Enough samples to get a stable p99 without waiting forever at 10 cps.
        let n = (target as u64 * 2).clamp(200, 4000);

        engine.run(
            Job {
                mode: Mode::Benchmark,
                limit: Some(n),
                ..Job::click(Btn::Left, target, 0.0)
            },
            opts,
        );

        // Wait for the run to finish. Generous cap so a slow machine still
        // reports instead of hanging.
        let start = crate::clock::qpc();
        let cap = crate::clock::ms_to_ticks(n as f64 / target * 1000.0 * 3.0 + 4000.0);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let running = engine.tel.running.load(Ordering::Relaxed);
            let done = engine.tel.events.load(Ordering::Relaxed) >= n;
            if (!running && done) || crate::clock::qpc() - start > cap {
                break;
            }
        }
        engine.stop();

        let s = engine.tel.snapshot();
        println!(
            "{:>6.0}cps  {:>8.3}ms  {:>8.3}ms  {:>8.3}ms  {:>8.2}ms  {:>6.0}cps  {:>6.0}%",
            target,
            s.mean_ms,
            s.p95_ms,
            s.p99_ms,
            s.max_ms,
            s.cps(s.mean_ms),
            s.spin_pct
        );
    }

    println!(
        "\n'achieved' comes from the exact mean period, so it is the rate actually\n\
         sustained rather than a theoretical figure. p95/p99 are read off a log\n\
         histogram and are quantised to ~1.9 %. 'spin' is the share of one core the\n\
         hybrid waiter burned: the loop sleeps on the high-resolution timer until a\n\
         learned margin remains -- capped at half the period -- then spins that sliver."
    );
}
