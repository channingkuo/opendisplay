//! Windows presentation: Win32 window, Media Foundation decode, Direct3D 11.

mod decoder;
mod gpu;
mod monitors;
mod pointer;
pub mod window;

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use od_receiver::video::VideoOutput;
use opendisplay_proto::control::CursorImg;
use tracing::{info, warn};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

pub use gpu::{Gpu, Presenter};
pub use monitors::list as monitors;
pub use window::{Shared, WindowOptions};

/// Process-wide Media Foundation init (once, from the main thread).
pub fn startup() -> Result<()> {
    // Media Foundation's worker threads and the decoder want an MTA; plain
    // Win32 windows work fine in one.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .context("CoInitializeEx")?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")
    }
}

/// Decode on the GPU when it can; software otherwise.
pub struct WinVideo {
    shared: Arc<Shared>,
    decoder: Option<decoder::Decoder>,
    /// Try the DXVA decoder first; flipped off if it fails before any
    /// picture was shown.
    hardware: bool,
    com_ready: bool,
    rendered: u64,
    decode_ms: Vec<f64>,
    pending_error: Option<String>,
    describe: String,
}

// The COM objects inside are only touched from the receiver thread; the
// shared presenter is behind a mutex.
unsafe impl Send for WinVideo {}

impl WinVideo {
    pub fn new(shared: Arc<Shared>) -> WinVideo {
        WinVideo {
            shared,
            decoder: None,
            hardware: true,
            com_ready: false,
            rendered: 0,
            decode_ms: Vec::new(),
            pending_error: None,
            describe: "Media Foundation H.264".into(),
        }
    }

    /// Never use the GPU decoder (`--software-decode`).
    pub fn force_software(&mut self) {
        self.hardware = false;
    }

    fn decoder(&mut self) -> Result<&mut decoder::Decoder> {
        if !self.com_ready {
            // Media Foundation wants an MTA thread; this is the receiver's.
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok().ok() };
            self.com_ready = true;
        }
        if self.decoder.is_none() {
            let manager = {
                let p = self.shared.presenter.lock().unwrap();
                self.hardware.then(|| p.manager().clone())
            };
            let d = decoder::Decoder::new(manager.as_ref())?;
            let gpu = self.shared.presenter.lock().unwrap().adapter_name();
            self.describe = if d.is_hardware() {
                format!("Media Foundation H.264 (DXVA, {gpu})")
            } else {
                format!("Media Foundation H.264 (software) -> {gpu}")
            };
            info!("decoder: {}", self.describe);
            self.decoder = Some(d);
        }
        Ok(self.decoder.as_mut().expect("just set"))
    }
}

impl VideoOutput for WinVideo {
    fn push(&mut self, annexb: &[u8], _is_idr: bool) -> Result<()> {
        let started = Instant::now();
        let picture = match self.decoder().and_then(|d| d.decode(annexb)) {
            Ok(p) => p,
            Err(e) => {
                if self.hardware && self.rendered == 0 {
                    warn!("GPU decode failed ({e:#}); falling back to software decode");
                    self.hardware = false;
                    self.decoder = None;
                }
                return Err(e);
            }
        };
        if let Some(pic) = picture {
            let r = self.shared.presenter.lock().unwrap().present(&pic);
            match r {
                Ok(()) => {
                    self.rendered += 1;
                    self.decode_ms
                        .push(started.elapsed().as_secs_f64() * 1000.0);
                }
                Err(e) => {
                    self.pending_error = Some(format!("{e:#}"));
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        // New parameter sets or a new sender: a fresh decoder drops every
        // buffered picture and reference.
        self.decoder = None;
        Ok(())
    }

    fn poll_error(&mut self) -> Option<String> {
        self.pending_error.take()
    }

    fn describe(&mut self) -> String {
        self.describe.clone()
    }

    fn rendered(&self) -> Option<u64> {
        Some(self.rendered)
    }

    fn take_decode_ms(&mut self) -> Vec<f64> {
        std::mem::take(&mut self.decode_ms)
    }

    fn cursor(&mut self, _x: Option<f64>, _y: Option<f64>, visible: bool) {
        self.shared.set_cursor_visible(visible);
    }

    fn cursor_image(&mut self, img: &CursorImg) {
        match crate::sprite::decode(img) {
            Ok(s) => self.shared.set_sprite(s),
            Err(e) => warn!("cursor sprite: {e:#}"),
        }
    }
}
