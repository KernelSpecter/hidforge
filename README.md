# HidForge

A device-agnostic macro engine for Windows. Works with any mouse or keyboard,
without vendor software.

Built because OEM macro tools (G HUB, Synapse, iCUE, ANT) are shallow in the same
way: flat action lists, no real repeat semantics, poor recording fidelity, and each
one only talks to its own hardware. HidForge does the software-layer part properly
and is honest about the part that is physically impossible.

## What it does

- **Learns any button.** Rather than assuming "button 4 is the side button", you
  press a button and it captures whatever that button actually emits, a standard
  HID button, a keyboard scancode, a consumer-page usage, or a vendor-page bit.
  This is the only approach that works across a 2-button office mouse and a
  12-button MMO mouse.
- **Records and replays macros** with real timing, captured straight off the
  hardware via Raw Input (so side buttons and every key are included). Keys replay
  by **scancode**, which more games honour than a bare virtual key.
- **Repeat semantics that mean something**: cycle until released · cycle until
  clicked again · cycle exactly N times.
- **A serious timing core.** Sustains an exact requested rate from 10 to ~10,000
  clicks per second, and costs 0% CPU at idle.
- **Manual profiles**, a CPS test, and a live timing scope.
- **Emergency stop: `Esc` always stops everything**, whatever is running.
- Sits in the system tray; closing the window hides it and leaves macros armed.

## Download

Get `hidforge.exe` from the [latest release](https://github.com/KernelSpecter/hidforge/releases/latest).
It is a single file with nothing to install, and needs 64-bit Windows 10 or 11.

The exe is not code signed, so the first time you run it Windows SmartScreen will
say it protected your PC. Click More info, then Run anyway. Each release also
carries a SHA-256 checksum if you want to check the file first:

```
certutil -hashfile hidforge.exe SHA256
```

## Honest limits

These are real constraints, not missing features.

- **Buttons handled inside the mouse's firmware cannot be bound by anyone.** DPI
  cycle, LED cycle and onboard "rapid fire" buttons never send anything to
  Windows, so there is no event to capture. Vendor software reaches them over a
  private USB protocol to the chip. If a button logs nothing in `--probe`, it emits
  nothing at all.
- **No onboard-memory macros.** Vendor tools flash macros into the mouse so they
  work with no software running, on any PC. That needs each vendor's proprietary,
  often encrypted protocol. HidForge always needs its process running.
- **Bindings are additive, not replacements.** Raw Input can identify the device
  but cannot suppress the event, so an assigned button still performs its original
  function too. Genuine replacement needs a `WH_MOUSE_LL` hook (standard mouse
  buttons only) or HIDHide.
- **Synthetic input is detectable.** `SendInput` events arrive at applications
  flagged as injected, and many games filter exactly that. Making output
  indistinguishable from real hardware requires a virtual HID device, which means a
  signed kernel driver. No amount of timing work changes this.
- **Software can only *increase* debounce, never reduce it.** Firmware debounces
  before anything reaches the USB wire, so the suppressed events do not exist as
  far as the OS is concerned. Raising it can fix a chattering worn switch; lowering
  it is impossible.
- **Elevated apps need an elevated HidForge.** Windows UIPI blocks a normal-privilege
  process from injecting into a process running as administrator. Run HidForge as
  administrator if you need macros inside one. Nothing can inject into the UAC
  secure desktop.

## Measured performance

From `hidforge.exe --bench` on an 8-core machine (QPF 10 MHz), engine pinned to the
last core:

| target | achieved | mean period | worst stall | CPU |
|--------:|---------:|------------:|------------:|----:|
| 10 cps | 10 | 100.001 ms | 100.3 ms | 0% |
| 100 cps | 100 | 10.001 ms | 10.3 ms | 3% |
| 500 cps | 500 | 2.000 ms | 3.8 ms | 19% |
| 1 000 cps | 1 000 | 1.000 ms | 1.6 ms | 100% |
| 5 000 cps | 4 956 | 0.202 ms | 0.9 ms | 100% |
| 10 000 cps | 9 660 | 0.104 ms | 1.2 ms | 100% |

The interesting part is the CPU column. Windows' default sleep granularity is
15.6 ms and a high-resolution waitable timer only lands within ~0.5 ms, so hitting
a 1 ms period normally means spinning a whole core. HidForge uses a **hybrid
waiter**: it sleeps on the high-resolution timer until a *learned* margin remains,
then spins only that sliver. The margin is derived from observed timer overshoot, so
a slow machine widens it automatically.

Two constraints in that loop are load-bearing:

- The margin is capped at half the period. Uncapped, the learned value (~0.6 ms)
  equals a 1 ms period and the loop spins 100% for no benefit.
- The "is this sleep worth handing to the timer" gate is a **fixed ~0.5 ms
  granularity floor, not the learned overshoot**. Overshoot is a high-water mark,
  so one unlucky sleep at a long period would raise the gate above a short period
  entirely and force permanent spinning.

Deadlines are absolute and re-anchored every iteration. Sleeping a fixed delta lets
every scheduler preemption permanently shift the phase, and the error accumulates.

## Building

Needs a Rust toolchain and the MSVC linker (Visual Studio Build Tools with the C++
workload).

```
cargo build --release
```

Produces a single self-contained `target/release/hidforge.exe` (~5.7 MB, no runtime
dependencies). The renderer is `glow` (OpenGL 3.3) rather than wgpu, for driver
compatibility on older Intel integrated graphics.

## Command line

```
hidforge.exe                 launch the GUI
hidforge.exe --probe [secs]  capture diagnostics: what does each button emit?
hidforge.exe --bench         headless timing sweep
hidforge.exe --selftest      12 functional checks (injects real clicks - focus an empty Notepad)
```

`--probe` is the tool to reach for first when something is not detected. It prints
staged counters and then one row per distinct signal.

**`reg_ok` is the canary.** It should be a single digit. If it is in the thousands,
Raw Input registration is looping and starving the message pump - which presents as
"buttons only register if I keep pressing, and randomly at that".

## Layout

```
src/clock.rs      QPC clock, hybrid sleep/spin waiter, thread priority and affinity
src/engine.rs     the macro engine thread: click loop, sequence playback, telemetry
src/inject.rs     SendInput wrappers, tagged so we recognise our own output
src/rawinput.rs   device discovery, button learning, HID report bit-diff
src/macros.rs     step model, recorder, profiles, JSON persistence
src/app.rs        UI
src/theme.rs      visual design
src/tray.rs       system tray icon
src/bench.rs      --bench / --selftest / --probe
research/         Python prototypes used to establish the timing ceilings
```

Config lives at `%APPDATA%\hidforge\config.json`.

## Notes on the design

`SetWindowsHookEx(WH_MOUSE_LL)` can block an event but cannot tell you which device
sent it. Raw Input tells you exactly which device but is observe-only. That tension
shapes everything: HidForge uses Raw Input, so it can distinguish your mouse from
your touchpad and see vendor-specific buttons, at the cost of not being able to
suppress the original event.

Digitizer collections (HID usage page 0x0D - touchscreens and precision touchpads)
are deliberately excluded. They stream contact coordinates, so a report bit-diff
turns finger movement into phantom button presses.

Raw Input capture is only registered while something needs it. A 1000 Hz mouse is
not free to listen to.

## License

MIT - see [LICENSE](LICENSE).
