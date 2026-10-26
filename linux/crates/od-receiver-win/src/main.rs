#![cfg_attr(not(windows), allow(dead_code))]

//! OpenDisplay receiver for Windows: turns a Windows PC's screen into an
//! OpenDisplay display (PROTOCOL.md pv 3). The network, session and Bonjour
//! halves are the Linux receiver's (`od-receiver`); this crate adds a Win32
//! window, Media Foundation H.264 decoding and Direct3D 11 presentation.

mod facts;
mod sprite;
#[cfg(windows)]
mod win;

#[cfg(not(windows))]
fn main() {
    eprintln!("od-receiver-win only runs on Windows; on Linux use od-receiver.");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    app::main()
}

#[cfg(windows)]
mod app {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::{Context, Result};
    use clap::Parser;
    use od_receiver::{Config, Receiver, discovery, id};
    use tokio::sync::watch;
    use tracing::{info, warn};

    use crate::facts::{self, Overrides};
    use crate::win;

    /// OpenDisplay receiver for Windows.
    #[derive(Parser, Debug)]
    #[command(version, about)]
    struct Args {
        /// TCP listen address (§1: the receiver listens).
        #[arg(long, default_value = "0.0.0.0:9000")]
        listen: SocketAddr,
        /// Bonjour instance name (display-only). Default: the computer name.
        #[arg(long)]
        name: Option<String>,
        /// `hello.device` (free-form).
        #[arg(long, default_value = "Windows")]
        device: String,
        /// Monitor to describe in `hello` and to cover when fullscreen: its
        /// 1-based number or device name (`\\.\DISPLAY2`). Default: primary.
        #[arg(long)]
        monitor: Option<String>,
        /// Print the monitors Windows reports and exit.
        #[arg(long)]
        list_monitors: bool,
        /// Override the announced panel width in physical pixels.
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        height: Option<u32>,
        /// Override the panel scale (else the monitor's DPI scale).
        #[arg(long)]
        scale: Option<f64>,
        /// Override `hello.displayMaxFrameRate` (else the monitor's refresh).
        #[arg(long)]
        refresh: Option<u32>,
        /// Present in a normal window instead of fullscreen (F11 toggles).
        #[arg(long)]
        windowed: bool,
        /// Decode on the CPU even when the GPU can decode H.264.
        #[arg(long)]
        software_decode: bool,
        /// Do not advertise via mDNS.
        #[arg(long)]
        no_mdns: bool,
        /// Do not bind UDP port+1 for the cursor side channel (§6.3).
        #[arg(long)]
        no_cursor_udp: bool,
        #[arg(long, default_value_t = 5.0)]
        stats_every: f64,
    }

    pub fn main() -> Result<()> {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .with_target(false)
            .init();
        let a = Args::parse();
        // Physical pixels everywhere: the panel size in `hello`, the video
        // rect and the pointer all depend on it.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }

        let monitors = win::monitors();
        for (i, m) in monitors.iter().enumerate() {
            info!(
                "monitor {}: {} {}x{} scale {} {} Hz{}",
                i + 1,
                m.name,
                m.pixels.0,
                m.pixels.1,
                m.scale,
                m.refresh_hz,
                if m.primary { " (primary)" } else { "" }
            );
        }
        if a.list_monitors {
            return Ok(());
        }
        let monitor = match facts::pick(&monitors, a.monitor.as_deref()) {
            Ok(m) => Some(m.clone()),
            Err(e) if a.monitor.is_none() => {
                warn!("{e:#}");
                None
            }
            Err(e) => return Err(e),
        };

        let install_id = id::load_or_create()?;
        let hello = facts::hello(
            &a.device,
            &install_id,
            monitor.as_ref(),
            &Overrides {
                width: a.width,
                height: a.height,
                scale: a.scale,
                refresh: a.refresh,
            },
        )?;

        win::startup()?;
        let gpu = win::Gpu::new()?;
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
        let (power_tx, power_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = watch::channel(false);
        let stop_tx = Arc::new(stop_tx);

        let (origin, size) = monitor
            .as_ref()
            .map(|m| (m.origin, m.pixels))
            .unwrap_or(((0, 0), (1280, 800)));
        let (hwnd, state) = win::window::create(
            &win::WindowOptions {
                monitor_origin: origin,
                monitor_size: size,
                windowed: a.windowed,
            },
            input_tx,
            power_tx,
            {
                let stop = stop_tx.clone();
                move || {
                    let _ = stop.send(true);
                }
            },
        )?;
        let presenter = win::Presenter::new(gpu, hwnd, win::window::client_size(hwnd))?;
        info!("GPU: {}", presenter.adapter_name());
        let shared = win::Shared::new(presenter);
        let mut video = win::WinVideo::new(shared.clone());
        if a.software_decode {
            video.force_software();
        }

        let rt = tokio::runtime::Runtime::new()?;
        // Bound here so a busy port fails before any window shows.
        let receiver = rt
            .block_on(Receiver::bind(
                Config {
                    listen: a.listen,
                    hello,
                    cursor_udp: !a.no_cursor_udp,
                    stats_every: Duration::from_secs_f64(a.stats_every),
                },
                Box::new(video),
            ))?
            .with_input(input_rx)
            .with_power(power_rx);

        let worker = {
            let shared = shared.clone();
            let stop_tx = stop_tx.clone();
            std::thread::Builder::new()
                .name("receiver".into())
                .spawn(move || {
                    let result = rt.block_on(async {
                        tokio::spawn(async move {
                            let _ = tokio::signal::ctrl_c().await;
                            info!("shutting down");
                            let _ = stop_tx.send(true);
                        });
                        serve(&a, receiver, &install_id, stop_rx).await
                    });
                    shared.quit();
                    result
                })?
        };
        win::window::run(hwnd, state, shared);
        // Window gone (or loop ended): make sure the receiver stops too.
        let _ = stop_tx.send(true);
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("receiver thread panicked"))?
    }

    /// Advertise, serve until `shutdown`, withdraw.
    async fn serve(
        a: &Args,
        receiver: Receiver,
        install_id: &str,
        shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let port = receiver.local_addr()?.port();
        let host = std::env::var("COMPUTERNAME")
            .ok()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "opendisplay".into());
        let advert = if a.no_mdns {
            None
        } else {
            match discovery::Advertisement::start(
                a.name.as_deref().unwrap_or(&host),
                &host,
                port,
                install_id,
            ) {
                Ok(ad) => Some(ad),
                Err(e) => {
                    warn!(
                        "mDNS advertisement failed ({e:#}); senders must dial {} manually",
                        receiver.local_addr()?
                    );
                    None
                }
            }
        };
        let result = receiver.run(shutdown).await.context("receiver");
        if let Some(ad) = advert {
            ad.stop();
        }
        result.map(|_| ())
    }
}
