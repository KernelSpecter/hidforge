"""
Measures the REAL ceiling on synthetic-input rate on this machine, so the
language choice is made on data instead of vibes.

Three separate questions, three separate measurements:

  A. What does one SendInput call cost?  -> is throughput ever the bottleneck?
  B. How precisely can we wait?          -> this is the actual limiter
  C. What CPS does each wait strategy imply?

Everything here is harmless: the injected event is a zero-delta mouse move
(MOUSEEVENTF_MOVE with dx=dy=0). No clicks are sent, nothing on screen moves.

Python's own interpreter overhead is included, so every number is a LOWER
bound on what a native engine achieves. That is exactly what we need: if the
ceiling is already far above any useful click rate even in Python, then raw
language speed is not the thing to optimise -- jitter is.
"""

import ctypes as C
import ctypes.wintypes as W
import statistics as st

user32 = C.WinDLL("user32", use_last_error=True)
kernel32 = C.WinDLL("kernel32", use_last_error=True)
winmm = C.WinDLL("winmm", use_last_error=True)

INPUT_MOUSE = 0
MOUSEEVENTF_MOVE = 0x0001

CREATE_WAITABLE_TIMER_HIGH_RESOLUTION = 0x00000002
TIMER_ALL_ACCESS = 0x1F0003
INFINITE = 0xFFFFFFFF

ULONG_PTR = C.c_ulonglong if C.sizeof(C.c_void_p) == 8 else C.c_ulong


class MOUSEINPUT(C.Structure):
    _fields_ = [("dx", W.LONG), ("dy", W.LONG), ("mouseData", W.DWORD),
                ("dwFlags", W.DWORD), ("time", W.DWORD),
                ("dwExtraInfo", ULONG_PTR)]


class KEYBDINPUT(C.Structure):
    _fields_ = [("wVk", W.WORD), ("wScan", W.WORD), ("dwFlags", W.DWORD),
                ("time", W.DWORD), ("dwExtraInfo", ULONG_PTR)]


class HARDWAREINPUT(C.Structure):
    _fields_ = [("uMsg", W.DWORD), ("wParamL", W.WORD), ("wParamH", W.WORD)]


class _IU(C.Union):
    _fields_ = [("mi", MOUSEINPUT), ("ki", KEYBDINPUT), ("hi", HARDWAREINPUT)]


class INPUT(C.Structure):
    _anonymous_ = ("u",)
    _fields_ = [("type", W.DWORD), ("u", _IU)]


user32.SendInput.argtypes = [W.UINT, C.POINTER(INPUT), C.c_int]
user32.SendInput.restype = W.UINT
kernel32.QueryPerformanceCounter.argtypes = [C.POINTER(C.c_int64)]
kernel32.QueryPerformanceFrequency.argtypes = [C.POINTER(C.c_int64)]
kernel32.CreateWaitableTimerExW.argtypes = [C.c_void_p, W.LPCWSTR, W.DWORD, W.DWORD]
kernel32.CreateWaitableTimerExW.restype = W.HANDLE
kernel32.SetWaitableTimer.argtypes = [W.HANDLE, C.POINTER(C.c_int64), W.LONG,
                                      C.c_void_p, C.c_void_p, W.BOOL]
kernel32.WaitForSingleObject.argtypes = [W.HANDLE, W.DWORD]
kernel32.Sleep.argtypes = [W.DWORD]
winmm.timeBeginPeriod.argtypes = [W.UINT]
winmm.timeBeginPeriod.restype = W.UINT   # TIMERR_NOERROR=0 / TIMERR_NOCANDO=97
winmm.timeEndPeriod.argtypes = [W.UINT]
winmm.timeEndPeriod.restype = W.UINT
kernel32.CloseHandle.argtypes = [W.HANDLE]

TIMERR_NOERROR = 0

# NtQueryTimerResolution reports the actual granularity the system is running
# at, in 100ns units -- the ground truth for whether timeBeginPeriod took.
try:
    ntdll = C.WinDLL("ntdll")
    ntdll.NtQueryTimerResolution.argtypes = [
        C.POINTER(W.ULONG), C.POINTER(W.ULONG), C.POINTER(W.ULONG)]

    def timer_resolution_ms():
        mn, mx, cur = W.ULONG(), W.ULONG(), W.ULONG()
        ntdll.NtQueryTimerResolution(C.byref(mx), C.byref(mn), C.byref(cur))
        # note: first out-param is the *maximum* interval (coarsest)
        return mn.value / 10000.0, mx.value / 10000.0, cur.value / 10000.0
except Exception:
    def timer_resolution_ms():
        return (None, None, None)

_qpf = C.c_int64()
kernel32.QueryPerformanceFrequency(C.byref(_qpf))
QPF = float(_qpf.value)
_ctr = C.c_int64()


def qpc():
    kernel32.QueryPerformanceCounter(C.byref(_ctr))
    return _ctr.value


def to_ms(ticks):
    return ticks * 1000.0 / QPF


def stats(xs):
    s = sorted(xs)
    return {
        "min": s[0], "med": st.median(s),
        "p95": s[min(len(s) - 1, int(0.95 * (len(s) - 1)))],
        "max": s[-1],
        "stdev": st.pstdev(s) if len(s) > 1 else 0.0,
    }


def row(name, d, unit="ms"):
    print(f"  {name:34} min {d['min']:8.4f}  med {d['med']:8.4f}  "
          f"p95 {d['p95']:8.4f}  max {d['max']:9.4f}  sd {d['stdev']:7.4f} {unit}")


# ----------------------------------------------------------- A: SendInput cost

def bench_sendinput(n=20000, batch=1):
    arr = (INPUT * batch)()
    for i in range(batch):
        arr[i].type = INPUT_MOUSE
        arr[i].mi = MOUSEINPUT(0, 0, 0, MOUSEEVENTF_MOVE, 0, 0)
    sz = C.sizeof(INPUT)
    calls = n // batch

    # warm up the syscall path and the ctypes marshalling caches
    for _ in range(500):
        user32.SendInput(batch, arr, sz)

    t0 = qpc()
    for _ in range(calls):
        user32.SendInput(batch, arr, sz)
    t1 = qpc()

    total_ms = to_ms(t1 - t0)
    per_event_us = total_ms * 1000.0 / (calls * batch)
    return {
        "events": calls * batch,
        "total_ms": total_ms,
        "per_event_us": per_event_us,
        "events_per_sec": (calls * batch) / (total_ms / 1000.0),
    }


# ----------------------------------------------------------- B: wait precision

def measure_wait(waiter, n=300):
    """Return observed interval between successive returns of waiter()."""
    waiter()  # warm
    out = []
    prev = qpc()
    for _ in range(n):
        waiter()
        now = qpc()
        out.append(to_ms(now - prev))
        prev = now
    return out


def make_sleep(ms):
    return lambda: kernel32.Sleep(ms)


def make_hires_timer(target_ms):
    h = kernel32.CreateWaitableTimerExW(
        None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS)
    if not h:
        return None, None
    due = C.c_int64(int(-target_ms * 10_000))  # negative = relative, 100ns units

    def waiter():
        kernel32.SetWaitableTimer(h, C.byref(due), 0, None, None, False)
        kernel32.WaitForSingleObject(h, INFINITE)

    return waiter, h


def make_spin(target_ms):
    ticks = int(target_ms * QPF / 1000.0)

    def waiter():
        end = qpc() + ticks
        while qpc() < end:
            pass

    return waiter


# ------------------------------------------- C: realistic full click loop


def bench_click_loop(kind, period_ms, n=400):
    """One iteration = wait until an absolute deadline, then inject down+up.

    Re-anchoring on an absolute deadline (rather than sleeping a fixed delta)
    is what stops a preemption from permanently shifting the phase -- error
    corrects itself instead of accumulating. Same reason the real engine must
    do it this way.

    Injection is a zero-delta MOUSEEVENTF_MOVE pair, not a real click, so this
    is harmless but costs the same two SendInput calls a click would.
    """
    down = (INPUT * 1)()
    down[0].type = INPUT_MOUSE
    down[0].mi = MOUSEINPUT(0, 0, 0, MOUSEEVENTF_MOVE, 0, 0)
    sz = C.sizeof(INPUT)

    h = None
    if kind == "timer":
        h = kernel32.CreateWaitableTimerExW(
            None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS)
        if not h:
            return None

    ticks = int(period_ms * QPF / 1000.0)
    deadline = qpc() + ticks
    intervals = []
    prev = qpc()

    for _ in range(n):
        if kind == "spin":
            while qpc() < deadline:
                pass
        else:
            remain = deadline - qpc()
            if remain > 0:
                # waitable timers take 100ns units; QPF is not necessarily 10MHz
                due = C.c_int64(-int(remain * (10_000_000.0 / QPF)))
                kernel32.SetWaitableTimer(h, C.byref(due), 0, None, None, False)
                kernel32.WaitForSingleObject(h, INFINITE)

        user32.SendInput(1, down, sz)   # "button down"
        user32.SendInput(1, down, sz)   # "button up"

        now = qpc()
        intervals.append(to_ms(now - prev))
        prev = now
        deadline += ticks
        if deadline < now:      # fell behind: re-anchor, don't spiral
            deadline = now + ticks

    if h:
        kernel32.CloseHandle(h)
    return {"intervals": stats(intervals)}


# ----------------------------------------------------------------------- main

def main():
    print(f"QPF = {QPF:,.0f} Hz   (one tick = {1e9/QPF:.1f} ns)")
    print(f"pointer size = {C.sizeof(C.c_void_p)*8}-bit   "
          f"sizeof(INPUT) = {C.sizeof(INPUT)} (expect 40)   "
          f"sizeof(MOUSEINPUT) = {C.sizeof(MOUSEINPUT)} (expect 32)\n")

    print("=" * 100)
    print("A. SendInput throughput  (zero-delta MOUSEEVENTF_MOVE -- harmless, "
          "nothing moves)")
    print("=" * 100)
    for batch in (1, 2, 8):
        r = bench_sendinput(20000, batch)
        print(f"  batch={batch:<2}  {r['events']:6} events in "
              f"{r['total_ms']:8.2f} ms  ->  {r['per_event_us']:7.3f} us/event"
              f"   {r['events_per_sec']:12,.0f} events/sec")
    print("\n  A click = 2 events (down+up). Divide events/sec by 2 for the")
    print("  theoretical click ceiling if we never waited at all.\n")

    print("=" * 100)
    print("B. Wait precision -- THIS is what actually limits click rate")
    print("=" * 100)

    mn, mx, cur = timer_resolution_ms()
    if cur is not None:
        print(f"\n  NtQueryTimerResolution: system min {mn:.4f} ms, "
              f"max {mx:.4f} ms, CURRENTLY {cur:.4f} ms")

    print("\n  [default timer resolution]")
    row("Sleep(1)", stats(measure_wait(make_sleep(1), 200)))

    rc = winmm.timeBeginPeriod(1)
    try:
        mn, mx, cur = timer_resolution_ms()
        print(f"\n  [timeBeginPeriod(1) returned {rc} "
              f"({'TIMERR_NOERROR - granted' if rc == TIMERR_NOERROR else 'TIMERR_NOCANDO - REFUSED'})"
              + (f"; resolution now {cur:.4f} ms]" if cur is not None else "]"))
        row("Sleep(1)", stats(measure_wait(make_sleep(1), 200)))
        row("Sleep(2)", stats(measure_wait(make_sleep(2), 200)))

        wt, h = make_hires_timer(1.0)
        if wt:
            row("waitable hi-res timer @1.0ms", stats(measure_wait(wt, 200)))
            kernel32.CloseHandle(h)
        wt, h = make_hires_timer(0.5)
        if wt:
            row("waitable hi-res timer @0.5ms", stats(measure_wait(wt, 200)))
            kernel32.CloseHandle(h)
    finally:
        winmm.timeEndPeriod(1)

    print("\n  [busy-wait on QueryPerformanceCounter -- burns one core]")
    for t in (1.0, 0.5, 0.1, 0.05):
        row(f"spin @{t}ms", stats(measure_wait(make_spin(t), 400)))

    print("\n" + "=" * 100)
    print("C. Real sustainable click rate -- the wait AND the injection in the")
    print("   same loop, on an absolute QPC deadline (phase cannot drift)")
    print("=" * 100)
    winmm.timeBeginPeriod(1)
    try:
        for name, kind, target in (
            ("waitable hi-res timer (~0% CPU)", "timer", 1.0),
            ("spin (100% of one core)", "spin", 1.0),
            ("spin (100% of one core)", "spin", 0.5),
            ("spin (100% of one core)", "spin", 0.1),
        ):
            r = bench_click_loop(kind, target, n=400)
            if r is None:
                continue
            d = r["intervals"]
            print(f"  {name:34} period={target:4.2f}ms  ->  "
                  f"med {d['med']:7.4f} ms = {1000.0/d['med']:8.1f} CPS   "
                  f"p95 {d['p95']:7.4f} ms = {1000.0/d['p95']:8.1f} CPS   "
                  f"worst {d['max']:8.3f} ms")
        print("\n  'worst' is a scheduler preemption -- the thing that makes an")
        print("  autoclicker feel broken. Cutting it needs TIME_CRITICAL priority")
        print("  and core affinity, which is a native-code job.")
    finally:
        winmm.timeEndPeriod(1)

    print("""
READ THIS BEFORE PICKING A LANGUAGE
-----------------------------------
If section A shows six figures of events/sec, then even CPython can issue
input faster than any click rate a human or game cares about -- so raw
language throughput is NOT the constraint.

What section B shows is the constraint: how tightly you can hold an interval.
That is a jitter problem, and jitter is where a GC language actually loses --
a 2 ms collection pause inside a 1 ms click loop is a visible stutter, and no
amount of CPU speed fixes it. That is the real argument for a no-GC language,
and it is a different argument from "faster".
""")


if __name__ == "__main__":
    main()
