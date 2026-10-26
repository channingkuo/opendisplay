//! Direct3D 11 presentation: the decoder's NV12 pictures go through the GPU's
//! video processor (colour conversion and scaling in one pass, letterboxed)
//! into a flip-model swap chain on the window.

use anyhow::{Context, Result, anyhow};
use od_receiver::input::VideoRect;
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};
use windows::core::Interface;

use super::decoder::{Color, Picture, PictureData};

pub struct Gpu {
    pub device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    /// Handed to the decoder so it decodes on this device.
    pub manager: IMFDXGIDeviceManager,
}

impl Gpu {
    pub fn new() -> Result<Gpu> {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("creating a Direct3D 11 device (needs a GPU with a WDDM driver)")?;
        }
        let device = device.ok_or_else(|| anyhow!("no D3D11 device"))?;
        let context = context.ok_or_else(|| anyhow!("no D3D11 context"))?;
        unsafe {
            // The decoder's worker threads touch the device too.
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
        }
        let video_device: ID3D11VideoDevice = device
            .cast()
            .context("this GPU/driver has no D3D11 video processor")?;
        let video_context: ID3D11VideoContext = context
            .cast()
            .context("this GPU/driver has no D3D11 video context")?;
        let mut token = 0u32;
        let mut manager = None;
        unsafe {
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        }
        let manager = manager.ok_or_else(|| anyhow!("no DXGI device manager"))?;
        unsafe { manager.ResetDevice(&device, token)? };
        Ok(Gpu {
            device,
            context,
            video_device,
            video_context,
            manager,
        })
    }

    /// Adapter description, for the log.
    pub fn adapter_name(&self) -> String {
        unsafe {
            self.device
                .cast::<IDXGIDevice>()
                .and_then(|d| d.GetAdapter())
                .and_then(|a| a.GetDesc())
                .map(|d| {
                    let end = d.Description.iter().position(|&c| c == 0).unwrap_or(128);
                    String::from_utf16_lossy(&d.Description[..end])
                })
                .unwrap_or_else(|_| "unknown GPU".into())
        }
    }
}

/// An NV12 texture the video processor reads from.
struct Source {
    texture: ID3D11Texture2D,
    /// Coded size of the texture.
    size: (u32, u32),
    /// System-memory upload target (software decode only).
    staging: Option<ID3D11Texture2D>,
}

struct Processor {
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// Input size the enumerator was made for.
    input: (u32, u32),
    output: (u32, u32),
}

pub struct Presenter {
    gpu: Gpu,
    swap: IDXGISwapChain1,
    size: (u32, u32),
    target: Option<Target>,
    source: Option<Source>,
    processor: Option<Processor>,
    /// Visible part of the source texture and how to read its colours.
    display: (u32, u32),
    color: Color,
    has_frame: bool,
}

/// The back buffer and the views onto it; rebuilt when the window resizes.
struct Target {
    render: ID3D11RenderTargetView,
    output: ID3D11VideoProcessorOutputView,
}

impl Presenter {
    pub fn new(gpu: Gpu, hwnd: HWND, size: (u32, u32)) -> Result<Presenter> {
        let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory1() }.context("DXGI factory")?;
        let size = (size.0.max(1), size.1.max(1));
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.0,
            Height: size.1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };
        let swap = unsafe { factory.CreateSwapChainForHwnd(&gpu.device, hwnd, &desc, None, None) }
            .context("creating the swap chain")?;
        unsafe {
            // We resize and present ourselves; no Alt+Enter exclusive mode.
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            if let Ok(d) = gpu.device.cast::<IDXGIDevice1>() {
                // Keep at most one frame queued: less latency.
                let _ = d.SetMaximumFrameLatency(1);
            }
        }
        let mut p = Presenter {
            gpu,
            swap,
            size,
            target: None,
            source: None,
            processor: None,
            display: (0, 0),
            color: Color {
                bt709: true,
                full_range: false,
            },
            has_frame: false,
        };
        p.clear()?;
        Ok(p)
    }

    pub fn manager(&self) -> &IMFDXGIDeviceManager {
        &self.gpu.manager
    }

    pub fn adapter_name(&self) -> String {
        self.gpu.adapter_name()
    }

    /// Decoded size of the picture on screen, once there is one.
    pub fn video_size(&self) -> Option<(u32, u32)> {
        (self.display.0 > 0 && self.display.1 > 0).then_some(self.display)
    }

    /// Where the video sits in the window.
    pub fn video_rect(&self) -> Option<VideoRect> {
        let v = self.video_size()?;
        VideoRect::fit(
            (self.size.0 as f64, self.size.1 as f64),
            (v.0 as f64, v.1 as f64),
        )
    }

    /// Input size the video processor is built for: the texture it reads.
    fn proc_input(&self) -> (u32, u32) {
        self.source.as_ref().map(|s| s.size).unwrap_or(self.display)
    }

    /// Make sure the back-buffer views exist (and the processor they belong to).
    fn ensure_target(&mut self) -> Result<()> {
        let input = self.proc_input();
        self.ensure_processor(input)?;
        if self.target.is_some() {
            return Ok(());
        }
        let back: ID3D11Texture2D = unsafe { self.swap.GetBuffer(0) }.context("back buffer")?;
        let mut render = None;
        let mut output = None;
        unsafe {
            self.gpu
                .device
                .CreateRenderTargetView(&back, None, Some(&mut render))?;
            let p = self.processor.as_ref().expect("ensured");
            let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            self.gpu.video_device.CreateVideoProcessorOutputView(
                &back,
                &p.enumerator,
                &desc,
                Some(&mut output),
            )?;
        }
        self.target = Some(Target {
            render: render.ok_or_else(|| anyhow!("no render target view"))?,
            output: output.ok_or_else(|| anyhow!("no output view"))?,
        });
        Ok(())
    }

    /// A video processor for `input`-sized pictures into the current window.
    fn ensure_processor(&mut self, input: (u32, u32)) -> Result<()> {
        let input = (input.0.max(16), input.1.max(16));
        if let Some(p) = &self.processor {
            if p.input == input && p.output == self.size {
                return Ok(());
            }
        }
        let rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: input.0,
            InputHeight: input.1,
            OutputFrameRate: rate,
            OutputWidth: self.size.0,
            OutputHeight: self.size.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { self.gpu.video_device.CreateVideoProcessorEnumerator(&desc) }
            .context("video processor enumerator")?;
        let processor = unsafe { self.gpu.video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("video processor")?;
        // The RGB output is full range sRGB-ish; no automatic enhancement
        // (it would add latency and change the picture).
        unsafe {
            self.gpu
                .video_context
                .VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            self.gpu.video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            let out = color_space(true, true, false);
            self.gpu
                .video_context
                .VideoProcessorSetOutputColorSpace(&processor, &out);
        }
        self.processor = Some(Processor {
            enumerator,
            processor,
            input,
            output: self.size,
        });
        // Views created against the old enumerator are not valid any more.
        self.target = None;
        Ok(())
    }

    /// The window changed size: resize the swap chain and redraw.
    pub fn resize(&mut self, size: (u32, u32)) -> Result<()> {
        let size = (size.0.max(1), size.1.max(1));
        if size == self.size {
            return Ok(());
        }
        self.target = None;
        self.processor = None;
        unsafe {
            self.swap
                .ResizeBuffers(
                    0,
                    size.0,
                    size.1,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
                .context("resizing the swap chain")?;
        }
        self.size = size;
        if self.has_frame {
            self.blit()
        } else {
            self.clear()
        }
    }

    /// Paint the window black.
    pub fn clear(&mut self) -> Result<()> {
        self.ensure_target()?;
        let t = self.target.as_ref().expect("ensured");
        unsafe {
            self.gpu
                .context
                .ClearRenderTargetView(&t.render, &[0.0, 0.0, 0.0, 1.0]);
            self.swap.Present(0, DXGI_PRESENT(0)).ok()?;
        }
        Ok(())
    }

    /// Show a decoded picture.
    pub fn present(&mut self, pic: &Picture) -> Result<()> {
        self.upload(pic)?;
        self.display = (pic.width, pic.height);
        self.color = pic.color;
        self.has_frame = true;
        self.blit()
    }

    /// Copy the picture into a texture of ours: decoder surfaces are recycled
    /// as soon as the next picture is decoded.
    fn upload(&mut self, pic: &Picture) -> Result<()> {
        match &pic.data {
            PictureData::Gpu {
                texture,
                subresource,
            } => {
                let mut d = D3D11_TEXTURE2D_DESC::default();
                unsafe { texture.GetDesc(&mut d) };
                let (dst, _) = self.source_for((d.Width, d.Height), false)?;
                unsafe {
                    self.gpu.context.CopySubresourceRegion(
                        &dst,
                        0,
                        0,
                        0,
                        0,
                        texture,
                        *subresource,
                        None,
                    );
                }
            }
            PictureData::Cpu {
                bytes,
                stride,
                rows,
            } => {
                // Coded size: rows may be padded to whole macroblocks.
                let coded = (
                    (*stride as u32).max(pic.width),
                    (*rows as u32).max(pic.height),
                );
                let (dst, staging) = self.source_for(coded, true)?;
                let staging = staging.expect("created for the CPU path");
                unsafe {
                    let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                    self.gpu
                        .context
                        .Map(&staging, 0, D3D11_MAP_WRITE, 0, Some(&mut m))
                        .context("mapping the upload texture")?;
                    let out = m.pData as *mut u8;
                    let pitch = m.RowPitch as usize;
                    let (w, h) = (coded.0 as usize, coded.1 as usize);
                    let copy = w.min(*stride).min(pitch);
                    // Y plane, then interleaved UV at half the rows. The
                    // staging texture lays the UV plane out right after the
                    // Y plane, `pitch * height` bytes in.
                    for y in 0..h {
                        if (y + 1) * stride > bytes.len() {
                            break;
                        }
                        std::ptr::copy_nonoverlapping(
                            bytes.as_ptr().add(y * stride),
                            out.add(y * pitch),
                            copy,
                        );
                    }
                    let uv_src = stride * rows;
                    for y in 0..h / 2 {
                        let from = uv_src + y * stride;
                        if from + copy > bytes.len() {
                            break;
                        }
                        std::ptr::copy_nonoverlapping(
                            bytes.as_ptr().add(from),
                            out.add((h + y) * pitch),
                            copy,
                        );
                    }
                    self.gpu.context.Unmap(&staging, 0);
                    self.gpu.context.CopyResource(&dst, &staging);
                }
            }
        }
        Ok(())
    }

    fn source_for(
        &mut self,
        size: (u32, u32),
        staging: bool,
    ) -> Result<(ID3D11Texture2D, Option<ID3D11Texture2D>)> {
        let size = (size.0 & !1, size.1 & !1);
        let fits = self
            .source
            .as_ref()
            .is_some_and(|s| s.size == size && s.staging.is_some() == staging);
        if !fits {
            let mut desc = D3D11_TEXTURE2D_DESC {
                Width: size.0,
                Height: size.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            };
            let mut texture = None;
            unsafe {
                self.gpu
                    .device
                    .CreateTexture2D(&desc, None, Some(&mut texture))
                    .context("creating the NV12 texture")?;
            }
            let staging = if staging {
                desc.Usage = D3D11_USAGE_STAGING;
                desc.BindFlags = 0;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE.0 as u32;
                let mut t = None;
                unsafe {
                    self.gpu
                        .device
                        .CreateTexture2D(&desc, None, Some(&mut t))
                        .context("creating the upload texture")?;
                }
                t
            } else {
                None
            };
            self.source = Some(Source {
                texture: texture.ok_or_else(|| anyhow!("no texture"))?,
                size,
                staging,
            });
        }
        let s = self.source.as_ref().expect("just set");
        Ok((s.texture.clone(), s.staging.clone()))
    }

    /// Draw the source texture, letterboxed, and present.
    fn blit(&mut self) -> Result<()> {
        let Some(rect) = self.video_rect() else {
            return self.clear();
        };
        self.ensure_target()?;
        let source = self.source.as_ref().expect("a frame was uploaded");
        let p = self.processor.as_ref().expect("ensured");
        let t = self.target.as_ref().expect("ensured");

        let mut view = None;
        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        unsafe {
            self.gpu.video_device.CreateVideoProcessorInputView(
                &source.texture,
                &p.enumerator,
                &in_desc,
                Some(&mut view),
            )?;
        }
        let view = view.ok_or_else(|| anyhow!("no input view"))?;

        let src = RECT {
            left: 0,
            top: 0,
            right: self.display.0 as i32,
            bottom: self.display.1 as i32,
        };
        let dst = RECT {
            left: rect.x.round() as i32,
            top: rect.y.round() as i32,
            right: (rect.x + rect.width).round() as i32,
            bottom: (rect.y + rect.height).round() as i32,
        };
        let full = RECT {
            left: 0,
            top: 0,
            right: self.size.0 as i32,
            bottom: self.size.1 as i32,
        };
        let vc = &self.gpu.video_context;
        unsafe {
            self.gpu
                .context
                .ClearRenderTargetView(&t.render, &[0.0, 0.0, 0.0, 1.0]);
            let cs = color_space(false, self.color.bt709, self.color.full_range);
            vc.VideoProcessorSetStreamColorSpace(&p.processor, 0, &cs);
            vc.VideoProcessorSetStreamSourceRect(&p.processor, 0, true, Some(&src));
            vc.VideoProcessorSetStreamDestRect(&p.processor, 0, true, Some(&dst));
            vc.VideoProcessorSetOutputTargetRect(&p.processor, true, Some(&full));
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(view)),
                ..Default::default()
            };
            let r = vc.VideoProcessorBlt(&p.processor, &t.output, 0, std::slice::from_ref(&stream));
            // Release the input view we moved into the stream.
            drop(std::mem::ManuallyDrop::into_inner(stream.pInputSurface));
            r.context("video processor blit")?;
            // Sync interval 0: show it now. Frames arrive at the stream's
            // rate; a faster stream than the display just replaces the queued
            // one (flip model, latency 1).
            let hr = self.swap.Present(0, DXGI_PRESENT(0));
            if hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET {
                return Err(anyhow!("the GPU device was removed or reset"));
            }
        }
        Ok(())
    }
}

/// `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` bit field: Usage:1 (0 = playback),
/// RGB_Range:1 (0 = full), YCbCr_Matrix:1 (0 = BT.601, 1 = BT.709),
/// YCbCr_xvYCC:1, Nominal_Range:2 (0 unspecified, 1 = 16-235, 2 = 0-255).
fn color_space(rgb: bool, bt709: bool, full_range: bool) -> D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
    let mut bits = 0u32;
    if rgb {
        // RGB output: full range 0-255.
        bits |= 2 << 4;
    } else {
        if bt709 {
            bits |= 1 << 2;
        }
        bits |= (if full_range { 2 } else { 1 }) << 4;
    }
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: bits }
}
