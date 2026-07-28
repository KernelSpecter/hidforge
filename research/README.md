# Research prototypes

Throwaway Python used to establish the facts the Rust engine is built on, kept
because the numbers are the justification for several design decisions. Pure
stdlib `ctypes`, no dependencies. Not part of the build.

## `bench_timing.py`

Measures the real ceiling on synthetic input, separating three questions that get
conflated:

- What does one `SendInput` call cost? (~47 µs via ctypes → 21k events/sec, so
  raw throughput is never the bottleneck at any useful click rate)
- How precisely can we wait? Default `Sleep` granularity is **15.6 ms**;
  `timeBeginPeriod(1)` gets `Sleep(1)` to ~1.54 ms; a high-resolution waitable
  timer lands ~1.07 ms at ~0% CPU; a QPC spin holds 0.5 ms at ±0.007 ms but burns
  a core.
- What click rate does each strategy actually sustain, with the injection cost
  inside the measured loop?

Conclusion that shaped the project: **language choice matters for jitter, not
throughput.** A 2 ms GC pause inside a 1 ms loop is a visible stutter and no CPU
speed fixes it — which is the argument for a no-GC language, and a different
argument from "faster". Rust then beat the prototype at every rate.

## `rawinput_probe.py`

Enumerates Raw Input devices and logs what each physical button emits. Answers the
question the whole "works on any mouse" claim depends on: extra buttons do not
arrive on a predictable channel, and some do not arrive at all.

Superseded by `hidforge.exe --probe`, which does the same thing with per-device
attribution and a distinct-signal summary.

Windows-only. Run in a console, press every button, press `Esc` to finish.
