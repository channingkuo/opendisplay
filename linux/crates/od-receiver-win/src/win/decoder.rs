//! H.264 decode through Media Foundation's Microsoft H.264 decoder MFT.
//!
//! With a Direct3D 11 device manager attached the MFT decodes on the GPU
//! (DXVA2) and hands back `ID3D11Texture2D` surfaces that stay in video
//! memory; without one (or when the GPU cannot decode the stream) it decodes in
//! software into NV12 system memory. Input is one Annex B access unit per call
//! (§5.1) and the MFT runs in low-latency mode, so a picture comes out as soon
//! as it is decodable (the stream has no B-frames).

use std::mem::ManuallyDrop;

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::{GUID, Interface};

/// `CLSID_CMSH264DecoderMFT`.
const CLSID_H264_DECODER: GUID = GUID::from_u128(0x62CE7E72_4C71_4D20_B15D_452831A87D9D);

/// `MF_E_NOTACCEPTING`: drain the output, then feed again.
const MF_E_NOTACCEPTING: i32 = 0xC00D36B5_u32 as i32;

/// `MFVideoTransferMatrix_BT709` / `_BT601`.
const MATRIX_BT709: u32 = 1;
const MATRIX_BT601: u32 = 2;

/// How the YCbCr in a decoded picture maps to RGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub bt709: bool,
    pub full_range: bool,
}

/// One decoded picture.
pub struct Picture {
    /// Visible size (the decoder pads the coded size to whole macroblocks).
    pub width: u32,
    pub height: u32,
    pub color: Color,
    pub data: PictureData,
}

pub enum PictureData {
    /// A decoder surface in video memory (a slice of a texture array).
    Gpu {
        texture: ID3D11Texture2D,
        subresource: u32,
    },
    /// NV12 in system memory: `stride` bytes per row, the chroma plane
    /// starting `stride * rows` bytes in, where `rows` is the padded height.
    Cpu {
        bytes: Vec<u8>,
        stride: usize,
        rows: usize,
    },
}

pub struct Decoder {
    mft: IMFTransform,
    /// Kept alive for as long as the MFT may use it.
    _manager: Option<IMFDXGIDeviceManager>,
    hardware: bool,
    out: OutputFormat,
    /// The MFT allocates its own output samples (DXVA); otherwise one is
    /// reused here.
    provides_samples: bool,
    scratch: Option<IMFSample>,
    next_time: i64,
}

#[derive(Debug, Clone, Copy)]
struct OutputFormat {
    width: u32,
    height: u32,
    stride: usize,
    color: Color,
}

impl Decoder {
    /// `manager`: the DXGI device manager of the device the pictures are
    /// presented on; `None` forces software decode.
    pub fn new(manager: Option<&IMFDXGIDeviceManager>) -> Result<Decoder> {
        let mft: IMFTransform =
            unsafe { CoCreateInstance(&CLSID_H264_DECODER, None, CLSCTX_INPROC_SERVER) }
                .context("creating the Microsoft H.264 decoder (is Media Foundation installed? N editions of Windows need the Media Feature Pack)")?;
        unsafe {
            let attrs = mft.GetAttributes().context("decoder attributes")?;
            // Output as soon as a picture is decodable.
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        let mut hardware = false;
        let mut kept = None;
        if let Some(m) = manager {
            let ok =
                unsafe { mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, m.as_raw() as usize) }
                    .is_ok();
            if ok {
                hardware = true;
                kept = Some(m.clone());
            }
        }
        let mut dec = Decoder {
            mft,
            _manager: kept,
            hardware,
            out: OutputFormat {
                width: 0,
                height: 0,
                stride: 0,
                color: Color {
                    bt709: true,
                    full_range: false,
                },
            },
            provides_samples: false,
            scratch: None,
            next_time: 0,
        };
        dec.set_input_type()?;
        // The output types are only known once an SPS has been seen on some
        // MFTs; ignore a miss here and negotiate on the first stream change.
        let _ = dec.negotiate_output();
        unsafe {
            dec.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("begin streaming")?;
            dec.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("start of stream")?;
        }
        Ok(dec)
    }

    /// Decoding on the GPU (DXVA) rather than the CPU.
    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    fn set_input_type(&self) -> Result<()> {
        unsafe {
            let t = MFCreateMediaType().context("MFCreateMediaType")?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            self.mft
                .SetInputType(0, &t, 0)
                .context("H.264 input type rejected")
        }
    }

    /// Pick NV12 as the output and read what the decoder says about it.
    fn negotiate_output(&mut self) -> Result<()> {
        unsafe {
            let mut chosen = None;
            for i in 0.. {
                let Ok(t) = self.mft.GetOutputAvailableType(0, i) else {
                    break;
                };
                if t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    chosen = Some(t);
                    break;
                }
            }
            let t = chosen.ok_or_else(|| anyhow!("the decoder offers no NV12 output"))?;
            self.mft
                .SetOutputType(0, &t, 0)
                .context("NV12 output type rejected")?;
            let t = self.mft.GetOutputCurrentType(0)?;
            let size = t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            let (w, h) = ((size >> 32) as u32, size as u32);
            let stride = t
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map(|s| s as i32)
                .ok()
                .filter(|s| *s > 0)
                .map(|s| s as usize)
                .unwrap_or(w as usize);
            // Pictures with no colour tags: BT.709 for HD, BT.601 for SD, as
            // players do. The Mac sender tags BT.709 in the SPS VUI.
            let bt709 = match t.GetUINT32(&MF_MT_YUV_MATRIX) {
                Ok(MATRIX_BT709) => true,
                Ok(MATRIX_BT601) => false,
                _ => h >= 720 || w >= 1280,
            };
            // MFNominalRange_0_255 = 1; everything else is studio range.
            let full_range = t.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE) == Ok(1);
            self.out = OutputFormat {
                width: w,
                height: h,
                stride,
                color: Color { bt709, full_range },
            };
            let info = self.mft.GetOutputStreamInfo(0)?;
            let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            let can_provide = info.dwFlags & MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32 != 0;
            self.provides_samples = provides || (can_provide && self.hardware);
            self.scratch = if self.provides_samples {
                None
            } else {
                let buf = MFCreateAlignedMemoryBuffer(info.cbSize, info.cbAlignment)?;
                let s = MFCreateSample()?;
                s.AddBuffer(&buf)?;
                Some(s)
            };
        }
        Ok(())
    }

    /// Decode one access unit; returns the newest picture it produced, if any.
    /// Earlier pictures from the same call are dropped (latest wins, §5).
    pub fn decode(&mut self, annexb: &[u8]) -> Result<Option<Picture>> {
        let sample = unsafe { self.input_sample(annexb)? };
        let mut latest = None;
        let mut fed = false;
        // One retry: a decoder that is full wants its output taken first.
        for _ in 0..2 {
            match unsafe { self.mft.ProcessInput(0, &sample, 0) } {
                Ok(()) => {
                    fed = true;
                    break;
                }
                Err(e) if e.code().0 == MF_E_NOTACCEPTING => {
                    if let Some(p) = self.drain()? {
                        latest = Some(p);
                    }
                }
                Err(e) => return Err(e).context("decoder input"),
            }
        }
        if !fed {
            bail!("the decoder keeps refusing input");
        }
        if let Some(p) = self.drain()? {
            latest = Some(p);
        }
        Ok(latest)
    }

    unsafe fn input_sample(&mut self, annexb: &[u8]) -> Result<IMFSample> {
        unsafe {
            let buf = MFCreateMemoryBuffer(annexb.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buf.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(annexb.as_ptr(), ptr, annexb.len());
            buf.Unlock()?;
            buf.SetCurrentLength(annexb.len() as u32)?;
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            // 100 ns units; the stream has no timestamps (§5.1), so count
            // frames at 60 Hz. Only the order matters to the decoder.
            s.SetSampleTime(self.next_time)?;
            s.SetSampleDuration(166_666)?;
            self.next_time += 166_666;
            Ok(s)
        }
    }

    /// Take every picture the decoder has ready; keep the last.
    fn drain(&mut self) -> Result<Option<Picture>> {
        let mut latest = None;
        loop {
            let sample = match self.scratch.clone() {
                Some(s) if !self.provides_samples => Some(s),
                _ => None,
            };
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = unsafe { self.mft.ProcessOutput(0, &mut out, &mut status) };
            // Take ownership back so the COM references are released.
            let produced = unsafe { ManuallyDrop::take(&mut out[0].pSample) };
            unsafe { ManuallyDrop::drop(&mut out[0].pEvents) };
            let format_changed =
                out[0].dwStatus & MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE.0 as u32 != 0;
            match r {
                Ok(()) if format_changed => {
                    // Older MFTs flag the change in the status instead.
                    self.negotiate_output()
                        .context("renegotiating the decoder output after a format change")?;
                }
                Ok(()) => {
                    let Some(sample) = produced else { continue };
                    latest = Some(self.picture(&sample)?);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // New SPS: a new size (or the first one).
                    self.negotiate_output()
                        .context("renegotiating the decoder output after a stream change")?;
                }
                Err(e) => return Err(e).context("decoder output"),
            }
        }
        Ok(latest)
    }

    fn picture(&self, sample: &IMFSample) -> Result<Picture> {
        let o = self.out;
        unsafe {
            let buffer = sample.GetBufferByIndex(0)?;
            if let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() {
                let mut raw = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
                if !raw.is_null() {
                    let texture = ID3D11Texture2D::from_raw(raw);
                    return Ok(Picture {
                        width: o.width,
                        height: o.height,
                        color: o.color,
                        data: PictureData::Gpu {
                            texture,
                            subresource: dxgi.GetSubresourceIndex()?,
                        },
                    });
                }
            }
            let contiguous = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            contiguous.Lock(&mut ptr, None, Some(&mut len))?;
            let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            contiguous.Unlock()?;
            let stride = o.stride.max(o.width as usize);
            // NV12 is 1.5 bytes per pixel; the buffer length gives the padded
            // row count without guessing the decoder's alignment.
            let rows = (len as usize * 2 / 3 / stride).max(o.height as usize);
            Ok(Picture {
                width: o.width,
                height: o.height,
                color: o.color,
                data: PictureData::Cpu {
                    bytes,
                    stride,
                    rows,
                },
            })
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            // Detach the device manager before it goes away.
            if self._manager.is_some() {
                let _ = self.mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
            }
        }
    }
}
