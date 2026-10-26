//! What the machine tells us about its panel, and the `hello` built from it
//! (§6.1, §6.7). Pure data, so it is tested on every platform.

use anyhow::{Result, bail};
use opendisplay_proto::control::{Hello, Panel, VideoCap};

/// One monitor as Windows reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorFacts {
    /// `\\.\DISPLAY1`.
    pub name: String,
    /// Virtual-screen position of the top-left corner, physical pixels.
    pub origin: (i32, i32),
    /// Physical pixels lit by the panel.
    pub pixels: (u32, u32),
    /// Effective DPI / 96 (1.0, 1.25, 1.5, 2.0 ...).
    pub scale: f64,
    /// 0 or 1 mean "unknown" (virtual / remote displays).
    pub refresh_hz: u32,
    pub primary: bool,
}

impl MonitorFacts {
    /// Whole Hz, or `None` when unknown: announcing 1 Hz would cap the
    /// sender at one frame per second.
    pub fn refresh(&self) -> Option<u32> {
        (self.refresh_hz > 1).then_some(self.refresh_hz)
    }
}

/// The monitor named by `--monitor` (an index into `list` or a device name
/// such as `\\.\DISPLAY2`), else the primary one.
pub fn pick<'a>(list: &'a [MonitorFacts], want: Option<&str>) -> Result<&'a MonitorFacts> {
    if list.is_empty() {
        bail!("Windows reports no monitors");
    }
    match want {
        None => Ok(list.iter().find(|m| m.primary).unwrap_or(&list[0])),
        Some(w) => {
            if let Ok(i) = w.parse::<usize>() {
                if let Some(m) = list.get(i.saturating_sub(1)).filter(|_| i >= 1) {
                    return Ok(m);
                }
            }
            list.iter()
                .find(|m| m.name.eq_ignore_ascii_case(w))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no monitor `{w}`; have {:?} (1-based index or device name)",
                        list.iter().map(|m| m.name.as_str()).collect::<Vec<_>>()
                    )
                })
        }
    }
}

/// Overrides from the command line.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub scale: Option<f64>,
    pub refresh: Option<u32>,
}

pub fn hello(
    device: &str,
    install_id: &str,
    monitor: Option<&MonitorFacts>,
    o: &Overrides,
) -> Result<Hello> {
    let (width, height) = match (o.width, o.height, monitor) {
        (Some(w), Some(h), _) => (w, h),
        (w, h, Some(m)) => (w.unwrap_or(m.pixels.0), h.unwrap_or(m.pixels.1)),
        _ => bail!("no display information; pass --width and --height"),
    };
    let scale = o.scale.or(monitor.map(|m| m.scale)).unwrap_or(1.0).max(0.5);
    let refresh = o.refresh.or(monitor.and_then(|m| m.refresh()));
    let points = (
        (width as f64 / scale).round() as u32,
        (height as f64 / scale).round() as u32,
    );
    Ok(Hello {
        pixels_wide: width,
        pixels_high: height,
        // Deprecated field (§7): senders without `panel` read it as a UI
        // factor, so keep it whole.
        scale: scale.ceil().max(1.0),
        device: Some(device.to_string()),
        id: Some(install_id.to_string()),
        display_max_frame_rate: refresh,
        // §6.7: facts only; the sender decides the desktop size.
        panel: Some(Panel {
            pixels_wide: Some(width),
            pixels_high: Some(height),
            scale: Some(scale),
            points_wide: Some(points.0),
            points_high: Some(points.1),
            malformed: false,
        }),
        // §6.5: H.264 always; only the panel's rate bounds it.
        video_caps: Some(vec![VideoCap {
            codec: "h264".into(),
            max_width: None,
            max_height: None,
            max_frame_rate: refresh,
            max_pixels_per_second: None,
        }]),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(name: &str, primary: bool, hz: u32) -> MonitorFacts {
        MonitorFacts {
            name: name.into(),
            origin: (0, 0),
            pixels: (2560, 1440),
            scale: 1.5,
            refresh_hz: hz,
            primary,
        }
    }

    #[test]
    fn picks_primary_index_and_name() {
        let l = [
            mon(r"\\.\DISPLAY1", false, 60),
            mon(r"\\.\DISPLAY2", true, 144),
        ];
        assert_eq!(pick(&l, None).unwrap().name, r"\\.\DISPLAY2");
        assert_eq!(pick(&l, Some("1")).unwrap().name, r"\\.\DISPLAY1");
        assert_eq!(
            pick(&l, Some(r"\\.\display2")).unwrap().name,
            r"\\.\DISPLAY2"
        );
        assert!(pick(&l, Some("0")).is_err());
        assert!(pick(&l, Some("3")).is_err());
        assert!(pick(&[], None).is_err());
    }

    #[test]
    fn hello_from_monitor() {
        let m = mon("a", true, 144);
        let h = hello("Windows", "ID", Some(&m), &Overrides::default()).unwrap();
        assert_eq!((h.pixels_wide, h.pixels_high), (2560, 1440));
        assert_eq!(h.scale, 2.0);
        assert_eq!(h.display_max_frame_rate, Some(144));
        let p = h.panel.unwrap();
        assert_eq!(p.scale, Some(1.5));
        assert_eq!((p.points_wide, p.points_high), (Some(1707), Some(960)));
        assert_eq!(h.video_caps.unwrap()[0].max_frame_rate, Some(144));
    }

    #[test]
    fn unknown_refresh_is_not_one_hertz() {
        let m = mon("a", true, 1);
        let h = hello("Windows", "ID", Some(&m), &Overrides::default()).unwrap();
        assert_eq!(h.display_max_frame_rate, None);
    }

    #[test]
    fn overrides_win_and_need_a_size_without_monitor() {
        let o = Overrides {
            width: Some(1920),
            height: Some(1080),
            scale: Some(1.0),
            refresh: Some(60),
        };
        let h = hello("W", "I", None, &o).unwrap();
        assert_eq!((h.pixels_wide, h.scale), (1920, 1.0));
        assert!(hello("W", "I", None, &Overrides::default()).is_err());
    }
}
