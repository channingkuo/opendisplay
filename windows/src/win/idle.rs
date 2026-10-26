//! The screen shown while no sender is connected: the app icon over a dark
//! background with a "waiting for connection" line, drawn with GDI into a
//! BGRA buffer the presenter copies to the back buffer.

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

const BACKGROUND: u32 = 0x0018_1A1F;

/// LANG_CHINESE, the primary language id of the UI language.
fn chinese_ui() -> bool {
    unsafe { GetUserDefaultUILanguage() & 0x3ff == 0x04 }
}

/// A top-down BGRA image of `size`, or `None` if GDI refuses.
pub fn render(size: (u32, u32)) -> Option<Vec<u8>> {
    let (w, h) = (size.0 as i32, size.1 as i32);
    if w <= 0 || h <= 0 {
        return None;
    }
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
        else {
            let _ = DeleteDC(dc);
            return None;
        };
        if bits.is_null() {
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            return None;
        }
        let old = SelectObject(dc, bitmap.into());
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
        pixels.fill(BACKGROUND);

        let icon_side = (h / 5).clamp(64, 256);
        let gap = icon_side / 4;
        let font_height = (h / 28).clamp(18, 40);
        let text_height = font_height * 2;
        let top = (h - (icon_side + gap + text_height)) / 2;

        let module = GetModuleHandleW(None).unwrap_or_default();
        // MAKEINTRESOURCE(1): the exe's icon resource (see build.rs).
        if let Ok(icon) = LoadImageW(
            Some(module.into()),
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            icon_side,
            icon_side,
            LR_DEFAULTCOLOR,
        ) {
            let icon = HICON(icon.0);
            let _ = DrawIconEx(
                dc,
                (w - icon_side) / 2,
                top,
                icon,
                icon_side,
                icon_side,
                0,
                None,
                DI_NORMAL,
            );
            let _ = DestroyIcon(icon);
        }

        let font = CreateFontW(
            font_height,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            w!("Segoe UI"),
        );
        let old_font = SelectObject(dc, font.into());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00C8_C8C8));
        let text = if chinese_ui() {
            "等待连接…"
        } else {
            "Waiting for connection…"
        };
        let mut text: Vec<u16> = text.encode_utf16().collect();
        let mut rect = RECT {
            left: 0,
            top: top + icon_side + gap,
            right: w,
            bottom: top + icon_side + gap + text_height,
        };
        DrawTextW(dc, &mut text, &mut rect, DT_CENTER | DT_TOP | DT_SINGLELINE);
        SelectObject(dc, old_font);
        let _ = DeleteObject(font.into());

        // GDI leaves the alpha byte at 0 or arbitrary; the swap chain
        // ignores it, but keep the image opaque anyway.
        let out: Vec<u8> = pixels
            .iter()
            .flat_map(|p| (p | 0xff00_0000).to_le_bytes())
            .collect();
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        Some(out)
    }
}
