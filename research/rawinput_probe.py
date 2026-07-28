"""
Raw Input probe -- answers the three questions that decide the architecture:

  1. Which physical devices are attached, and what does Windows think each one is?
  2. When you press EVERY button on your mouse, which device+channel does each
     button actually arrive on? (standard HID button / keyboard scancode /
     consumer-control page / vendor page / nothing at all)
  3. What is the firmware's real debounce floor and poll rate, measured -- not
     guessed -- from the wire?

Pure stdlib ctypes. No deps. Run in a console, press every button, click as
fast as you can, then hit ESC.

Writes a JSON log next to this file for later analysis.
"""

import ctypes as C
import ctypes.wintypes as W
import json
import os
import re
import sys
import time
from collections import defaultdict

user32 = C.WinDLL("user32", use_last_error=True)
kernel32 = C.WinDLL("kernel32", use_last_error=True)

# ---------------------------------------------------------------- constants

WM_INPUT = 0x00FF
WM_CLOSE = 0x0010
WM_DESTROY = 0x0002
HWND_MESSAGE = -3

RID_INPUT = 0x10000003
RIDI_DEVICENAME = 0x20000007
RIDI_DEVICEINFO = 0x2000000B

RIM_TYPEMOUSE, RIM_TYPEKEYBOARD, RIM_TYPEHID = 0, 1, 2
# Anything above 2 is undocumented but does occur in the wild (seen: 3 on an
# ELAN touchpad collection). Treat unknown types as HID-ish so we still
# register for them and still dump their reports.
TYPE_NAME = {0: "MOUSE", 1: "KEYBOARD", 2: "HID"}

RIDEV_INPUTSINK = 0x00000100  # receive input even when not focused

# RAWMOUSE.usButtonFlags
BTN_FLAGS = [
    (0x0001, "BTN1_LEFT", "down"),
    (0x0002, "BTN1_LEFT", "up"),
    (0x0004, "BTN2_RIGHT", "down"),
    (0x0008, "BTN2_RIGHT", "up"),
    (0x0010, "BTN3_MIDDLE", "down"),
    (0x0020, "BTN3_MIDDLE", "up"),
    (0x0040, "BTN4", "down"),
    (0x0080, "BTN4", "up"),
    (0x0100, "BTN5", "down"),
    (0x0200, "BTN5", "up"),
]
RI_MOUSE_WHEEL = 0x0400
RI_MOUSE_HWHEEL = 0x0800

MOUSE_MOVE_ABSOLUTE = 0x01
MOUSE_VIRTUAL_DESKTOP = 0x02

VK_ESCAPE = 0x1B

# ---------------------------------------------------------------- structs


class RAWINPUTDEVICELIST(C.Structure):
    _fields_ = [("hDevice", W.HANDLE), ("dwType", W.DWORD)]


class RID_DEVICE_INFO_MOUSE(C.Structure):
    _fields_ = [
        ("dwId", W.DWORD),
        ("dwNumberOfButtons", W.DWORD),
        ("dwSampleRate", W.DWORD),
        ("fHasHorizontalWheel", W.BOOL),
    ]


class RID_DEVICE_INFO_KEYBOARD(C.Structure):
    _fields_ = [
        ("dwType", W.DWORD),
        ("dwSubType", W.DWORD),
        ("dwKeyboardMode", W.DWORD),
        ("dwNumberOfFunctionKeys", W.DWORD),
        ("dwNumberOfIndicators", W.DWORD),
        ("dwNumberOfKeysTotal", W.DWORD),
    ]


class RID_DEVICE_INFO_HID(C.Structure):
    _fields_ = [
        ("dwVendorId", W.DWORD),
        ("dwProductId", W.DWORD),
        ("dwVersionNumber", W.DWORD),
        ("usUsagePage", W.USHORT),
        ("usUsage", W.USHORT),
    ]


class _RID_UNION(C.Union):
    _fields_ = [
        ("mouse", RID_DEVICE_INFO_MOUSE),
        ("keyboard", RID_DEVICE_INFO_KEYBOARD),
        ("hid", RID_DEVICE_INFO_HID),
    ]


class RID_DEVICE_INFO(C.Structure):
    _anonymous_ = ("u",)
    _fields_ = [("cbSize", W.DWORD), ("dwType", W.DWORD), ("u", _RID_UNION)]


class RAWINPUTDEVICE(C.Structure):
    _fields_ = [
        ("usUsagePage", W.USHORT),
        ("usUsage", W.USHORT),
        ("dwFlags", W.DWORD),
        ("hwndTarget", W.HWND),
    ]


class RAWINPUTHEADER(C.Structure):
    _fields_ = [
        ("dwType", W.DWORD),
        ("dwSize", W.DWORD),
        ("hDevice", W.HANDLE),
        ("wParam", W.WPARAM),
    ]


class RAWMOUSE(C.Structure):
    # usFlags is USHORT, then 2 bytes of padding before the 4-byte-aligned union.
    _fields_ = [
        ("usFlags", W.USHORT),
        ("_pad", W.USHORT),
        ("usButtonFlags", W.USHORT),
        ("usButtonData", C.c_short),  # signed: wheel delta can be negative
        ("ulRawButtons", W.ULONG),
        ("lLastX", W.LONG),
        ("lLastY", W.LONG),
        ("ulExtraInformation", W.ULONG),
    ]


class RAWKEYBOARD(C.Structure):
    _fields_ = [
        ("MakeCode", W.USHORT),
        ("Flags", W.USHORT),
        ("Reserved", W.USHORT),
        ("VKey", W.USHORT),
        ("Message", W.UINT),
        ("ExtraInformation", W.ULONG),
    ]


class RAWHID(C.Structure):
    _fields_ = [("dwSizeHid", W.DWORD), ("dwCount", W.DWORD)]
    # variable-length bRawData follows


# ---------------------------------------------------------------- prototypes

user32.GetRawInputDeviceList.argtypes = [
    C.POINTER(RAWINPUTDEVICELIST), C.POINTER(W.UINT), W.UINT]
user32.GetRawInputDeviceList.restype = W.UINT

user32.GetRawInputDeviceInfoW.argtypes = [
    W.HANDLE, W.UINT, C.c_void_p, C.POINTER(W.UINT)]
user32.GetRawInputDeviceInfoW.restype = W.UINT

user32.RegisterRawInputDevices.argtypes = [
    C.POINTER(RAWINPUTDEVICE), W.UINT, W.UINT]
user32.RegisterRawInputDevices.restype = W.BOOL

user32.GetRawInputData.argtypes = [
    W.HANDLE, W.UINT, C.c_void_p, C.POINTER(W.UINT), W.UINT]
user32.GetRawInputData.restype = W.UINT

# Without explicit argtypes ctypes marshals LPARAM as c_int and overflows on
# x64 pointer-sized values.
user32.DefWindowProcW.argtypes = [W.HWND, W.UINT, W.WPARAM, W.LPARAM]
user32.DefWindowProcW.restype = C.c_longlong
user32.GetMessageW.argtypes = [C.c_void_p, W.HWND, W.UINT, W.UINT]
user32.GetMessageW.restype = W.BOOL
user32.TranslateMessage.argtypes = [C.c_void_p]
user32.DispatchMessageW.argtypes = [C.c_void_p]
user32.DispatchMessageW.restype = C.c_longlong
user32.PostQuitMessage.argtypes = [C.c_int]
kernel32.GetModuleHandleW.argtypes = [W.LPCWSTR]
kernel32.GetModuleHandleW.restype = W.HMODULE
user32.RegisterClassExW.argtypes = [C.c_void_p]
user32.RegisterClassExW.restype = W.WORD
user32.CreateWindowExW.argtypes = [
    W.DWORD, W.LPCWSTR, W.LPCWSTR, W.DWORD,
    C.c_int, C.c_int, C.c_int, C.c_int,
    W.HWND, W.HMENU, W.HINSTANCE, C.c_void_p]
user32.CreateWindowExW.restype = W.HWND

kernel32.QueryPerformanceCounter.argtypes = [C.POINTER(C.c_int64)]
kernel32.QueryPerformanceFrequency.argtypes = [C.POINTER(C.c_int64)]

# ---------------------------------------------------------------- qpc clock

_qpf = C.c_int64()
kernel32.QueryPerformanceFrequency(C.byref(_qpf))
QPF = float(_qpf.value)


def qpc_ms():
    v = C.c_int64()
    kernel32.QueryPerformanceCounter(C.byref(v))
    return v.value * 1000.0 / QPF


# ---------------------------------------------------------------- enumerate

VIDPID_RE = re.compile(r"VID_([0-9A-Fa-f]{4})&PID_([0-9A-Fa-f]{4})")


def device_name(h):
    n = W.UINT(0)
    user32.GetRawInputDeviceInfoW(h, RIDI_DEVICENAME, None, C.byref(n))
    if n.value == 0:
        return ""
    buf = C.create_unicode_buffer(n.value + 1)
    if user32.GetRawInputDeviceInfoW(h, RIDI_DEVICENAME, buf, C.byref(n)) == 0xFFFFFFFF:
        return ""
    return buf.value


def device_info(h):
    info = RID_DEVICE_INFO()
    info.cbSize = C.sizeof(RID_DEVICE_INFO)
    n = W.UINT(C.sizeof(RID_DEVICE_INFO))
    if user32.GetRawInputDeviceInfoW(h, RIDI_DEVICEINFO, C.byref(info), C.byref(n)) == 0xFFFFFFFF:
        return None
    return info


def enumerate_devices():
    n = W.UINT(0)
    user32.GetRawInputDeviceList(None, C.byref(n), C.sizeof(RAWINPUTDEVICELIST))
    if n.value == 0:
        return []
    arr = (RAWINPUTDEVICELIST * n.value)()
    got = user32.GetRawInputDeviceList(arr, C.byref(n), C.sizeof(RAWINPUTDEVICELIST))
    if got == 0xFFFFFFFF:
        raise C.WinError(C.get_last_error())

    out = []
    for i in range(got):
        h = arr[i].hDevice
        t = arr[i].dwType
        path = device_name(h)
        m = VIDPID_RE.search(path or "")
        rec = {
            "handle": int(h) if h else 0,
            "type": TYPE_NAME.get(t, f"HID?{t}"),
            "path": path,
            "vid": m.group(1).upper() if m else None,
            "pid": m.group(2).upper() if m else None,
        }
        info = device_info(h)
        if info is not None:
            if t == RIM_TYPEMOUSE:
                rec["buttons"] = info.mouse.dwNumberOfButtons
                rec["sample_rate_hz"] = info.mouse.dwSampleRate
                rec["hwheel"] = bool(info.mouse.fHasHorizontalWheel)
            elif t == RIM_TYPEKEYBOARD:
                rec["keys_total"] = info.keyboard.dwNumberOfKeysTotal
                rec["kbd_type"] = info.keyboard.dwType
            elif t >= RIM_TYPEHID:
                rec["usage_page"] = f"0x{info.hid.usUsagePage:04X}"
                rec["usage"] = f"0x{info.hid.usUsage:04X}"
                rec["hid_vid"] = f"0x{info.hid.dwVendorId:04X}"
                rec["hid_pid"] = f"0x{info.hid.dwProductId:04X}"
        out.append(rec)
    return out


# ---------------------------------------------------------------- state

DEVICES = {}          # handle -> record
SHORT = {}            # handle -> short label
events = []           # full JSON log
press_gaps = defaultdict(list)   # (label, button) -> [release->press ms]
click_durs = defaultdict(list)   # (label, button) -> [press->release ms]
last_up = {}          # (label, button) -> qpc ms
last_down = {}        # (label, button) -> qpc ms
btn_counts = defaultdict(int)
motion_count = defaultdict(int)
motion_window_start = qpc_ms()
seen_channels = set()
quit_flag = [False]


def label_for(h):
    h = int(h) if h else 0
    if h in SHORT:
        return SHORT[h]
    rec = DEVICES.get(h)
    if rec:
        vp = f"{rec['vid']}:{rec['pid']}" if rec.get("vid") else "????:????"
        lbl = f"{rec['type'][:3]}#{len(SHORT)} {vp}"
    else:
        lbl = f"UNK#{len(SHORT)} h={h:#x}"
    SHORT[h] = lbl
    return lbl


def note(kind, label, detail):
    seen_channels.add((label, kind, detail))


# ---------------------------------------------------------------- wndproc

WNDPROC = C.WINFUNCTYPE(C.c_longlong, W.HWND, W.UINT, W.WPARAM, W.LPARAM)
_buf = (C.c_ubyte * 4096)()


def handle_rawinput(lparam):
    size = W.UINT(C.sizeof(_buf))
    got = user32.GetRawInputData(
        W.HANDLE(lparam), RID_INPUT, C.byref(_buf),
        C.byref(size), C.sizeof(RAWINPUTHEADER))
    if got == 0xFFFFFFFF or got == 0:
        return

    hdr = C.cast(_buf, C.POINTER(RAWINPUTHEADER)).contents
    body = C.addressof(_buf) + C.sizeof(RAWINPUTHEADER)
    lbl = label_for(hdr.hDevice)
    t = qpc_ms()

    if hdr.dwType == RIM_TYPEMOUSE:
        m = C.cast(body, C.POINTER(RAWMOUSE)).contents
        bf = m.usButtonFlags

        if bf == 0 and (m.lLastX or m.lLastY):
            motion_count[lbl] += 1
            return

        for mask, name, edge in BTN_FLAGS:
            if bf & mask:
                note("mouse-button", lbl, name)
                key = (lbl, name)
                if edge == "down":
                    btn_counts[key] += 1
                    gap = t - last_up[key] if key in last_up else None
                    if gap is not None:
                        press_gaps[key].append(gap)
                    last_down[key] = t
                    g = f"  gap-since-release {gap:8.3f} ms" if gap is not None else ""
                    print(f"[{t:12.3f}] {lbl:24} {name:11} DOWN{g}")
                else:
                    dur = t - last_down[key] if key in last_down else None
                    if dur is not None:
                        click_durs[key].append(dur)
                    last_up[key] = t
                    d = f"  held {dur:8.3f} ms" if dur is not None else ""
                    print(f"[{t:12.3f}] {lbl:24} {name:11} UP  {d}")
                events.append({"t": t, "dev": lbl, "ch": "mouse",
                               "btn": name, "edge": edge})

        if bf & RI_MOUSE_WHEEL:
            note("mouse-wheel", lbl, "vertical")
            print(f"[{t:12.3f}] {lbl:24} WHEEL       delta={m.usButtonData}")
            events.append({"t": t, "dev": lbl, "ch": "wheel",
                           "delta": m.usButtonData})
        if bf & RI_MOUSE_HWHEEL:
            note("mouse-wheel", lbl, "horizontal")
            print(f"[{t:12.3f}] {lbl:24} HWHEEL      delta={m.usButtonData}")
            events.append({"t": t, "dev": lbl, "ch": "hwheel",
                           "delta": m.usButtonData})

    elif hdr.dwType == RIM_TYPEKEYBOARD:
        k = C.cast(body, C.POINTER(RAWKEYBOARD)).contents
        edge = "up" if (k.Flags & 0x01) else "down"
        if k.VKey == VK_ESCAPE and edge == "down":
            quit_flag[0] = True
            user32.PostQuitMessage(0)
            return
        note("keyboard", lbl, f"VK=0x{k.VKey:02X} scan=0x{k.MakeCode:02X}")
        print(f"[{t:12.3f}] {lbl:24} KEY VK=0x{k.VKey:02X} "
              f"scan=0x{k.MakeCode:02X} {edge.upper()}")
        events.append({"t": t, "dev": lbl, "ch": "keyboard",
                       "vk": k.VKey, "scan": k.MakeCode, "edge": edge})

    elif hdr.dwType >= RIM_TYPEHID:
        h = C.cast(body, C.POINTER(RAWHID)).contents
        raw = bytes(
            (C.c_ubyte * (h.dwSizeHid * h.dwCount)).from_address(
                body + C.sizeof(RAWHID)))
        hx = raw.hex(" ")
        rec = DEVICES.get(int(hdr.hDevice) if hdr.hDevice else 0, {})
        up = rec.get("usage_page", "?")
        us = rec.get("usage", "?")
        note("hid-report", lbl, f"page={up} usage={us}")
        print(f"[{t:12.3f}] {lbl:24} HID page={up} usage={us} : {hx}")
        events.append({"t": t, "dev": lbl, "ch": "hid",
                       "page": up, "usage": us, "report": hx})


def wndproc(hwnd, msg, wparam, lparam):
    if msg == WM_INPUT:
        try:
            handle_rawinput(lparam)
        except Exception as e:  # never let a bad report kill the pump
            print(f"  !! {type(e).__name__}: {e}")
        return 0
    if msg == WM_DESTROY:
        user32.PostQuitMessage(0)
        return 0
    return user32.DefWindowProcW(hwnd, msg, wparam, lparam)


_wndproc_ref = WNDPROC(wndproc)


class WNDCLASSEX(C.Structure):
    _fields_ = [
        ("cbSize", W.UINT), ("style", W.UINT), ("lpfnWndProc", WNDPROC),
        ("cbClsExtra", C.c_int), ("cbWndExtra", C.c_int),
        ("hInstance", W.HINSTANCE), ("hIcon", W.HICON),
        ("hCursor", W.HANDLE), ("hbrBackground", W.HBRUSH),
        ("lpszMenuName", W.LPCWSTR), ("lpszClassName", W.LPCWSTR),
        ("hIconSm", W.HICON),
    ]


_PTR_MASK = (1 << (8 * C.sizeof(C.c_void_p))) - 1


def _as_handle(v):
    """HWND is c_void_p, which rejects negative ints -- reinterpret as unsigned."""
    return W.HWND(v & _PTR_MASK)


def make_message_window():
    wc = WNDCLASSEX()
    wc.cbSize = C.sizeof(WNDCLASSEX)
    wc.lpfnWndProc = _wndproc_ref
    wc.hInstance = kernel32.GetModuleHandleW(None)
    wc.lpszClassName = "HidForgeProbe"
    if not user32.RegisterClassExW(C.byref(wc)):
        raise C.WinError(C.get_last_error())
    user32.CreateWindowExW.restype = W.HWND
    hwnd = user32.CreateWindowExW(
        0, "HidForgeProbe", "HidForgeProbe", 0, 0, 0, 0, 0,
        _as_handle(HWND_MESSAGE), None, wc.hInstance, None)
    if not hwnd:
        raise C.WinError(C.get_last_error())
    return hwnd


# ---------------------------------------------------------------- register


def register(hwnd):
    """Register mouse + keyboard + consumer page, plus every HID collection we
    enumerated. The last part is what catches vendor-specific side buttons."""
    wanted = [(0x01, 0x02), (0x01, 0x06), (0x01, 0x01), (0x0C, 0x01)]
    for rec in DEVICES.values():
        if rec["type"].startswith("HID") and rec.get("usage_page"):
            pair = (int(rec["usage_page"], 16), int(rec["usage"], 16))
            if pair[0] == 0:  # type-3 collections report page 0; not registrable
                continue
            if pair not in wanted:
                wanted.append(pair)

    ok, failed = [], []
    for page, usage in wanted:
        rid = RAWINPUTDEVICE(page, usage, RIDEV_INPUTSINK, hwnd)
        if user32.RegisterRawInputDevices(C.byref(rid), 1, C.sizeof(RAWINPUTDEVICE)):
            ok.append((page, usage))
        else:
            failed.append((page, usage, C.get_last_error()))
    return ok, failed


# ---------------------------------------------------------------- summary


def pct(xs, p):
    if not xs:
        return None
    s = sorted(xs)
    i = max(0, min(len(s) - 1, int(round((p / 100.0) * (len(s) - 1)))))
    return s[i]


def summarize():
    print("\n" + "=" * 78)
    print("PER-BUTTON TIMING  (all values ms, measured via QueryPerformanceCounter)")
    print("=" * 78)
    if not btn_counts:
        print("  no button events captured")
    hdr = f"{'device / button':38} {'n':>4} {'min gap':>9} {'p5 gap':>9} {'med gap':>9} {'min held':>9}"
    print(hdr)
    print("-" * len(hdr))
    for key in sorted(btn_counts, key=lambda k: (k[0], k[1])):
        g = press_gaps.get(key, [])
        d = click_durs.get(key, [])
        def f(v):
            return f"{v:9.3f}" if v is not None else f"{'-':>9}"
        print(f"{key[0] + ' / ' + key[1]:38} {btn_counts[key]:4} "
              f"{f(min(g) if g else None)} {f(pct(g,5))} {f(pct(g,50))} "
              f"{f(min(d) if d else None)}")

    print("\n'min gap' = shortest release->press interval the OS ever saw for that")
    print("button. That is the FLOOR imposed by firmware debounce + poll interval.")
    print("No software can go below it -- the events simply never left the mouse.\n")

    elapsed = (qpc_ms() - motion_window_start) / 1000.0
    if motion_count:
        print("=" * 78)
        print(f"MOTION REPORT RATE  (over {elapsed:.1f} s)")
        print("=" * 78)
        for lbl, n in sorted(motion_count.items()):
            print(f"  {lbl:28} {n:7} reports   ~{n/max(elapsed,1e-9):8.1f} Hz")
        print("\nObserved Hz approximates the real USB poll rate. Button-timing")
        print("resolution can never be finer than 1000/Hz ms.\n")

    print("=" * 78)
    print("CHANNELS THAT ACTUALLY FIRED  (this is the 'works on any mouse' answer)")
    print("=" * 78)
    by_dev = defaultdict(set)
    for lbl, kind, detail in seen_channels:
        by_dev[lbl].add(f"{kind}: {detail}")
    for lbl in sorted(by_dev):
        print(f"  {lbl}")
        for d in sorted(by_dev[lbl]):
            print(f"      - {d}")

    out = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                       "probe_log.json")
    with open(out, "w", encoding="utf-8") as fh:
        json.dump({
            "qpf": QPF,
            "devices": list(DEVICES.values()),
            "events": events,
            "timing": {
                f"{k[0]}|{k[1]}": {
                    "downs": btn_counts[k],
                    "release_to_press_ms": press_gaps.get(k, []),
                    "held_ms": click_durs.get(k, []),
                } for k in btn_counts
            },
            "motion_reports": dict(motion_count),
            "elapsed_s": elapsed,
        }, fh, indent=2)
    print(f"\nwrote {out}")


# ---------------------------------------------------------------- main


def main():
    if sys.platform != "win32":
        sys.exit("windows only")

    print(f"python {sys.version.split()[0]}  {8 * C.sizeof(C.c_void_p)}-bit  "
          f"QPF={QPF:.0f} Hz  (tick = {1e6/QPF:.3f} us)\n")

    print("=" * 78)
    print("ATTACHED RAW INPUT DEVICES")
    print("=" * 78)
    for rec in enumerate_devices():
        DEVICES[rec["handle"]] = rec
        extra = {k: v for k, v in rec.items()
                 if k not in ("handle", "type", "path", "vid", "pid")}
        vp = f"{rec['vid']}:{rec['pid']}" if rec.get("vid") else "-"
        print(f"\n  [{rec['type']:8}] h={rec['handle']:#x}  vid:pid={vp}")
        print(f"      {rec['path']}")
        if extra:
            print(f"      {extra}")

    hwnd = make_message_window()
    ok, failed = register(hwnd)
    print("\n" + "=" * 78)
    print(f"REGISTERED {len(ok)} usage pair(s): "
          + ", ".join(f"{p:#04x}/{u:#04x}" for p, u in ok))
    for p, u, err in failed:
        print(f"  FAILED {p:#04x}/{u:#04x} -> win32 error {err}")
    print("=" * 78)
    print("""
NOW DO ALL OF THIS, in order:

  1. Press EVERY button on the mouse, one at a time, slowly. Include side
     buttons, DPI/sniper button, wheel tilt, wheel click. Note which ones
     produce NO line at all -- those are the ones firmware is swallowing.
  2. Scroll the wheel up and down a few notches.
  3. Move the mouse around for ~5 seconds (motion is counted, not printed).
  4. Click the left button AS FAST AS YOU PHYSICALLY CAN for ~10 seconds.
     This is what measures the firmware debounce floor.
  5. If any button on any mouse double-fires on a single press, spam that
     button too -- the gap histogram will show the chatter.

Press ESC when done.
""")
    print("-" * 78)

    msg = W.MSG()
    while user32.GetMessageW(C.byref(msg), None, 0, 0) > 0:
        user32.TranslateMessage(C.byref(msg))
        user32.DispatchMessageW(C.byref(msg))

    summarize()


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        summarize()
