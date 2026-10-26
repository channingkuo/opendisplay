# OpenDisplay receiver for Windows (beta)

`od-receiver-win.exe` turns a Windows PC's screen into an OpenDisplay display:
a Mac (or a Linux desktop) extends onto it like onto an iPad. It speaks the
same protocol as every other receiver ([PROTOCOL.md](../../../PROTOCOL.md)), so
the Mac sender needs no change: the PC shows up in its receiver list.

It is one self-contained `.exe`: a Win32 window, the **Microsoft H.264 decoder
through Media Foundation** (on the GPU via DXVA when the driver supports it,
otherwise on the CPU) and **Direct3D 11** for scaling and colour conversion.
No GStreamer, no codec pack, no runtime to install. Like the sender, the
stream is **not encrypted**: use it on a network you trust.

> **Status:** written against the protocol and the Windows APIs and checked on
> CI for build and lint, but it has had little time on real hardware. Reports
> are welcome: please include the log (`set RUST_LOG=debug`, then run from a
> terminal), the GPU and the Windows version.

## Requirements

* Windows 10 or 11, x86_64. (Windows N/KN editions need the Media Feature Pack
  for the H.264 decoder.)
* A GPU with a WDDM 2.0+ driver (any current Intel, AMD or NVIDIA part).
* A Mac running OpenDisplay (any recent release) on the same network.

## Use

1. Run `od-receiver-win.exe` from a terminal (or double-click it). It covers the
   primary monitor with a black window and advertises itself over Bonjour.
2. Allow it through the **Windows Firewall** on private networks when asked
   (see below).
3. On the Mac, pick the PC in the OpenDisplay menu. The virtual display appears
   as a second monitor sized for this PC's screen.
4. Click and drag on the PC's window to click and drag on the Mac; the mouse
   wheel / touchpad scrolls. The pointer you see is the Mac's.

Keys: **F11** or **Alt+Enter** toggles fullscreen, **Esc** leaves fullscreen,
**Alt+F4** or closing the window quits (the sender is told).

```
od-receiver-win.exe --list-monitors        # what Windows reports
od-receiver-win.exe --monitor 2            # cover the second monitor (or --monitor \\.\DISPLAY2)
od-receiver-win.exe --windowed             # a normal window instead of fullscreen
od-receiver-win.exe --software-decode      # never use the GPU decoder
od-receiver-win.exe --name "Desk PC"       # name shown in the sender's list
od-receiver-win.exe --listen 0.0.0.0:9000  # port (the cursor channel uses port+1)
od-receiver-win.exe --width 2560 --height 1440 --scale 1.5 --refresh 144   # override the panel facts
```

If the Mac cannot see the PC (guest networks and some routers block mDNS), run
the sender against the address directly, e.g. `-host 192.168.1.20 -port 9000`
(see the Mac sender's manual endpoint option).

## Firewall

The first run triggers a Windows Defender Firewall prompt: allow **private**
networks. Or add the rules once, from an elevated terminal:

```
netsh advfirewall firewall add rule name="OpenDisplay Receiver (TCP)" dir=in action=allow protocol=TCP localport=9000 profile=private
netsh advfirewall firewall add rule name="OpenDisplay Receiver (cursor UDP)" dir=in action=allow protocol=UDP localport=9001 profile=private
netsh advfirewall firewall add rule name="OpenDisplay Receiver (mDNS)" dir=in action=allow protocol=UDP localport=5353 profile=private
```

## How it works

| | |
|---|---|
| Network, session, Bonjour, stats, keyframe recovery | the shared `od-receiver` library (the Linux receiver's), over `opendisplay-proto` / `opendisplay-session` |
| Window, input, sleep | Win32. Pointer moves are coalesced per message batch; `WM_POWERBROADCAST` suspend sends `sleeping` |
| Decode | `CLSID_CMSH264DecoderMFT` with a DXGI device manager (DXVA), low-latency mode, NV12 out; software fallback |
| Present | D3D11 video processor (NV12 to RGB, scale, letterbox) into a flip-model swap chain, no vsync wait |
| Colours | BT.709/BT.601 and range taken from the decoder's output type (the SPS VUI) |
| Cursor | the sender's sprite is turned into the window's own pointer (`CreateIconIndirect`), so it follows the mouse with no extra latency |

Panel facts in `hello` come from the monitor: physical pixels, DPI scale
(`GetDpiForMonitor`) and refresh rate. Only H.264 is offered; HEVC would need
the optional HEVC video extension and is not used.

## Build

```
cd linux
cargo build --release -p od-receiver-win     # target\release\od-receiver-win.exe
```

Needs Rust (MSVC toolchain). The portable parts (`hello` construction, cursor
sprite scaling) are unit-tested on every platform; `cargo check --target
x86_64-pc-windows-msvc -p od-receiver-win` works from Linux and macOS too.

## Known limits

* H.264 only; no HEVC, no audio, no Apple Pencil pressure (a plain pointer).
* Only the left mouse button is sent (as touch); right click / keyboard are not forwarded.
* After a GPU reset or driver update the receiver must be restarted.
* The sender's cursor *position* is not drawn separately: the pointer shown is
  the sender's sprite at your mouse position (the sender follows your mouse).
