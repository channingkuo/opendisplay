//! Monitor facts for `hello` (§6.7): physical pixels, DPI scale, refresh.

use std::mem::size_of;

use windows::Win32::Foundation::{LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::core::BOOL;

use crate::facts::MonitorFacts;

const MONITORINFOF_PRIMARY: u32 = 1;

pub fn list() -> Vec<MonitorFacts> {
    let mut out: Vec<MonitorFacts> = Vec::new();
    unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let out = unsafe { &mut *(data.0 as *mut Vec<MonitorFacts>) };
        if let Some(f) = facts(m) {
            out.push(f);
        }
        TRUE
    }
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut out as *mut _ as isize));
    }
    // Stable order: the primary first, then left to right.
    out.sort_by_key(|m| (!m.primary, m.origin));
    out
}

fn facts(m: HMONITOR) -> Option<MonitorFacts> {
    unsafe {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(m, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
            return None;
        }
        let r = info.monitorInfo.rcMonitor;
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let dev = info.szDevice;
        let mut mode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let refresh = if EnumDisplaySettingsW(
            windows::core::PCWSTR(dev.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
        .as_bool()
        {
            mode.dmDisplayFrequency
        } else {
            0
        };
        let end = dev.iter().position(|&c| c == 0).unwrap_or(dev.len());
        Some(MonitorFacts {
            name: String::from_utf16_lossy(&dev[..end]),
            origin: (r.left, r.top),
            pixels: (
                (r.right - r.left).max(0) as u32,
                (r.bottom - r.top).max(0) as u32,
            ),
            scale: dx.max(96) as f64 / 96.0,
            refresh_hz: refresh,
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        })
    }
}
