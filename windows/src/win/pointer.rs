//! The sender's cursor, drawn in its own transparent click-through window
//! laid over the video (a layered window with per-pixel alpha), so it moves
//! at the sender's cursor rate and never touches the swap chain.

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use crate::sprite::PointerBitmap;

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// Registers the class (once) and creates the hidden overlay owned by `owner`.
pub fn create_window(owner: HWND) -> Option<HWND> {
    unsafe {
        let hinstance = GetModuleHandleW(None).ok()?;
        let class = w!("OpenDisplayCursor");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(overlay_proc),
            hInstance: hinstance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        // A second registration (same class) fails harmlessly.
        RegisterClassW(&wc);
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class,
            w!(""),
            WS_POPUP,
            0,
            0,
            1,
            1,
            Some(owner),
            None,
            Some(hinstance.into()),
            None,
        )
        .ok()
    }
}

/// A pointer bitmap selected into a memory DC, ready for `UpdateLayeredWindow`.
pub struct Image {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    pub width: u32,
    pub height: u32,
    pub hot_x: i32,
    pub hot_y: i32,
}

impl Image {
    pub fn new(p: &PointerBitmap) -> Option<Image> {
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let header = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: p.width as i32,
                    // Negative: top-down rows, like our buffer.
                    biHeight: -(p.height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap =
                CreateDIBSection(Some(screen), &header, DIB_RGB_COLORS, &mut bits, None, 0);
            ReleaseDC(None, screen);
            let Ok(bitmap) = bitmap else {
                let _ = DeleteDC(dc);
                return None;
            };
            if bits.is_null() {
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(dc);
                return None;
            }
            std::ptr::copy_nonoverlapping(p.bgra.as_ptr(), bits as *mut u8, p.bgra.len());
            let old = SelectObject(dc, bitmap.into());
            Some(Image {
                dc,
                bitmap,
                old,
                width: p.width,
                height: p.height,
                hot_x: p.hot_x as i32,
                hot_y: p.hot_y as i32,
            })
        }
    }

    /// Show the image with its hotspot at screen point `at`.
    pub fn place(&self, window: HWND, at: POINT) {
        let dst = POINT {
            x: at.x - self.hot_x,
            y: at.y - self.hot_y,
        };
        let size = SIZE {
            cx: self.width as i32,
            cy: self.height as i32,
        };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            let _ = UpdateLayeredWindow(
                window,
                None,
                Some(&dst),
                Some(&size),
                Some(self.dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}
