//! Windows' own H.264 decoder (Media Foundation), for the viewer: openh264 can't keep up with an
//! HW4 front stream in busy scenes. With a D3D11 device it decodes on the GPU, and pictures come
//! back to memory only when shown. The DLLs are missing on Windows N and some Server editions;
//! `MfDecoder::new` fails there and the caller moves on (build.rs delay-loads them). Export encodes
//! on the GPU's H.264 encoder through it too (`MfEncoder`), where the machine has one.

use crate::mp4::Track;
use std::cell::{OnceCell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::OnceLock;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIAdapter;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::LibraryLoader::LoadLibraryW;
use windows::Win32::System::Variant::{VARIANT, VT_UI4};
use windows::core::{Error, GUID, Interface, Result, w};

/// CLSID_CMSH264DecoderMFT from wmcodecdsp.h.
const H264_DECODER: GUID = GUID::from_u128(0x62ce7e72_4c71_4d20_b15d_452831a87d9d);
/// Samples without a picture before MF counts as not working here: low latency means one in, one out.
pub const MAX_SILENT: usize = 8;

/// The visible part of the coded picture.
#[derive(Clone, Copy, Default)]
struct Crop {
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
}

/// One NV12 picture in memory, cropped.
struct Planes<'a> {
    data: &'a [u8],
    w: usize,
    h: usize,
    stride: usize,
    y0: usize,
    uv0: usize,
}

impl<'a> Planes<'a> {
    /// `rows`: coded height, where the chroma plane starts. The layout comes from the decoder, so
    /// it's checked here: conversion never indexes past what it wrote.
    fn new(data: &'a [u8], crop: Crop, stride: usize, rows: usize) -> Result<Planes<'a>> {
        let y0 = crop.y0 * stride + crop.x0;
        let uv0 = rows * stride + crop.y0 / 2 * stride + (crop.x0 & !1);
        let last_y = y0 + crop.h.saturating_sub(1) * stride + crop.w;
        let last_uv = uv0 + crop.h.div_ceil(2).saturating_sub(1) * stride + crop.w.div_ceil(2) * 2;
        if crop.w == 0 || crop.h == 0 || stride < crop.x0 + crop.w || last_y > data.len() || last_uv > data.len() {
            return Err(Error::from_hresult(MF_E_INVALIDMEDIATYPE));
        }
        Ok(Planes { data, w: crop.w, h: crop.h, stride, y0, uv0 })
    }

    /// Writes the picture as little-endian RGBA, opaque. `out` holds `w * h` pixels.
    fn write_rgba(&self, out: &mut [u32]) {
        #[cfg(target_arch = "x86_64")]
        if is_x86_feature_detected!("avx2") {
            // Safety: AVX2 was just detected.
            return unsafe { self.write_rgba_avx2(out) };
        }
        self.convert(out);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    fn write_rgba_avx2(&self, out: &mut [u32]) {
        self.convert(out);
    }

    /// Limited-range BT.601, the same picture as openh264's `write_rgba8` to within rounding, so
    /// a clip looks the same on either decoder. Written for the compiler to vectorize.
    #[inline(always)]
    fn convert(&self, out: &mut [u32]) {
        // 13-bit fixed point: 255/219, and the chroma factors of openh264's yuv2rgb.
        const Y: i32 = 9539;
        const RV: i32 = 13075;
        const GV: i32 = -6660;
        const GU: i32 = -3209;
        const BU: i32 = 16525;
        let w = self.w;
        let cw = w.div_ceil(2);
        // Chroma offsets at half width, then doubled: plain loops over equal-length slices vectorize.
        let (mut cr, mut cg, mut cb) = (vec![0i32; cw], vec![0i32; cw], vec![0i32; cw]);
        let (mut r, mut g, mut b) = (vec![0i32; cw * 2], vec![0i32; cw * 2], vec![0i32; cw * 2]);
        for (row, px) in out.chunks_exact_mut(w).take(self.h).enumerate() {
            if row % 2 == 0 {
                let uv = &self.data[self.uv0 + row / 2 * self.stride..][..cw * 2];
                for (((c, r), g), b) in uv.chunks_exact(2).zip(&mut cr).zip(&mut cg).zip(&mut cb) {
                    let (u, v) = (c[0] as i32 - 128, c[1] as i32 - 128);
                    *r = RV * v + 4096;
                    *g = GV * v + GU * u + 4096;
                    *b = BU * u + 4096;
                }
                double(&cr, &mut r);
                double(&cg, &mut g);
                double(&cb, &mut b);
            }
            let luma = &self.data[self.y0 + row * self.stride..][..w];
            let (r, g, b, px) = (&r[..w], &g[..w], &b[..w], &mut px[..w]);
            for i in 0..w {
                let l = (luma[i] as i32 - 16) * Y;
                let (r, g, b) = ((l + r[i]) >> 13, (l + g[i]) >> 13, (l + b[i]) >> 13);
                px[i] = r.clamp(0, 255) as u32 | (g.clamp(0, 255) as u32) << 8 | (b.clamp(0, 255) as u32) << 16 | 0xff00_0000;
            }
        }
    }
}

#[inline(always)]
fn double(half: &[i32], full: &mut [i32]) {
    for (d, &v) in full.chunks_exact_mut(2).zip(half) {
        d[0] = v;
        d[1] = v;
    }
}

/// A CPU-readable copy target for decoded surfaces.
struct Readback {
    context: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    width: u32,
    rows: u32,
}

impl Readback {
    /// Copies a decoded surface over and hands `read` its bytes, stride and rows.
    fn read<T>(&self, texture: &ID3D11Texture2D, index: u32, read: impl FnOnce(&[u8], usize, usize) -> T) -> Result<T> {
        // Safety: both textures are NV12 of the same size on this device, which is multithread
        // protected against the decoder. The slice is NV12's size at the pitch Map reports, and is
        // only used before Unmap.
        unsafe {
            self.context.CopySubresourceRegion(&self.staging, 0, 0, 0, 0, texture, index, None);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let (stride, rows) = (m.RowPitch as usize, self.rows as usize);
            let data = std::slice::from_raw_parts(m.pData.cast::<u8>(), stride * rows * 3 / 2);
            let out = read(data, stride, rows);
            self.context.Unmap(&self.staging, 0);
            Ok(out)
        }
    }
}

/// The GPU side of decoding, one per decoder thread: decoders come and go with every seek.
struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    manager: IMFDXGIDeviceManager,
    readback: RefCell<Option<Rc<Readback>>>,
}

impl Gpu {
    fn new() -> Result<Gpu> {
        // Safety: plain API calls with valid out-pointers. The DLL check is what makes the
        // delay-load safe, as in `startup`.
        unsafe {
            LoadLibraryW(w!("d3d11.dll"))?;
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.ok_or_else(Error::empty)?, context.ok_or_else(Error::empty)?);
            // This thread copies from surfaces while the decoder's own threads use the context.
            let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
            let (mut token, mut manager) = (0, None);
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.ok_or_else(Error::empty)?;
            manager.ResetDevice(&device, token)?;
            Ok(Gpu { device, context, manager, readback: RefCell::new(None) })
        }
    }

    /// This thread's device, if the machine has one that works.
    fn current() -> Option<Rc<Gpu>> {
        thread_local! { static GPU: OnceCell<Option<Rc<Gpu>>> = const { OnceCell::new() }; }
        GPU.with(|g| g.get_or_init(|| Gpu::new().ok().map(Rc::new)).clone())
    }

    /// A staging texture to suit `texture`, kept between decoders: it's megabytes of GPU memory.
    fn readback(&self, texture: &ID3D11Texture2D) -> Result<Rc<Readback>> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // Safety: a plain getter into a struct of the right type.
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_NV12 {
            return Err(Error::from_hresult(MF_E_INVALIDMEDIATYPE));
        }
        let mut slot = self.readback.borrow_mut();
        if let Some(r) = slot.as_ref().filter(|r| r.width == desc.Width && r.rows == desc.Height) {
            return Ok(r.clone());
        }
        let staging = D3D11_TEXTURE2D_DESC {
            Width: desc.Width,
            Height: desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut out = None;
        // Safety: a valid description and out-pointer.
        unsafe { self.device.CreateTexture2D(&staging, None, Some(&mut out))? };
        let r = Rc::new(Readback { context: self.context.clone(), staging: out.ok_or_else(Error::empty)?, width: desc.Width, rows: desc.Height });
        *slot = Some(r.clone());
        Ok(r)
    }
}

enum Pixels {
    Memory { data: Vec<u8>, stride: usize, rows: usize },
    /// Still on the GPU. Holding the sample keeps the decoder from reusing the surface.
    Surface { _sample: IMFSample, texture: ID3D11Texture2D, index: u32, readback: Rc<Readback> },
}

/// One decoded picture.
pub struct Frame {
    pub pts: u64,
    crop: Crop,
    pixels: Pixels,
}

impl Frame {
    pub fn size(&self) -> [usize; 2] {
        [self.crop.w, self.crop.h]
    }

    /// Writes the picture as little-endian RGBA, opaque. `out` holds `w * h` pixels.
    pub fn write_rgba(&self, out: &mut [u32]) -> Result<()> {
        match &self.pixels {
            Pixels::Memory { data, stride, rows } => Planes::new(data, self.crop, *stride, *rows)?.write_rgba(out),
            Pixels::Surface { texture, index, readback, .. } => {
                readback.read(texture, *index, |data, stride, rows| Planes::new(data, self.crop, stride, rows).map(|p| p.write_rgba(out)))??;
            }
        }
        Ok(())
    }
}

/// True once Media Foundation is up in this process. It is never shut down: decoders come and go
/// for the life of the app.
fn startup() -> bool {
    static UP: OnceLock<bool> = OnceLock::new();
    *UP.get_or_init(|| {
        // Safety: plain library calls. Checking first is what makes the delay-load safe: calling
        // into a missing delay-loaded DLL raises an exception instead of returning an error.
        unsafe { LoadLibraryW(w!("mfplat.dll")).is_ok() && MFStartup(MF_VERSION, MFSTARTUP_LITE).is_ok() }
    })
}

pub struct MfDecoder {
    mft: IMFTransform,
    gpu: Option<Rc<Gpu>>,
    /// SPS and PPS in Annex-B form, sent before every key frame so decoding can start at any of them.
    params: Vec<u8>,
    nal_len: usize,
    annex_b: Vec<u8>,
    /// Where pictures go, unless the decoder provides its own samples.
    output: Option<IMFSample>,
    /// Display size from the container, used when the decoder reports no aperture.
    size: (usize, usize),
    crop: Crop,
    stride: usize,
    rows: usize,
    ready: VecDeque<Frame>,
    /// The picture the last `decode` returned.
    current: Option<Frame>,
    /// Frame memory to use again: each picture would otherwise fault in megabytes of fresh pages.
    spare: Vec<Vec<u8>>,
    flushed: Vec<Frame>,
    /// Samples fed before any picture came out; `None` once one has.
    pub silent: Option<usize>,
    /// A picture has come back on a GPU surface. The decoder itself may fall back to software.
    pub on_gpu: bool,
}

impl MfDecoder {
    /// `gpu`: decode on the GPU, failing if the machine can't set that up.
    pub fn new(track: &Track, gpu: bool) -> Result<MfDecoder> {
        if !startup() {
            return Err(Error::from_hresult(MF_E_NOT_INITIALIZED));
        }
        let gpu = if gpu { Some(Gpu::current().ok_or_else(|| Error::from_hresult(MF_E_NOT_INITIALIZED))?) } else { None };
        // Safety: COM calls with valid arguments. COM stays initialized for the decoder thread.
        let mft: IMFTransform = unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            CoCreateInstance(&H264_DECODER, None, CLSCTX_INPROC_SERVER)?
        };
        // Safety: as above; the media type and device manager outlive the calls that take them.
        unsafe {
            let attributes = mft.GetAttributes()?;
            // Clips with reordered frames never get here, so nothing needs to be held back.
            attributes.SetUINT32(&CODECAPI_AVLowLatencyMode, 1)?;
            if let Some(g) = &gpu {
                if attributes.GetUINT32(&MF_SA_D3D11_AWARE)? == 0 {
                    return Err(Error::from_hresult(MF_E_UNSUPPORTED_D3D_TYPE));
                }
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, g.manager.as_raw() as usize)?;
            }
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            // No frame size: the decoder takes the coded (16-aligned) one from the SPS.
            mft.SetInputType(0, &input, 0)?;
        }
        let mut d = MfDecoder {
            mft,
            gpu,
            params: crate::decode::annex_b_params(track),
            nal_len: track.nal_len as usize,
            annex_b: Vec::new(),
            output: None,
            size: (track.width as usize, track.height as usize),
            crop: Crop::default(),
            stride: 0,
            rows: 0,
            ready: VecDeque::new(),
            current: None,
            spare: Vec::new(),
            flushed: Vec::new(),
            silent: Some(0),
            on_gpu: false,
        };
        d.negotiate()?;
        // Safety: messages without parameters.
        unsafe {
            d.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            d.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(d)
    }

    /// Picks NV12 output and notes where the planes are. Runs again when the stream's SPS changes
    /// what the decoder assumed at first.
    fn negotiate(&mut self) -> Result<()> {
        // Safety: COM calls with valid arguments; the blob is read into a struct of its own size.
        unsafe {
            let mut i = 0;
            let ty = loop {
                let ty = self.mft.GetOutputAvailableType(0, i)?;
                if ty.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    break ty;
                }
                i += 1;
            };
            self.mft.SetOutputType(0, &ty, 0)?;
            let coded = ty.GetUINT64(&MF_MT_FRAME_SIZE)?;
            let (cw, ch) = ((coded >> 32) as usize, coded as u32 as usize);
            self.rows = ch;
            self.stride = ty.GetUINT32(&MF_MT_DEFAULT_STRIDE).map_or(cw, |s| s as i32 as usize);
            let mut area = MFVideoArea::default();
            let blob = std::slice::from_raw_parts_mut((&raw mut area).cast::<u8>(), size_of::<MFVideoArea>());
            self.crop = match ty.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, blob, None) {
                Ok(()) if area.Area.cx > 0 && area.Area.cy > 0 => {
                    Crop { x0: area.OffsetX.value as usize, y0: area.OffsetY.value as usize, w: area.Area.cx as usize, h: area.Area.cy as usize }
                }
                _ => Crop { x0: 0, y0: 0, w: self.size.0.min(cw), h: self.size.1.min(ch) },
            };
            let info = self.mft.GetOutputStreamInfo(0)?;
            self.output = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0 {
                None
            } else {
                let buffer = MFCreateAlignedMemoryBuffer(info.cbSize, info.cbAlignment.saturating_sub(1))?;
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                Some(sample)
            };
        }
        Ok(())
    }

    /// Moves every picture the decoder has ready into `ready`.
    fn drain(&mut self) -> Result<()> {
        loop {
            let mut out = MFT_OUTPUT_DATA_BUFFER { pSample: std::mem::ManuallyDrop::new(self.output.clone()), ..Default::default() };
            // Safety: `out` holds a sample with a buffer of the size the decoder asked for, or
            // none if it provides its own. Both fields are owned references the call leaves in
            // the struct. A buffer still marked full makes the decoder fail the next copy.
            let (r, sample) = unsafe {
                if let Some(s) = &self.output {
                    s.GetBufferByIndex(0)?.SetCurrentLength(0)?;
                }
                let mut status = 0;
                let r = self.mft.ProcessOutput(0, std::slice::from_mut(&mut out), &mut status);
                std::mem::ManuallyDrop::drop(&mut out.pEvents);
                (r, std::mem::ManuallyDrop::take(&mut out.pSample))
            };
            match r {
                Ok(()) => {
                    let frame = self.picture(sample.ok_or_else(Error::empty)?)?;
                    self.silent = None;
                    self.ready.push_back(frame);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.negotiate()?,
                Err(e) => return Err(e),
            }
        }
    }

    fn picture(&mut self, sample: IMFSample) -> Result<Frame> {
        // Safety: COM calls with valid arguments. The raw texture pointer is an owned reference.
        // The locked slice covers what Lock says it locked, and is copied before Unlock.
        unsafe {
            let pts = sample.GetSampleTime()? as u64;
            let buffer = sample.GetBufferByIndex(0)?;
            if let (Ok(dxgi), Some(gpu)) = (buffer.cast::<IMFDXGIBuffer>(), &self.gpu) {
                let mut raw = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
                let texture = ID3D11Texture2D::from_raw(raw);
                let readback = gpu.readback(&texture)?;
                if self.crop.x0 + self.crop.w > readback.width as usize || self.crop.y0 + self.crop.h > readback.rows as usize {
                    return Err(Error::from_hresult(MF_E_INVALIDMEDIATYPE));
                }
                self.on_gpu = true;
                let index = dxgi.GetSubresourceIndex()?;
                return Ok(Frame { pts, crop: self.crop, pixels: Pixels::Surface { _sample: sample, texture, index, readback } });
            }
            let (mut p, mut len) = (std::ptr::null_mut(), 0);
            buffer.Lock(&mut p, None, Some(&mut len))?;
            let mut data = self.spare.pop().unwrap_or_default();
            data.clear();
            data.extend_from_slice(std::slice::from_raw_parts(p, len as usize));
            buffer.Unlock()?;
            Planes::new(&data, self.crop, self.stride, self.rows)?;
            Ok(Frame { pts, crop: self.crop, pixels: Pixels::Memory { data, stride: self.stride, rows: self.rows } })
        }
    }

    /// Feeds one AVCC sample. `None`: no picture ready yet. Pictures come back in order, one per call.
    pub fn decode(&mut self, sample: &[u8], key: bool, pts: u64, dur: u32) -> std::result::Result<Option<&Frame>, String> {
        crate::decode::annex_b(&mut self.annex_b, if key { &self.params } else { &[] }, self.nal_len, sample)?;
        let e = |e: Error| format!("media foundation: {e}");
        self.feed(key, pts, dur).map_err(e)?;
        if let Some(n) = &mut self.silent {
            *n += 1;
        }
        self.drain().map_err(e)?;
        let Some(next) = self.ready.pop_front() else { return Ok(None) };
        if let Some(Frame { pixels: Pixels::Memory { data, .. }, .. }) = self.current.replace(next) {
            self.spare.push(data);
        }
        Ok(self.current.as_ref())
    }

    fn feed(&mut self, key: bool, pts: u64, dur: u32) -> Result<()> {
        // Safety: the buffer is locked only while copying `annex_b`, which fits its allocation.
        let input = unsafe {
            let buffer = MFCreateMemoryBuffer(self.annex_b.len() as u32)?;
            let mut p = std::ptr::null_mut();
            buffer.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(self.annex_b.as_ptr(), p, self.annex_b.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(self.annex_b.len() as u32)?;
            let input = MFCreateSample()?;
            input.AddBuffer(&buffer)?;
            input.SetSampleTime(pts as i64)?;
            input.SetSampleDuration(dur as i64)?;
            input.SetUINT32(&MFSampleExtension_CleanPoint, key as u32)?;
            input
        };
        loop {
            // Safety: the sample stays alive for the call.
            match unsafe { self.mft.ProcessInput(0, &input, 0) } {
                // Pictures are in the way: take them and offer the sample again.
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    let held = self.ready.len();
                    self.drain()?;
                    if self.ready.len() == held {
                        return Err(e);
                    }
                }
                r => return r,
            }
        }
    }

    /// Pictures still held by the decoder; call after the last sample.
    pub fn flush(&mut self) -> std::result::Result<&[Frame], String> {
        let e = |e: Error| format!("media foundation: {e}");
        // Safety: a message without parameters.
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }.map_err(e)?;
        self.drain().map_err(e)?;
        self.flushed = self.ready.drain(..).collect();
        Ok(&self.flushed)
    }
}

/// The GPU's H.264 encoders (NVENC, AMF, Quick Sync behind Media Foundation), best first. Needs COM
/// on the calling thread.
fn hardware_encoders() -> Vec<IMFActivate> {
    if !startup() {
        return Vec::new();
    }
    let ty = |subtype| MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype };
    let (input, output) = (ty(MFVideoFormat_NV12), ty(MFVideoFormat_H264));
    let (mut list, mut n) = (std::ptr::null_mut(), 0);
    // Safety: valid type infos and out-pointers. Each entry is taken out of the array, which is then
    // freed as MFTEnumEx's contract says.
    unsafe {
        let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
        if MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut list, &mut n).is_err() || list.is_null() {
            return Vec::new();
        }
        let found = std::slice::from_raw_parts_mut(list, n as usize).iter_mut().filter_map(Option::take).collect();
        CoTaskMemFree(Some(list as *const _));
        found
    }
}

/// The name of the GPU's H.264 encoder, if Media Foundation lists one. Looked up once, on a thread of
/// its own so the UI thread's COM apartment is left alone.
pub fn hardware_encoder() -> Option<&'static str> {
    static NAME: OnceLock<Option<String>> = OnceLock::new();
    NAME.get_or_init(|| {
        std::thread::spawn(|| {
            // Safety: COM for this short-lived thread; the string is copied before it's freed.
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let a = hardware_encoders().into_iter().next()?;
                let (mut s, mut len) = (windows::core::PWSTR::null(), 0);
                a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut s, &mut len).ok()?;
                let name = s.to_string().ok();
                CoTaskMemFree(Some(s.0 as *const _));
                Some(name.unwrap_or_else(|| "GPU encoder".to_owned()))
            }
        })
        .join()
        .ok()
        .flatten()
    })
    .as_deref()
}

/// Sets an encoder property to a number.
unsafe fn set_u32(api: &ICodecAPI, key: &GUID, n: u32) -> Result<()> {
    let mut v = VARIANT::default();
    // Safety: a VT_UI4 variant holds its value in ulVal; it owns nothing to free.
    unsafe {
        (*v.Anonymous.Anonymous).vt = VT_UI4;
        (*v.Anonymous.Anonymous).Anonymous.ulVal = n;
        api.SetValue(key, &v)
    }
}

/// H.264 on the GPU, for export: NV12 pictures in, Annex-B access units out. Hardware encoders are
/// asynchronous, so a picture comes out some calls after it went in, and the GPU encodes while the
/// caller composes the next frame.
pub struct MfEncoder {
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    /// Input the encoder asked for and hasn't had.
    wanted: u32,
    /// Times of pictures sent and not back yet: they must come back in this order (no B-frames).
    sent: VecDeque<i64>,
    /// Where output goes, unless the encoder provides its own samples.
    output: Option<IMFSample>,
    nv12_len: usize,
}

impl MfEncoder {
    /// The first hardware encoder that takes these settings; the error of the last one tried if none.
    pub fn new(width: u32, height: u32, bps: u32, fps: u32) -> Result<MfEncoder> {
        // Safety: COM stays initialized for the export thread.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let mut last = Error::from_hresult(MF_E_NOT_FOUND);
        for a in hardware_encoders() {
            match Self::with(&a, width, height, bps, fps) {
                Ok(e) => return Ok(e),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn with(a: &IMFActivate, width: u32, height: u32, bps: u32, fps: u32) -> Result<MfEncoder> {
        // Safety: COM calls with valid arguments; the media types outlive the calls that take them.
        unsafe {
            let mft: IMFTransform = a.ActivateObject()?;
            mft.GetAttributes()?.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let events: IMFMediaEventGenerator = mft.cast()?;
            let video = |subtype: &GUID| -> Result<IMFMediaType> {
                let t = MFCreateMediaType()?;
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
                t.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | height as u64)?;
                t.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1)?;
                t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1 << 32 | 1)?;
                t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                Ok(t)
            };
            // No B-frames: pictures must come out in the order they went in, as the muxer takes them.
            // An encoder that can't be told so isn't used. A keyframe every second, as openh264's.
            let api: ICodecAPI = mft.cast()?;
            if api.IsSupported(&CODECAPI_AVEncMPVDefaultBPictureCount).is_ok() {
                set_u32(&api, &CODECAPI_AVEncMPVDefaultBPictureCount, 0)?;
            }
            let _ = set_u32(&api, &CODECAPI_AVEncMPVGOPSize, fps);
            // Encoders take the output type first. High, not Baseline: NVENC's Baseline has CAVLC
            // escapes only High allows, which openh264 can't read.
            let out = video(&MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bps)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            mft.SetOutputType(0, &out, 0)?;
            let input = video(&MFVideoFormat_NV12)?;
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width)?;
            mft.SetInputType(0, &input, 0)?;
            let info = mft.GetOutputStreamInfo(0)?;
            let output = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0 {
                None
            } else {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(info.cbSize)?)?;
                Some(sample)
            };
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let nv12_len = width as usize * height as usize * 3 / 2;
            Ok(MfEncoder { mft, events, wanted: 0, sent: VecDeque::new(), output, nv12_len })
        }
    }

    /// Sends one NV12 picture (`width` stride) at `time` for `dur` (100 ns units). Returns the access
    /// units that came out meanwhile, oldest first.
    // shortcut: waits on the encoder's events without a timeout, so a driver that stops answering hangs the export; add one if that's ever seen.
    pub fn encode(&mut self, nv12: &[u8], time: i64, dur: i64) -> Result<Vec<Vec<u8>>> {
        if nv12.len() != self.nv12_len {
            return Err(Error::from_hresult(MF_E_INVALIDMEDIATYPE));
        }
        let mut out = Vec::new();
        while self.wanted == 0 {
            // Safety: a blocking wait on the encoder's own event queue.
            let e = unsafe { self.events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) }?;
            self.on(&e, &mut out)?;
        }
        // Safety: the buffer is locked only while copying `nv12`, which fits its allocation.
        unsafe {
            let buffer = MFCreateMemoryBuffer(nv12.len() as u32)?;
            let mut p = std::ptr::null_mut();
            buffer.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), p, nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(nv12.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(dur)?;
            self.mft.ProcessInput(0, &sample, 0)?;
        }
        self.wanted -= 1;
        self.sent.push_back(time);
        loop {
            // Safety: as above, without waiting.
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => self.on(&e, &mut out)?,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(out),
                Err(e) => return Err(e),
            }
        }
    }

    /// The access units still in the encoder; call after the last picture.
    pub fn finish(&mut self) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        // Safety: messages without parameters, then blocking waits on the event queue.
        unsafe {
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
            loop {
                let e = self.events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))?;
                if e.GetType()? == METransformDrainComplete.0 as u32 {
                    e.GetStatus()?.ok()?;
                    return Ok(out);
                }
                self.on(&e, &mut out)?;
            }
        }
    }

    fn on(&mut self, e: &IMFMediaEvent, out: &mut Vec<Vec<u8>>) -> Result<()> {
        // Safety: getters on a live event.
        let (ty, status) = unsafe { (e.GetType()?, e.GetStatus()?) };
        status.ok()?;
        let ty = MF_EVENT_TYPE(ty as i32);
        if ty == METransformNeedInput {
            self.wanted += 1;
        } else if ty == METransformHaveOutput {
            out.push(self.take()?);
        }
        Ok(())
    }

    /// One access unit, once the encoder said it has it.
    fn take(&mut self) -> Result<Vec<u8>> {
        loop {
            let mut buf = MFT_OUTPUT_DATA_BUFFER { pSample: std::mem::ManuallyDrop::new(self.output.clone()), ..Default::default() };
            // Safety: as in `MfDecoder::drain`.
            let (r, sample) = unsafe {
                if let Some(s) = &self.output {
                    s.GetBufferByIndex(0)?.SetCurrentLength(0)?;
                }
                let mut status = 0;
                let r = self.mft.ProcessOutput(0, std::slice::from_mut(&mut buf), &mut status);
                std::mem::ManuallyDrop::drop(&mut buf.pEvents);
                (r, std::mem::ManuallyDrop::take(&mut buf.pSample))
            };
            match r {
                // The encoder settled its output format: take it and ask again.
                // Safety: a type the encoder offered.
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => unsafe { self.mft.SetOutputType(0, &self.mft.GetOutputAvailableType(0, 0)?, 0)? },
                Err(e) => return Err(e),
                Ok(()) => {
                    let sample = sample.ok_or_else(Error::empty)?;
                    // Safety: the locked slice covers what Lock says, and is copied before Unlock.
                    let (time, data) = unsafe {
                        let buffer = sample.ConvertToContiguousBuffer()?;
                        let (mut p, mut len) = (std::ptr::null_mut(), 0);
                        buffer.Lock(&mut p, None, Some(&mut len))?;
                        let data = std::slice::from_raw_parts(p, len as usize).to_vec();
                        buffer.Unlock()?;
                        (sample.GetSampleTime()?, data)
                    };
                    if self.sent.pop_front() != Some(time) {
                        return Err(Error::new(MF_E_INVALIDMEDIATYPE, "the GPU encoder put pictures out of order"));
                    }
                    return Ok(data);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8x2 picture with a 10-byte stride, as decoders pad rows.
    fn nv12(y: u8, u: u8, v: u8) -> Vec<u8> {
        let mut data = vec![0xee; 30];
        data[..8].fill(y);
        data[10..18].fill(y);
        for c in data[20..28].chunks_exact_mut(2) {
            c.copy_from_slice(&[u, v]);
        }
        data
    }

    const CROP: Crop = Crop { x0: 0, y0: 0, w: 8, h: 2 };

    /// The reference values of openh264's own converter tests.
    #[test]
    fn conversion_matches_openh264() {
        for (yuv, want) in [
            ((235, 128, 128), [255, 255, 255]),
            ((16, 128, 128), [0, 0, 0]),
            ((235, 240, 240), [255, 120, 255]),
            ((235, 0, 240), [255, 214, 0]),
            ((235, 240, 0), [50, 255, 255]),
        ] {
            let data = nv12(yuv.0, yuv.1, yuv.2);
            let mut out = [0u32; 16];
            Planes::new(&data, CROP, 10, 2).unwrap().write_rgba(&mut out);
            for p in out {
                let [r, g, b, a] = p.to_le_bytes();
                assert_eq!(a, 255);
                for (got, want) in [r, g, b].into_iter().zip(want) {
                    assert!(got.abs_diff(want) <= 1, "{yuv:?}: got {r},{g},{b}, want {want:?}");
                }
            }
        }
    }

    /// A layout the decoder got wrong must not reach the conversion, whose indexing would panic.
    #[test]
    fn impossible_layouts_are_refused() {
        let data = nv12(16, 128, 128);
        assert!(Planes::new(&data, CROP, 10, 2).is_ok());
        for (crop, stride, rows) in [
            (Crop { w: 0, ..CROP }, 10, 2),
            (Crop { h: 0, ..CROP }, 10, 2),
            (Crop { w: 11, ..CROP }, 10, 2),
            (Crop { h: 4, ..CROP }, 10, 2),
            (Crop { x0: 4, ..CROP }, 10, 2),
            (CROP, 10, 3),
            (CROP, 7, 2),
        ] {
            assert!(Planes::new(&data, crop, stride, rows).is_err(), "{},{},{}x{} stride {stride} rows {rows}", crop.x0, crop.y0, crop.w, crop.h);
        }
    }
}
