//! The sender's cursor sprite (§6.2) as a bitmap the window system can use as
//! its pointer: decode the PNG, scale it to the size the video is shown at,
//! premultiply, and order the bytes BGRA as GDI wants them.

use anyhow::{Context, Result, bail};
use base64::Engine;

/// Straight-alpha RGBA as sent by the sender.
#[derive(Debug, Clone, PartialEq)]
pub struct Sprite {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Size normalised to the display, hotspot normalised within the sprite.
    pub nw: f64,
    pub nh: f64,
    pub ax: f64,
    pub ay: f64,
}

/// Largest pointer Windows will draw reliably.
pub const MAX_SIDE: u32 = 256;

pub fn decode(img: &opendisplay_proto::control::CursorImg) -> Result<Sprite> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(img.png.trim())
        .context("cursor sprite is not base64")?;
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().context("cursor sprite PNG header")?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).context("cursor sprite PNG")?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => bail!("indexed cursor PNG was not expanded"),
    };
    if info.width == 0 || info.height == 0 {
        bail!("empty cursor sprite");
    }
    Ok(Sprite {
        width: info.width,
        height: info.height,
        rgba,
        nw: img.nw,
        nh: img.nh,
        ax: img.ax.clamp(0.0, 1.0),
        ay: img.ay.clamp(0.0, 1.0),
    })
}

/// A ready-to-use pointer bitmap.
#[derive(Debug, Clone, PartialEq)]
pub struct PointerBitmap {
    pub width: u32,
    pub height: u32,
    /// Premultiplied BGRA, top-down.
    pub bgra: Vec<u8>,
    pub hot_x: u32,
    pub hot_y: u32,
}

impl Sprite {
    /// The pointer when the video is drawn `video_w` x `video_h` window pixels
    /// large: the sprite's normalised size times that, capped at
    /// [`MAX_SIDE`], never below 1 pixel.
    pub fn render(&self, video_w: f64, video_h: f64) -> PointerBitmap {
        let w = ((self.nw * video_w).round() as u32).clamp(1, MAX_SIDE);
        let h = ((self.nh * video_h).round() as u32).clamp(1, MAX_SIDE);
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        // Area average (box filter): sprites are small, and shrinking them is
        // the common case on a HiDPI sender.
        for y in 0..h {
            let (y0, y1) = span(y, h, self.height);
            for x in 0..w {
                let (x0, x1) = span(x, w, self.width);
                let (mut r, mut g, mut b, mut a, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        let i = ((sy * self.width + sx) * 4) as usize;
                        let pa = self.rgba[i + 3] as u32;
                        r += self.rgba[i] as u32 * pa;
                        g += self.rgba[i + 1] as u32 * pa;
                        b += self.rgba[i + 2] as u32 * pa;
                        a += pa;
                        n += 1;
                    }
                }
                let o = ((y * w + x) * 4) as usize;
                if n > 0 {
                    // r,g,b were weighted by alpha, so dividing by 255*n gives
                    // the premultiplied average.
                    bgra[o] = (b / (255 * n)) as u8;
                    bgra[o + 1] = (g / (255 * n)) as u8;
                    bgra[o + 2] = (r / (255 * n)) as u8;
                    bgra[o + 3] = (a / n) as u8;
                }
            }
        }
        PointerBitmap {
            width: w,
            height: h,
            bgra,
            hot_x: ((self.ax * w as f64).round() as u32).min(w - 1),
            hot_y: ((self.ay * h as f64).round() as u32).min(h - 1),
        }
    }
}

/// Source pixels `[start, end)` that feed destination pixel `i` of `dst`.
fn span(i: u32, dst: u32, src: u32) -> (u32, u32) {
    let start = (i as u64 * src as u64 / dst as u64) as u32;
    let end = (((i as u64 + 1) * src as u64).div_ceil(dst as u64) as u32).clamp(start + 1, src);
    (start.min(src - 1), end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendisplay_proto::control::CursorImg;

    fn png_of(w: u32, h: u32, rgba: &[u8]) -> String {
        let mut out = Vec::new();
        let mut e = png::Encoder::new(&mut out, w, h);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.write_header().unwrap().write_image_data(rgba).unwrap();
        base64::engine::general_purpose::STANDARD.encode(out)
    }

    #[test]
    fn decodes_and_premultiplies_to_bgra() {
        // One opaque red pixel and one half-transparent white pixel.
        let img = CursorImg {
            nw: 0.002,
            nh: 0.001,
            ax: 0.0,
            ay: 0.0,
            png: png_of(2, 1, &[255, 0, 0, 255, 255, 255, 255, 128]),
        };
        let s = decode(&img).unwrap();
        assert_eq!((s.width, s.height), (2, 1));
        // Shown at 1:1 (2x1 px): nw * 1000 = 2, nh * 1000 = 1.
        let p = s.render(1000.0, 1000.0);
        assert_eq!((p.width, p.height), (2, 1));
        assert_eq!(&p.bgra[0..4], &[0, 0, 255, 255]);
        assert_eq!(&p.bgra[4..8], &[128, 128, 128, 128]);
    }

    #[test]
    fn scales_and_places_hotspot() {
        let px: Vec<u8> = (0..4 * 4).flat_map(|_| [0, 255, 0, 255]).collect();
        let img = CursorImg {
            nw: 0.01,
            nh: 0.02,
            ax: 0.5,
            ay: 1.0,
            png: png_of(4, 4, &px),
        };
        let p = decode(&img).unwrap().render(1000.0, 1000.0);
        assert_eq!((p.width, p.height), (10, 20));
        assert_eq!((p.hot_x, p.hot_y), (5, 19));
        assert!(p.bgra.chunks(4).all(|c| c == [0, 255, 0, 255]));
    }

    #[test]
    fn caps_the_size() {
        let img = CursorImg {
            nw: 1.0,
            nh: 1.0,
            ax: 0.0,
            ay: 0.0,
            png: png_of(1, 1, &[1, 2, 3, 255]),
        };
        let p = decode(&img).unwrap().render(4000.0, 4000.0);
        assert_eq!((p.width, p.height), (MAX_SIDE, MAX_SIDE));
    }

    #[test]
    fn rejects_garbage() {
        let img = CursorImg {
            nw: 0.1,
            nh: 0.1,
            ax: 0.0,
            ay: 0.0,
            png: "not base64!".into(),
        };
        assert!(decode(&img).is_err());
    }
}
