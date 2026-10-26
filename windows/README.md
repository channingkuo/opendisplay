# OpenDisplay Receiver for Windows (beta)

`OpenDisplay Receiver.exe` turns a Windows PC's screen into an OpenDisplay display:
a Mac (or a Linux desktop) extends onto it like onto an iPad. It speaks the
same protocol as every other receiver ([PROTOCOL.md](../PROTOCOL.md)), so
the Mac sender needs no change: the PC shows up in its receiver list.

It is one self-contained `.exe`: a Win32 window, the **Microsoft H.264 decoder
through Media Foundation** (on the GPU via DXVA when the driver supports it,
otherwise on the CPU) and **Direct3D 11** for scaling and colour conversion.
No GStreamer, no codec pack, no runtime to install. Like the sender, the
stream is **not encrypted**: use it on a network you trust.

The exe has the Mac receiver's name and app icon. It is a GUI program: started
from Explorer it opens no console and writes its log to
`%LOCALAPPDATA%\opendisplay\receiver.log` (a fatal error also shows in a
message box); started from a terminal it prints the log there, so
`--help`, `--list-monitors` and `set RUST_LOG=debug` work as usual.

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

1. Double-click `OpenDisplay Receiver.exe` (no console window opens). It covers the
   primary monitor with a black window and advertises itself over Bonjour.
2. Allow it through the **Windows Firewall** on private networks when asked
   (see below).
3. On the Mac, pick the PC in the OpenDisplay menu. The virtual display appears
   as a second monitor sized for this PC's screen.
4. Click and drag on the PC's window to click and drag on the Mac; the mouse
   wheel / touchpad scrolls. The pointer you see is the Mac's, drawn where the Mac says it is, so it also moves when you use the Mac's own mouse.

The receiver opens in a 1920x1080 window (smaller on small monitors) showing
"waiting for connection" until a Mac connects. Keys: **F11**, **Ctrl+F** or
**Alt+Enter** toggles fullscreen, **Esc** leaves fullscreen,
**Alt+F4** or closing the window quits (the sender is told).

```
"OpenDisplay Receiver.exe" --list-monitors        # what Windows reports
"OpenDisplay Receiver.exe" --monitor 2            # cover the second monitor (or --monitor \\.\DISPLAY2)
"OpenDisplay Receiver.exe" --fullscreen           # start fullscreen instead of in a window
"OpenDisplay Receiver.exe" --software-decode      # never use the GPU decoder
"OpenDisplay Receiver.exe" --name "Desk PC"       # name shown in the sender's list
"OpenDisplay Receiver.exe" --listen 0.0.0.0:9000  # port (the cursor channel uses port+1)
"OpenDisplay Receiver.exe" --width 2560 --height 1440 --scale 1.5 --refresh 144   # override the panel facts
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
| Cursor | the sender's sprite is drawn at the position the sender reports, in a transparent click-through overlay window (per-pixel alpha); the local pointer is hidden over the window |

Panel facts in `hello` come from the monitor: physical pixels, DPI scale
(`GetDpiForMonitor`) and refresh rate. Only H.264 is offered; HEVC would need
the optional HEVC video extension and is not used.

## Build

Needs Rust (MSVC toolchain) and the Windows SDK (Visual Studio Build Tools, for
`rc.exe`, which embeds the icon).

```
cd windows
.\build.ps1                  # dist\OpenDisplay Receiver.exe
cargo build --release        # the same binary, as target\release\od-receiver-win.exe
```

Cargo cannot name a binary with a space, so `build.ps1` copies it to the
Mac receiver's name. The icon is `assets\OpenDisplay Receiver.ico`, made from
`MacReceiver/Assets.xcassets/AppIcon.appiconset`. This crate is separate from
the Linux workspace; it reuses `linux/crates/od-receiver` (built without
GStreamer/GTK) for the network and session code. The portable parts (`hello`
construction, cursor sprite scaling) are unit-tested on every platform, and
`cargo check --target x86_64-pc-windows-msvc` works from Linux and macOS too.

## Known limits

* H.264 only; no HEVC, no audio, no Apple Pencil pressure (a plain pointer).
* Only the left mouse button is sent (as touch); right click / keyboard are not forwarded.
* After a GPU reset or driver update the receiver must be restarted.
* The cursor is a separate overlay window, so a capture of the PC's screen
  (or a window manager that hides topmost tool windows) may not include it.
