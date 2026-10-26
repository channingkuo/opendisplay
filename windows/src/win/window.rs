//! The Win32 window: owns the swap chain's HWND, turns mouse input into
//! touch/scroll (§6.1), shows the sender's cursor as the window's pointer
//! (§6.2) and tells the sender when the machine sleeps (§6.1 `sleeping`).
//!
//! It runs on the thread that creates it (the process's main thread); the
//! receiver loop lives on another thread and reaches it through [`Shared`]
//! and `PostMessage`.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use od_receiver::input::{ScrollUnit, UserInput, scroll_to_video};
use od_receiver::power::PowerEvent;
use opendisplay_proto::control::TouchPhase;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use super::gpu::Presenter;
use super::pointer;
use crate::sprite::Sprite;

/// Posted by the receiver thread: the sender's cursor sprite or visibility
/// changed.
const WM_OD_CURSOR: u32 = WM_APP + 1;
/// Posted when the receiver loop has ended: close the window and leave.
const WM_OD_QUIT: u32 = WM_APP + 2;

const PBT_APMSUSPEND: usize = 0x0004;

/// State both threads touch.
pub struct Shared {
    pub presenter: Mutex<Presenter>,
    hwnd: AtomicIsize,
    cursor: Mutex<Option<Sprite>>,
    /// Bumped on every new sprite.
    sprite_gen: AtomicU64,
    /// Last reported cursor position, normalised to the video.
    cursor_pos: Mutex<Option<(f64, f64)>>,
    cursor_visible: AtomicBool,
    /// A `WM_OD_CURSOR` is already queued: coalesce the 120 Hz updates.
    cursor_dirty: AtomicBool,
}

// The presenter's Direct3D objects are free-threaded and only used under the
// mutex; the other fields are atomics and mutexes.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    pub fn new(presenter: Presenter) -> Arc<Shared> {
        Arc::new(Shared {
            presenter: Mutex::new(presenter),
            hwnd: AtomicIsize::new(0),
            cursor: Mutex::new(None),
            sprite_gen: AtomicU64::new(0),
            cursor_pos: Mutex::new(None),
            cursor_visible: AtomicBool::new(true),
            cursor_dirty: AtomicBool::new(false),
        })
    }

    fn hwnd(&self) -> HWND {
        HWND(self.hwnd.load(Ordering::Acquire) as *mut _)
    }

    /// New cursor sprite (receiver thread).
    pub fn set_sprite(&self, s: Sprite) {
        *self.cursor.lock().unwrap() = Some(s);
        self.sprite_gen.fetch_add(1, Ordering::AcqRel);
        self.notify_cursor();
    }

    /// The sender's cursor moved or was shown/hidden (receiver thread).
    pub fn set_cursor(&self, x: Option<f64>, y: Option<f64>, visible: bool) {
        if let (Some(x), Some(y)) = (x, y) {
            *self.cursor_pos.lock().unwrap() = Some((x, y));
        }
        self.cursor_visible.store(visible, Ordering::Release);
        self.notify_cursor();
    }

    fn notify_cursor(&self) {
        if self.cursor_dirty.swap(true, Ordering::AcqRel) {
            return;
        }
        let h = self.hwnd();
        if !h.0.is_null() {
            unsafe {
                let _ = PostMessageW(Some(h), WM_OD_CURSOR, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// The receiver loop ended: close the window and end the message loop.
    pub fn quit(&self) {
        let h = self.hwnd();
        if !h.0.is_null() {
            unsafe {
                let _ = PostMessageW(Some(h), WM_OD_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// Client size of the window at startup (physical pixels).
const DEFAULT_CLIENT: (i32, i32) = (1920, 1080);

pub struct WindowOptions {
    /// Top-left and size of the monitor to cover when fullscreen.
    pub monitor_origin: (i32, i32),
    pub monitor_size: (u32, u32),
    pub windowed: bool,
}

/// Window-thread state, behind `GWLP_USERDATA`.
struct State {
    shared: Option<Arc<Shared>>,
    input: UnboundedSender<UserInput>,
    power: UnboundedSender<PowerEvent>,
    on_close: Box<dyn Fn()>,
    pressed: bool,
    pending_move: Option<(i32, i32)>,
    /// Where the real mouse last was. Windows re-sends `WM_MOUSEMOVE` with
    /// the same coordinates whenever a window moves under the pointer (our
    /// cursor overlay does, constantly); treating those as motion would drag
    /// the sender's cursor back to this spot.
    last_mouse: Option<(i32, i32)>,
    /// The windowed state to return to from fullscreen.
    fullscreen: Option<Saved>,
    overlay: Option<HWND>,
    overlay_shown: bool,
    image: Option<pointer::Image>,
    /// What `image` was rendered from: sprite generation and video size.
    image_for: (u64, (u32, u32)),
    sprite: Option<Sprite>,
    sprite_gen: u64,
}

struct Saved {
    style: isize,
    placement: WINDOWPLACEMENT,
}

/// Register the class and create the window, not yet shown.
pub fn create(
    opts: &WindowOptions,
    input: UnboundedSender<UserInput>,
    power: UnboundedSender<PowerEvent>,
    on_close: impl Fn() + 'static,
) -> Result<(HWND, *mut ())> {
    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let class = w!("OpenDisplayReceiver");
        // MAKEINTRESOURCE(1): the exe's icon resource (see build.rs).
        #[allow(clippy::manual_dangling_ptr)]
        let icon_id = PCWSTR(1 as *const u16);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            // The exe's embedded icon (resource id 1, see build.rs), if any.
            hIcon: LoadIconW(Some(hinstance.into()), icon_id).unwrap_or_default(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            style: CS_HREDRAW | CS_VREDRAW,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(anyhow!("RegisterClassW failed"));
        }
        let state = Box::into_raw(Box::new(State {
            shared: None,
            input,
            power,
            on_close: Box::new(on_close),
            pressed: false,
            pending_move: None,
            last_mouse: None,
            fullscreen: None,
            overlay: None,
            overlay_shown: false,
            image: None,
            image_for: (0, (0, 0)),
            sprite: None,
            sprite_gen: 0,
        }));
        let (style, x, y, w, h) = if opts.windowed {
            // A DEFAULT_CLIENT-sized client area, shrunk to fit small
            // monitors, centred on the chosen monitor.
            let (mw, mh) = (opts.monitor_size.0 as i32, opts.monitor_size.1 as i32);
            let client = (
                DEFAULT_CLIENT.0.min(mw * 9 / 10),
                DEFAULT_CLIENT.1.min(mh * 9 / 10),
            );
            let mut r = RECT {
                left: 0,
                top: 0,
                right: client.0,
                bottom: client.1,
            };
            let _ = AdjustWindowRectExForDpi(
                &mut r,
                WS_OVERLAPPEDWINDOW,
                false,
                WINDOW_EX_STYLE(0),
                GetDpiForSystem(),
            );
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            (
                WS_OVERLAPPEDWINDOW,
                opts.monitor_origin.0 + (mw - w) / 2,
                opts.monitor_origin.1 + (mh - h) / 2,
                w,
                h,
            )
        } else {
            (
                WS_POPUP,
                opts.monitor_origin.0,
                opts.monitor_origin.1,
                opts.monitor_size.0 as i32,
                opts.monitor_size.1 as i32,
            )
        };
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("OpenDisplay Receiver"),
            style,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinstance.into()),
            Some(state as *const _),
        )?;
        (*state).overlay = pointer::create_window(hwnd);
        if !opts.windowed {
            (*state).fullscreen = Some(Saved {
                style: WS_OVERLAPPEDWINDOW.0 as isize,
                placement: WINDOWPLACEMENT::default(),
            });
        }
        Ok((hwnd, state as *mut ()))
    }
}

/// Client size in pixels.
pub fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut r = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut r);
    }
    (
        (r.right - r.left).max(1) as u32,
        (r.bottom - r.top).max(1) as u32,
    )
}

/// Attach the shared state, show the window and run the message loop until
/// [`Shared::quit`] (or the user closes the window after the receiver ends).
pub fn run(hwnd: HWND, state: *mut (), shared: Arc<Shared>) {
    let state = state as *mut State;
    unsafe {
        (*state).shared = Some(shared.clone());
        shared.hwnd.store(hwnd.0 as isize, Ordering::Release);
        // Keep the display on while this window shows a stream.
        SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let mut msg = MSG::default();
        loop {
            // Handle everything queued, then send one coalesced pointer move:
            // a 1000 Hz mouse would otherwise flood the connection.
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    SetThreadExecutionState(ES_CONTINUOUS);
                    return;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            flush_move(&mut *state, hwnd);
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                SetThreadExecutionState(ES_CONTINUOUS);
                return;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

fn xy(lparam: LPARAM) -> (i32, i32) {
    (
        (lparam.0 & 0xFFFF) as u16 as i16 as i32,
        ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32,
    )
}

fn touch(st: &State, phase: TouchPhase, px: i32, py: i32) {
    let Some(shared) = &st.shared else { return };
    let rect = shared.presenter.lock().unwrap().video_rect();
    if let Some(rect) = rect {
        let (x, y) = rect.normalise(px as f64, py as f64);
        let _ = st.input.send(UserInput::Touch {
            phase,
            x,
            y,
            local_ms: now_ms(),
        });
    }
}

fn flush_move(st: &mut State, _hwnd: HWND) {
    if let Some((x, y)) = st.pending_move.take() {
        touch(st, TouchPhase::Moved, x, y);
    }
}

unsafe fn state<'a>(hwnd: HWND) -> Option<&'a mut State> {
    let p = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut State;
    unsafe { p.as_mut() }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let Some(st) = state(hwnd) else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        match msg {
            WM_ERASEBKGND => return LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
                return LRESULT(0);
            }
            WM_SIZE => {
                if wp.0 as u32 != SIZE_MINIMIZED {
                    let size = ((lp.0 & 0xFFFF) as u32, ((lp.0 >> 16) & 0xFFFF) as u32);
                    if let Some(shared) = &st.shared {
                        if let Err(e) = shared.presenter.lock().unwrap().resize(size) {
                            warn!("resize: {e:#}");
                        }
                    }
                }
                update_overlay(st, hwnd);
                return LRESULT(0);
            }
            WM_MOVE | WM_SHOWWINDOW => {
                update_overlay(st, hwnd);
            }
            WM_MOUSEMOVE => {
                let p = xy(lp);
                if st.last_mouse != Some(p) {
                    st.last_mouse = Some(p);
                    st.pending_move = Some(p);
                }
                return LRESULT(0);
            }
            WM_LBUTTONDOWN => {
                flush_move(st, hwnd);
                let (x, y) = xy(lp);
                st.last_mouse = Some((x, y));
                st.pressed = true;
                SetCapture(hwnd);
                touch(st, TouchPhase::Began, x, y);
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                flush_move(st, hwnd);
                let (x, y) = xy(lp);
                st.last_mouse = Some((x, y));
                if st.pressed {
                    st.pressed = false;
                    let _ = ReleaseCapture();
                    touch(st, TouchPhase::Ended, x, y);
                }
                return LRESULT(0);
            }
            WM_CAPTURECHANGED => {
                // Capture taken away mid-press (another window, Alt+Tab).
                if st.pressed {
                    st.pressed = false;
                    let mut p = POINT::default();
                    let _ = GetCursorPos(&mut p);
                    let _ = ScreenToClient(hwnd, &mut p);
                    touch(st, TouchPhase::Cancelled, p.x, p.y);
                }
                return LRESULT(0);
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let notches = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as f64 / 120.0;
                if let Some(shared) = &st.shared {
                    let p = shared.presenter.lock().unwrap();
                    if let (Some(rect), Some(v)) = (p.video_rect(), p.video_size()) {
                        // Windows: wheel forward / right is positive. The
                        // toolkit convention `scroll_to_video` takes is
                        // "down / right is positive" for the vertical axis.
                        let (dx, dy) = if msg == WM_MOUSEWHEEL {
                            (0.0, -notches)
                        } else {
                            (notches, 0.0)
                        };
                        let (dx, dy) =
                            scroll_to_video(dx, dy, ScrollUnit::Wheel, &rect, v.1 as f64);
                        let _ = st.input.send(UserInput::Scroll { dx, dy });
                    }
                }
                return LRESULT(0);
            }
            WM_SETCURSOR => {
                if (lp.0 & 0xFFFF) as u32 == HTCLIENT {
                    // The sender's cursor is drawn by the overlay; hide ours
                    // once there is one to show instead.
                    if st.sprite.is_some() {
                        SetCursor(None);
                    } else if let Ok(arrow) = LoadCursorW(None, IDC_ARROW) {
                        SetCursor(Some(arrow));
                    }
                    return LRESULT(1);
                }
            }
            WM_OD_CURSOR => {
                if let Some(shared) = &st.shared {
                    shared.cursor_dirty.store(false, Ordering::Release);
                }
                update_overlay(st, hwnd);
                return LRESULT(0);
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let vk = VIRTUAL_KEY(wp.0 as u16);
                let alt = msg == WM_SYSKEYDOWN;
                let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
                if vk == VK_F11 || (alt && vk == VK_RETURN) || (ctrl && vk == VK_F) {
                    toggle_fullscreen(hwnd, st);
                    return LRESULT(0);
                }
                if vk == VK_ESCAPE && is_fullscreen(hwnd) {
                    toggle_fullscreen(hwnd, st);
                    return LRESULT(0);
                }
            }
            WM_POWERBROADCAST => {
                if wp.0 == PBT_APMSUSPEND {
                    info!("system suspending: telling the sender");
                    let (done_tx, done_rx) = std::sync::mpsc::channel();
                    if st
                        .power
                        .send(PowerEvent::Sleeping {
                            done: Some(done_tx),
                        })
                        .is_ok()
                    {
                        // Windows gives a suspend handler about two seconds.
                        let _ = done_rx.recv_timeout(Duration::from_millis(1500));
                    }
                }
                return LRESULT(1);
            }
            WM_CLOSE => {
                info!("window closed");
                (st.on_close)();
                let _ = ShowWindow(hwnd, SW_HIDE);
                return LRESULT(0);
            }
            WM_OD_QUIT => {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(st as *mut State));
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

fn is_fullscreen(hwnd: HWND) -> bool {
    unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & WS_CAPTION.0 == 0 }
}

/// Borderless over the whole monitor, or back to the saved window.
fn toggle_fullscreen(hwnd: HWND, st: &mut State) {
    unsafe {
        if is_fullscreen(hwnd) {
            let saved = st.fullscreen.take();
            let (style, placement) = match saved {
                Some(s) => (s.style, Some(s.placement)),
                None => (WS_OVERLAPPEDWINDOW.0 as isize, None),
            };
            SetWindowLongPtrW(hwnd, GWL_STYLE, style | WS_VISIBLE.0 as isize);
            match placement.filter(|p| p.length != 0) {
                Some(p) => {
                    let _ = SetWindowPlacement(hwnd, &p);
                }
                None => {
                    let _ = SetWindowPos(hwnd, None, 100, 100, 1280, 800, SWP_NOZORDER);
                }
            }
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
            );
        } else {
            let mut placement = WINDOWPLACEMENT {
                length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                ..Default::default()
            };
            let _ = GetWindowPlacement(hwnd, &mut placement);
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(mon, &mut info).as_bool() {
                st.fullscreen = Some(Saved { style, placement });
                SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
                let r = info.rcMonitor;
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
                );
            }
        }
    }
}

/// Draw the sender's cursor where it says it is, or hide it.
fn update_overlay(st: &mut State, hwnd: HWND) {
    let (Some(shared), Some(overlay)) = (st.shared.clone(), st.overlay) else {
        return;
    };
    let hide = |st: &mut State| {
        if st.overlay_shown {
            st.overlay_shown = false;
            unsafe {
                let _ = ShowWindow(overlay, SW_HIDE);
            }
        }
    };
    let gen_now = shared.sprite_gen.load(Ordering::Acquire);
    if gen_now != st.sprite_gen {
        st.sprite = shared.cursor.lock().unwrap().clone();
        st.sprite_gen = gen_now;
    }
    let pos = *shared.cursor_pos.lock().unwrap();
    let rect = shared.presenter.lock().unwrap().video_rect();
    let shown = unsafe { IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() };
    let (Some(sprite), Some((x, y)), Some(rect)) = (&st.sprite, pos, rect) else {
        return hide(st);
    };
    if !shared.cursor_visible.load(Ordering::Acquire) || !shown {
        return hide(st);
    }
    let size = (rect.width.round() as u32, rect.height.round() as u32);
    if st.image.is_none() || st.image_for != (gen_now, size) {
        st.image = pointer::Image::new(&sprite.render(rect.width, rect.height));
        st.image_for = (gen_now, size);
        if st.image.is_none() {
            debug!("could not build the cursor image");
        }
    }
    let Some(image) = &st.image else {
        return hide(st);
    };
    let (px, py) = rect.point(x, y);
    let mut at = POINT {
        x: px.round() as i32,
        y: py.round() as i32,
    };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut at);
    }
    image.place(overlay, at);
    if !st.overlay_shown {
        st.overlay_shown = true;
        unsafe {
            let _ = ShowWindow(overlay, SW_SHOWNOACTIVATE);
        }
    }
}
