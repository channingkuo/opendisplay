//! The sender's cursor sprite as a Windows pointer (an `HCURSOR`).

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::sprite::PointerBitmap;

/// An owned pointer; destroyed on drop.
pub struct Pointer(pub HCURSOR);

impl Drop for Pointer {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyCursor(self.0);
        }
    }
}

pub fn create(p: &PointerBitmap) -> Option<Pointer> {
    unsafe {
        let mut header = BITMAPV5HEADER {
            bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
            bV5Width: p.width as i32,
            // Negative: rows run top-down, like our buffer.
            bV5Height: -(p.height as i32),
            bV5Planes: 1,
            bV5BitCount: 32,
            bV5Compression: BI_BITFIELDS,
            bV5RedMask: 0x00FF_0000,
            bV5GreenMask: 0x0000_FF00,
            bV5BlueMask: 0x0000_00FF,
            bV5AlphaMask: 0xFF00_0000,
            ..Default::default()
        };
        let hdc = GetDC(None::<HWND>);
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(
            Some(hdc),
            &mut header as *mut _ as *const BITMAPINFO,
            DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        );
        ReleaseDC(None, hdc);
        let color = color.ok()?;
        if bits.is_null() {
            let _ = DeleteObject(color.into());
            return None;
        }
        std::ptr::copy_nonoverlapping(p.bgra.as_ptr(), bits as *mut u8, p.bgra.len());
        // A colour bitmap with alpha ignores the mask, but one is required.
        let mask = CreateBitmap(p.width as i32, p.height as i32, 1, 1, None);
        let info = ICONINFO {
            fIcon: false.into(),
            xHotspot: p.hot_x,
            yHotspot: p.hot_y,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info);
        let _ = DeleteObject(mask.into());
        let _ = DeleteObject(color.into());
        icon.ok().map(|i| Pointer(HCURSOR(i.0)))
    }
}
