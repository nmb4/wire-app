//! Windows Media Foundation hardware H.264 encoder/decoder.

use std::sync::{mpsc, Once};

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL, LUID, RECT, VARIANT_TRUE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_BOOL, VT_I4, VT_UI4};
use wire::video::VideoConfig;

use std::sync::Arc;

use crate::win_mf_d3d::{enumerate_adapters, GpuNv12Frames, GpuVideoProcessor, MfD3d};
use crate::yuv_convert::{bgra_to_nv12, nv12_to_rgba};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BIND_DECODER, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

static MF_INIT: Once = Once::new();

const DECODE_PRESENTATION_TEXTURES: usize = 4;

struct ReturnedVideoTexture {
    generation: u64,
    texture: ID3D11Texture2D,
}

/// A decoder-owned NV12 surface copied out of Media Foundation's sample pool.
///
/// The surface is moved to the UI thread and returned to the decoder when the
/// displayed frame is replaced. This keeps the decoder sample pool free while
/// avoiding a GPU-to-CPU readback and a second GPU upload.
pub struct D3d11VideoFrame {
    texture: Option<ID3D11Texture2D>,
    device: ID3D11Device,
    return_tx: mpsc::Sender<ReturnedVideoTexture>,
    generation: u64,
    coded_width: u32,
    coded_height: u32,
    display_rect: RECT,
}

impl D3d11VideoFrame {
    pub(crate) fn texture(&self) -> &ID3D11Texture2D {
        self.texture
            .as_ref()
            .expect("video frame texture was taken")
    }

    pub(crate) fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub(crate) fn coded_size(&self) -> (u32, u32) {
        (self.coded_width, self.coded_height)
    }

    pub(crate) fn display_rect(&self) -> RECT {
        self.display_rect
    }

    /// Expensive compatibility path used only if native D3D11 presentation
    /// cannot be initialized on this machine.
    pub(crate) fn to_rgba(&self) -> Result<Vec<u8>> {
        let width = self.coded_width;
        let height = self.coded_height;
        let texture = self.texture();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .context("creating decoder fallback staging texture")?;
        }
        let staging = staging.context("decoder fallback staging texture was null")?;
        let context = unsafe { self.device.GetImmediateContext()? };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            context.CopyResource(&staging, texture);
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("mapping decoder fallback staging texture")?;
        }
        let pitch = mapped.RowPitch as usize;
        let width_usize = width as usize;
        let height_usize = height as usize;
        let mut nv12 = vec![0; width_usize * height_usize * 3 / 2];
        unsafe {
            let base = mapped.pData as *const u8;
            for row in 0..height_usize {
                std::ptr::copy_nonoverlapping(
                    base.add(row * pitch),
                    nv12.as_mut_ptr().add(row * width_usize),
                    width_usize,
                );
            }
            let uv_offset = width_usize * height_usize;
            for row in 0..height_usize / 2 {
                std::ptr::copy_nonoverlapping(
                    base.add((height_usize + row) * pitch),
                    nv12.as_mut_ptr().add(uv_offset + row * width_usize),
                    width_usize,
                );
            }
            context.Unmap(&staging, 0);
        }
        let mut rgba = vec![0; width_usize * height_usize * 4];
        nv12_to_rgba(&nv12, width, height, &mut rgba);
        Ok(crop_rgba(&rgba, width, height, self.display_rect))
    }
}

impl Drop for D3d11VideoFrame {
    fn drop(&mut self) {
        if let Some(texture) = self.texture.take() {
            let _ = self.return_tx.send(ReturnedVideoTexture {
                generation: self.generation,
                texture,
            });
        }
    }
}

struct DecoderPresentationPool {
    d3d: Arc<MfD3d>,
    return_tx: mpsc::Sender<ReturnedVideoTexture>,
    return_rx: mpsc::Receiver<ReturnedVideoTexture>,
    available: Vec<ID3D11Texture2D>,
    allocated: usize,
    width: u32,
    height: u32,
    display_rect: RECT,
    generation: u64,
    skipped: u64,
    last_skip_log: std::time::Instant,
}

impl DecoderPresentationPool {
    fn new(d3d: Arc<MfD3d>, width: u32, height: u32, display_rect: RECT, generation: u64) -> Self {
        let (return_tx, return_rx) = mpsc::channel();
        Self {
            d3d,
            return_tx,
            return_rx,
            available: Vec::with_capacity(DECODE_PRESENTATION_TEXTURES),
            allocated: 0,
            width,
            height,
            display_rect,
            generation,
            skipped: 0,
            last_skip_log: std::time::Instant::now(),
        }
    }

    fn reclaim(&mut self) {
        while let Ok(returned) = self.return_rx.try_recv() {
            if returned.generation == self.generation {
                self.available.push(returned.texture);
            }
        }
    }

    fn allocate_texture(&mut self) -> Result<Option<ID3D11Texture2D>> {
        self.reclaim();
        if let Some(texture) = self.available.pop() {
            return Ok(Some(texture));
        }
        if self.allocated >= DECODE_PRESENTATION_TEXTURES {
            self.skipped += 1;
            if self.last_skip_log.elapsed() >= std::time::Duration::from_secs(5) {
                warn!(
                    "native decoder presentation pool skipped {} display frame(s) while the UI held all {} surfaces",
                    self.skipped, DECODE_PRESENTATION_TEXTURES
                );
                self.skipped = 0;
                self.last_skip_log = std::time::Instant::now();
            }
            return Ok(None);
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_DECODER.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe {
            self.d3d
                .device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .context("creating decoder presentation texture")?;
        }
        self.allocated += 1;
        Ok(texture)
    }

    fn copy_sample(&mut self, sample: &IMFSample) -> Result<Option<D3d11VideoFrame>> {
        let Some(destination) = self.allocate_texture()? else {
            return Ok(None);
        };
        let buffer: IMFMediaBuffer = unsafe { sample.GetBufferByIndex(0)? };
        let dxgi: IMFDXGIBuffer = buffer
            .cast()
            .context("decoder output buffer is not a DXGI surface")?;
        let mut resource: Option<windows::core::IUnknown> = None;
        unsafe {
            dxgi.GetResource(
                &ID3D11Texture2D::IID,
                &mut resource as *mut _ as *mut *mut _,
            )?;
        }
        let source: ID3D11Texture2D = resource
            .context("decoder output DXGI resource was null")?
            .cast()?;
        let source_subresource = unsafe { dxgi.GetSubresourceIndex()? };
        unsafe {
            self.d3d.context.CopySubresourceRegion(
                &destination,
                0,
                0,
                0,
                0,
                &source,
                source_subresource,
                None,
            );
            self.d3d.context.Flush();
        }
        Ok(Some(D3d11VideoFrame {
            texture: Some(destination),
            device: self.d3d.device.clone(),
            return_tx: self.return_tx.clone(),
            generation: self.generation,
            coded_width: self.width,
            coded_height: self.height,
            display_rect: self.display_rect,
        }))
    }
}

fn rect_size(rect: RECT) -> (u32, u32) {
    (
        rect.right.saturating_sub(rect.left).max(0) as u32,
        rect.bottom.saturating_sub(rect.top).max(0) as u32,
    )
}

fn crop_rgba(src: &[u8], coded_width: u32, coded_height: u32, rect: RECT) -> Vec<u8> {
    let (display_width, display_height) = rect_size(rect);
    let valid = rect.left >= 0
        && rect.top >= 0
        && rect.right <= coded_width as i32
        && rect.bottom <= coded_height as i32
        && display_width > 0
        && display_height > 0;
    if !valid {
        return src.to_vec();
    }
    if rect.left == 0
        && rect.top == 0
        && display_width == coded_width
        && display_height == coded_height
    {
        return src.to_vec();
    }
    let source_stride = coded_width as usize * 4;
    let row_bytes = display_width as usize * 4;
    let mut out = vec![0; row_bytes * display_height as usize];
    for row in 0..display_height as usize {
        let source_offset = (rect.top as usize + row) * source_stride + rect.left as usize * 4;
        let destination_offset = row * row_bytes;
        out[destination_offset..destination_offset + row_bytes]
            .copy_from_slice(&src[source_offset..source_offset + row_bytes]);
    }
    out
}

fn ensure_media_foundation() -> Result<()> {
    let mut err = Ok(());
    MF_INIT.call_once(|| {
        err = unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .context("CoInitializeEx failed")
                .and_then(|_| MFStartup(MF_VERSION, MFSTARTUP_LITE).context("MFStartup failed"))
        };
    });
    err
}

fn pack_ratio(numerator: u32, denominator: u32) -> u64 {
    ((numerator as u64) << 32) | denominator as u64
}

fn pack_size(width: u32, height: u32) -> u64 {
    ((width as u64) << 32) | height as u64
}

fn create_video_media_type(
    subtype: windows::core::GUID,
    width: u32,
    height: u32,
    framerate: u32,
    bitrate: Option<u32>,
    compressed: bool,
) -> Result<IMFMediaType> {
    let media_type = unsafe { MFCreateMediaType()? };
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        media_type.SetUINT32(&MF_MT_COMPRESSED, compressed as u32)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(framerate, 1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        if let Some(bitrate) = bitrate {
            media_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        }
    }
    Ok(media_type)
}

fn enumerate_transforms(
    category: windows::core::GUID,
    input: windows::core::GUID,
    output: windows::core::GUID,
    flags: MFT_ENUM_FLAG,
) -> Result<Vec<IMFTransform>> {
    let input_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: input,
    };
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: output,
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            category,
            flags,
            Some(&input_info),
            Some(&output_info),
            &mut activates,
            &mut count,
        )?;
    }

    if count == 0 {
        return Err(anyhow!("no Media Foundation transform found"));
    }

    let mut transforms = Vec::with_capacity(count as usize);
    for i in 0..count {
        let transform = unsafe {
            let activate = (*activates.add(i as usize))
                .as_ref()
                .ok_or_else(|| anyhow!("null MFT activate"))?;
            info!(
                "found MFT candidate #{}: {}",
                i + 1,
                read_mft_name(activate)
            );
            activate.ActivateObject::<IMFTransform>()
        }?;
        transforms.push(transform);
    }
    unsafe {
        CoTaskMemFree(Some(activates as *const _ as *mut _));
    }
    Ok(transforms)
}

fn find_transform(
    category: windows::core::GUID,
    input: windows::core::GUID,
    output: windows::core::GUID,
    flags: MFT_ENUM_FLAG,
) -> Result<IMFTransform> {
    enumerate_transforms(category, input, output, flags)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no Media Foundation transform found"))
}

struct MftCandidate {
    activate: IMFActivate,
    adapter_luid: Option<LUID>,
    name: String,
}

fn activate_transform(activate: &IMFActivate) -> Result<IMFTransform> {
    unsafe {
        activate
            .ActivateObject::<IMFTransform>()
            .map_err(Into::into)
    }
}

fn read_adapter_luid(activate: &IMFActivate) -> Option<LUID> {
    unsafe {
        let size = activate.GetBlobSize(&MFT_ENUM_ADAPTER_LUID).ok()?;
        if size as usize != std::mem::size_of::<LUID>() {
            return None;
        }
        let mut bytes = vec![0u8; size as usize];
        activate
            .GetBlob(&MFT_ENUM_ADAPTER_LUID, &mut bytes, None)
            .ok()?;
        Some(std::ptr::read(bytes.as_ptr().cast::<LUID>()))
    }
}

fn read_mft_name(activate: &IMFActivate) -> String {
    unsafe {
        let mut len = activate
            .GetStringLength(&MFT_FRIENDLY_NAME_Attribute)
            .unwrap_or(0);
        if len == 0 {
            return "unknown MFT".to_string();
        }
        let mut wide = vec![0u16; (len + 1) as usize];
        if activate
            .GetString(&MFT_FRIENDLY_NAME_Attribute, &mut wide, Some(&mut len))
            .is_err()
        {
            return "unknown MFT".to_string();
        }
        String::from_utf16_lossy(&wide[..len as usize])
    }
}

fn enumerate_video_encoders(prefer_hardware: bool) -> Result<Vec<MftCandidate>> {
    let base = MFT_ENUM_FLAG_SORTANDFILTER;
    let flags = if prefer_hardware {
        MFT_ENUM_FLAG(base.0 | MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_ASYNCMFT.0)
    } else {
        MFT_ENUM_FLAG(base.0 | MFT_ENUM_FLAG_SYNCMFT.0)
    };

    let input_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input_info),
            Some(&output_info),
            &mut activates,
            &mut count,
        )?;
    }

    if count == 0 {
        return Err(anyhow!("no Media Foundation transform found"));
    }

    let mut candidates = Vec::with_capacity(count as usize);
    for i in 0..count {
        let candidate = unsafe {
            let activate = (*activates.add(i as usize))
                .as_ref()
                .ok_or_else(|| anyhow!("null MFT activate"))?;
            let name = read_mft_name(activate);
            let adapter_luid = read_adapter_luid(activate);
            info!(
                "found MFT candidate #{}: {name}{}",
                i + 1,
                adapter_luid
                    .map(|l| format!(" (adapter {:x}:{:x})", l.HighPart, l.LowPart))
                    .unwrap_or_default()
            );
            MftCandidate {
                activate: activate.clone(),
                adapter_luid,
                name,
            }
        };
        candidates.push(candidate);
    }
    unsafe {
        CoTaskMemFree(Some(activates as *const _ as *mut _));
    }
    Ok(candidates)
}

fn is_async_transform(transform: &IMFTransform) -> Result<bool> {
    let attrs = unsafe { transform.GetAttributes()? };
    let async_flag = unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0);
    Ok(async_flag != 0)
}

fn unlock_async_transform(transform: &IMFTransform) -> Result<()> {
    let attrs = unsafe { transform.GetAttributes()? };
    unsafe {
        attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
    }
    Ok(())
}

fn wait_for_transform_event(
    transform: &IMFTransform,
    output_stream_id: u32,
    expected: MF_EVENT_TYPE,
) -> Result<()> {
    loop {
        let event = read_transform_event(transform, MF_EVENT_FLAG_NONE)?
            .context("MFT event stream ended")?;
        if matches_expected_event(&event, expected) {
            return Ok(());
        }
        if matches!(event, TransformEvent::HaveOutput) {
            let _ = process_output_once(transform, output_stream_id)?;
        }
    }
}

const MIN_H264_OUTPUT_BUFFER: u32 = 4 * 1024 * 1024;

enum TransformEvent {
    NeedInput,
    HaveOutput,
    Other(u32),
}

fn matches_expected_event(event: &TransformEvent, expected: MF_EVENT_TYPE) -> bool {
    match event {
        TransformEvent::NeedInput => expected == METransformNeedInput,
        TransformEvent::HaveOutput => expected == METransformHaveOutput,
        TransformEvent::Other(event_type) => *event_type == expected.0 as u32,
    }
}

fn read_transform_event(
    transform: &IMFTransform,
    flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
) -> Result<Option<TransformEvent>> {
    let events: IMFMediaEventGenerator = transform.cast()?;
    let event = match unsafe { events.GetEvent(flags) } {
        Ok(event) => event,
        Err(e) if flags == MF_EVENT_FLAG_NO_WAIT && e.code() == MF_E_NO_EVENTS_AVAILABLE => {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    let event_type = unsafe { event.GetType()? };
    let status = unsafe { event.GetStatus()? };
    if !status.is_ok() {
        return Err(anyhow!(
            "MFT event {} failed with status {status}",
            event_type
        ));
    }
    Ok(Some(if event_type == METransformNeedInput.0 as u32 {
        TransformEvent::NeedInput
    } else if event_type == METransformHaveOutput.0 as u32 {
        TransformEvent::HaveOutput
    } else {
        TransformEvent::Other(event_type)
    }))
}

fn get_stream_ids(transform: &IMFTransform) -> Result<(u32, u32)> {
    let mut input_ids = [0u32; 1];
    let mut output_ids = [0u32; 1];
    match unsafe { transform.GetStreamIDs(&mut input_ids, &mut output_ids) } {
        Ok(()) => Ok((input_ids[0], output_ids[0])),
        Err(e) if e.code() == E_NOTIMPL => Ok((0, 0)),
        Err(e) => Err(e.into()),
    }
}

fn is_no_more_types(err: &windows::core::Error) -> bool {
    err.code() == MF_E_NO_MORE_TYPES
}

fn negotiate_output_type(
    transform: &IMFTransform,
    stream_id: u32,
    width: u32,
    height: u32,
    framerate: u32,
    bitrate: u32,
) -> Result<()> {
    let mut index = 0u32;
    loop {
        let output_type = match unsafe { transform.GetOutputAvailableType(stream_id, index) } {
            Ok(t) => t,
            Err(e) if is_no_more_types(&e) => {
                break;
            }
            Err(e) => return Err(e.into()),
        };
        index += 1;

        let subtype = unsafe { output_type.GetGUID(&MF_MT_SUBTYPE)? };
        if subtype != MFVideoFormat_H264 {
            continue;
        }

        unsafe {
            output_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))?;
            output_type.SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(framerate, 1))?;
            output_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))?;
            output_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            output_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        }

        if unsafe { transform.SetOutputType(stream_id, &output_type, 0) }.is_ok() {
            return Ok(());
        }
    }

    let output_type = create_video_media_type(
        MFVideoFormat_H264,
        width,
        height,
        framerate,
        Some(bitrate),
        true,
    )?;
    unsafe {
        transform.SetOutputType(stream_id, &output_type, 0)?;
    }
    Ok(())
}

fn negotiate_input_type(
    transform: &IMFTransform,
    stream_id: u32,
    width: u32,
    height: u32,
    framerate: u32,
) -> Result<()> {
    let mut index = 0u32;
    loop {
        let input_type = match unsafe { transform.GetInputAvailableType(stream_id, index) } {
            Ok(t) => t,
            Err(e) if is_no_more_types(&e) => {
                break;
            }
            Err(e) => return Err(e.into()),
        };
        index += 1;

        let subtype = unsafe { input_type.GetGUID(&MF_MT_SUBTYPE)? };
        if subtype != MFVideoFormat_NV12 {
            continue;
        }

        unsafe {
            input_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))?;
            input_type.SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(framerate, 1))?;
            input_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))?;
            input_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            let _ = input_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, width);
            let _ = input_type.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1);
            let _ = input_type.SetUINT32(&MF_MT_SAMPLE_SIZE, nv12_buffer_size(width, height));
            let _ = input_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_0_255.0 as u32);
            let _ = input_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
        }

        if unsafe { transform.SetInputType(stream_id, &input_type, 0) }.is_ok() {
            return Ok(());
        }
    }

    let input_type =
        create_video_media_type(MFVideoFormat_NV12, width, height, framerate, None, false)?;
    unsafe {
        input_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, width)?;
        input_type.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
        input_type.SetUINT32(&MF_MT_SAMPLE_SIZE, nv12_buffer_size(width, height))?;
        transform.SetInputType(stream_id, &input_type, 0)?;
    }
    Ok(())
}

fn configure_encoder(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    framerate: u32,
    bitrate: u32,
    async_mode: bool,
    d3d: Option<&MfD3d>,
) -> Result<(u32, u32)> {
    // Async hardware MFTs must be unlocked before any other call (including GetStreamIDs).
    if async_mode {
        unlock_async_transform(transform)?;
    }
    if let Some(d3d) = d3d {
        d3d.attach_to_transform(transform)?;
    }
    let (input_stream_id, output_stream_id) = get_stream_ids(transform)?;

    if let Ok(attrs) = unsafe { transform.GetAttributes() } {
        unsafe {
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }
    }

    configure_codec_api(transform, framerate, bitrate);
    negotiate_output_type(
        transform,
        output_stream_id,
        width,
        height,
        framerate,
        bitrate,
    )?;
    negotiate_input_type(transform, input_stream_id, width, height, framerate)?;
    enable_low_latency(transform);
    send_transform_message(transform, MFT_MESSAGE_COMMAND_FLUSH)?;
    send_transform_message(transform, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING)?;
    send_transform_message(transform, MFT_MESSAGE_NOTIFY_START_OF_STREAM)?;
    if async_mode {
        wait_for_transform_event(transform, output_stream_id, METransformNeedInput)?;
    }
    Ok((input_stream_id, output_stream_id))
}

fn send_transform_message(transform: &IMFTransform, message: MFT_MESSAGE_TYPE) -> Result<()> {
    unsafe {
        if let Err(e) = transform.ProcessMessage(message, 0) {
            // Some sync encoders reject flush before the first streaming session.
            if message == MFT_MESSAGE_COMMAND_FLUSH && e.code() == E_FAIL {
                return Ok(());
            }
            return Err(anyhow!("MFT message {:?} failed: {e}", message.0));
        }
    }
    Ok(())
}

fn set_codecapi_bool(api: &ICodecAPI, key: &GUID, value: bool) {
    unsafe {
        let mut variant = VARIANT::default();
        let inner = &mut *variant.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = if value {
            VARIANT_TRUE
        } else {
            windows::Win32::Foundation::VARIANT_BOOL(0)
        };
        let _ = api.SetValue(key, &variant);
        let _ = VariantClear(&mut variant);
    }
}

fn set_codecapi_i32(api: &ICodecAPI, key: &GUID, value: i32) {
    unsafe {
        let mut variant = VARIANT::default();
        let inner = &mut *variant.Anonymous.Anonymous;
        inner.vt = VT_I4;
        inner.Anonymous.lVal = value;
        let _ = api.SetValue(key, &variant);
        let _ = VariantClear(&mut variant);
    }
}

fn set_codecapi_u32(api: &ICodecAPI, key: &GUID, value: u32) {
    unsafe {
        let mut variant = VARIANT::default();
        let inner = &mut *variant.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
        let _ = api.SetValue(key, &variant);
        let _ = VariantClear(&mut variant);
    }
}

fn configure_codec_api(transform: &IMFTransform, framerate: u32, bitrate: u32) {
    if let Ok(api) = transform.cast::<ICodecAPI>() {
        set_codecapi_i32(
            &api,
            &CODECAPI_AVEncCommonRateControlMode,
            eAVEncCommonRateControlMode_CBR.0,
        );
        set_codecapi_u32(&api, &CODECAPI_AVEncCommonMeanBitRate, bitrate);
        set_codecapi_u32(&api, &CODECAPI_AVEncCommonMaxBitRate, bitrate);
        set_codecapi_bool(&api, &CODECAPI_AVEncCommonRealTime, true);
        set_codecapi_bool(&api, &CODECAPI_AVEncCommonLowLatency, true);
        set_codecapi_bool(&api, &CODECAPI_AVLowLatencyMode, true);
        set_codecapi_u32(&api, &CODECAPI_AVEncMPVDefaultBPictureCount, 0);
        set_codecapi_u32(
            &api,
            &CODECAPI_AVEncVideoMaxKeyframeDistance,
            framerate.saturating_mul(2).max(1),
        );
    }
}

fn enable_low_latency(transform: &IMFTransform) {
    if let Ok(api) = transform.cast::<ICodecAPI>() {
        set_codecapi_bool(&api, &CODECAPI_AVEncCommonLowLatency, true);
    }
}

fn nv12_buffer_size(width: u32, height: u32) -> u32 {
    width * (height + height / 2)
}

fn read_nv12_stride(transform: &IMFTransform, stream_id: u32, width: u32) -> u32 {
    unsafe {
        if let Ok(input_type) = transform.GetInputCurrentType(stream_id) {
            if let Ok(stride) = input_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) {
                return stride.max(width);
            }
        }
    }
    width
}

fn pack_nv12_strided(src: &[u8], width: u32, height: u32, stride: u32) -> Vec<u8> {
    if stride == width {
        return src.to_vec();
    }
    let w = width as usize;
    let h = height as usize;
    let stride = stride as usize;
    let mut out = vec![0u8; stride * h + stride * (h / 2)];
    for row in 0..h {
        out[row * stride..row * stride + w].copy_from_slice(&src[row * w..(row + 1) * w]);
    }
    let uv_src = w * h;
    let uv_dst = stride * h;
    for row in 0..h / 2 {
        out[uv_dst + row * stride..uv_dst + row * stride + w]
            .copy_from_slice(&src[uv_src + row * w..uv_src + (row + 1) * w]);
    }
    out
}

fn create_media_sample(data: &[u8], timestamp: i64, duration: i64) -> Result<IMFSample> {
    let buffer = unsafe { MFCreateMemoryBuffer(data.len() as u32)? };
    unsafe {
        let mut ptr = std::ptr::null_mut();
        let mut max_len = 0u32;
        let mut cur_len = 0u32;
        buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(data.len() as u32)?;
    }

    let sample = unsafe { MFCreateSample()? };
    unsafe {
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(timestamp)?;
        sample.SetSampleDuration(duration)?;
    }
    Ok(sample)
}

fn create_h264_sample(data: &[u8], timestamp: i64, duration: i64) -> Result<IMFSample> {
    let buffer = unsafe { MFCreateMemoryBuffer(data.len() as u32)? };
    unsafe {
        let mut ptr = std::ptr::null_mut();
        let mut max_len = 0u32;
        let mut cur_len = 0u32;
        buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(data.len() as u32)?;
    }
    let sample = unsafe { MFCreateSample()? };
    unsafe {
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(timestamp)?;
        sample.SetSampleDuration(duration)?;
    }
    Ok(sample)
}

fn create_output_sample(
    transform: &IMFTransform,
    output_stream_id: u32,
) -> Result<Option<IMFSample>> {
    match unsafe { transform.GetOutputStreamInfo(output_stream_id) } {
        Ok(info)
            if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0
                && info.dwFlags & MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32 == 0 =>
        {
            let sample = unsafe { MFCreateSample()? };
            let buffer = unsafe { MFCreateMemoryBuffer(info.cbSize.max(MIN_H264_OUTPUT_BUFFER))? };
            unsafe {
                sample.AddBuffer(&buffer)?;
            }
            Ok(Some(sample))
        }
        Ok(_) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn process_output_once(transform: &IMFTransform, output_stream_id: u32) -> Result<Option<Vec<u8>>> {
    let output_sample = create_output_sample(transform, output_stream_id)?;
    let mut buffer = MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: output_stream_id,
        pSample: std::mem::ManuallyDrop::new(output_sample),
        dwStatus: 0,
        pEvents: std::mem::ManuallyDrop::new(None),
    };
    let mut status = 0u32;
    let result =
        unsafe { transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
    match result {
        Ok(()) => {
            let sample = buffer
                .pSample
                .take()
                .ok_or_else(|| anyhow!("MFT output missing sample"))?;
            Ok(Some(read_sample_bytes(&sample)?))
        }
        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn drain_output(transform: &IMFTransform, output_stream_id: u32) -> Result<Vec<Vec<u8>>> {
    let mut packets = Vec::new();
    while let Some(packet) = process_output_once(transform, output_stream_id)? {
        packets.push(packet);
    }
    Ok(packets)
}

fn read_sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    let buffer: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer()? };
    let mut ptr = std::ptr::null_mut();
    let mut max_len = 0u32;
    let mut cur_len = 0u32;
    unsafe {
        buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))?;
        let data = std::slice::from_raw_parts(ptr, cur_len as usize).to_vec();
        buffer.Unlock()?;
        Ok(data)
    }
}

fn process_output_once_sample(
    transform: &IMFTransform,
    output_stream_id: u32,
) -> Result<Option<IMFSample>> {
    let output_sample = create_output_sample(transform, output_stream_id)?;
    let mut buffer = MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: output_stream_id,
        pSample: std::mem::ManuallyDrop::new(output_sample),
        dwStatus: 0,
        pEvents: std::mem::ManuallyDrop::new(None),
    };
    let mut status = 0u32;
    match unsafe { transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) } {
        Ok(()) => {
            let sample = buffer
                .pSample
                .take()
                .ok_or_else(|| anyhow!("MFT output missing sample"))?;
            Ok(Some(sample))
        }
        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
        Err(e) => Err(e.into()),
    }
}

enum DecoderOutput {
    Sample(IMFSample),
    NeedMoreInput,
    StreamChange,
}

fn process_decoder_output_once(
    transform: &IMFTransform,
    output_stream_id: u32,
) -> Result<DecoderOutput> {
    let output_sample = create_output_sample(transform, output_stream_id)?;
    let mut buffer = MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: output_stream_id,
        pSample: std::mem::ManuallyDrop::new(output_sample),
        dwStatus: 0,
        pEvents: std::mem::ManuallyDrop::new(None),
    };
    let mut status = 0u32;
    match unsafe { transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) } {
        Ok(()) => Ok(DecoderOutput::Sample(
            buffer
                .pSample
                .take()
                .ok_or_else(|| anyhow!("decoder output missing sample"))?,
        )),
        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(DecoderOutput::NeedMoreInput),
        Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => Ok(DecoderOutput::StreamChange),
        Err(e) => Err(e.into()),
    }
}

/// Read an image sample, normalizing for bottom-up (negative stride) buffers so
/// the result is always top-down. MF video buffers are frequently bottom-up,
/// which otherwise renders upside down. `is_nv12` selects the NV12 (3/2) versus
/// RGB32 (4 bpp) plane layout. Non-2D (compressed) buffers fall back to a raw copy.
fn read_sample_topdown(
    sample: &IMFSample,
    width: u32,
    height: u32,
    is_nv12: bool,
) -> Result<Vec<u8>> {
    let buffer: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer()? };
    let w = width as usize;
    let h = height as usize;
    let out_len = if is_nv12 { w * h * 3 / 2 } else { w * h * 4 };
    let mut out = vec![0u8; out_len];

    if let Ok(buf2d) = unsafe { buffer.cast::<IMF2DBuffer>() } {
        let mut ptr = std::ptr::null_mut();
        let mut stride: i32 = 0;
        unsafe {
            buf2d.Lock2D(&mut ptr, &mut stride)?;
        }
        let bottom_up = stride < 0;
        let stride = stride.unsigned_abs() as usize;
        let src = unsafe {
            std::slice::from_raw_parts(ptr as *const u8, ((h + h / 2) * stride) as usize)
        };
        if is_nv12 {
            for y in 0..h {
                let src_row = if bottom_up {
                    (h - 1 - y) * stride
                } else {
                    y * stride
                };
                out[y * w..(y + 1) * w].copy_from_slice(&src[src_row..src_row + w]);
            }
            let uv_rows = h / 2;
            for cy in 0..uv_rows {
                let src_row = if bottom_up {
                    (h + uv_rows - 1 - cy) * stride
                } else {
                    (h + cy) * stride
                };
                let dst = w * h + cy * w;
                out[dst..dst + w].copy_from_slice(&src[src_row..src_row + w]);
            }
        } else {
            for y in 0..h {
                let src_row = if bottom_up {
                    (h - 1 - y) * stride
                } else {
                    y * stride
                };
                let dst = y * w * 4;
                out[dst..dst + w * 4].copy_from_slice(&src[src_row..src_row + w * 4]);
            }
        }
        unsafe {
            buf2d.Unlock2D()?;
        }
        Ok(out)
    } else {
        let mut p = std::ptr::null_mut();
        let mut max_len = 0u32;
        let mut cur_len = 0u32;
        unsafe {
            buffer.Lock(&mut p, Some(&mut max_len), Some(&mut cur_len))?;
            let data = std::slice::from_raw_parts(p as *const u8, cur_len as usize).to_vec();
            buffer.Unlock()?;
            Ok(data)
        }
    }
}

fn configure_processor(
    transform: &IMFTransform,
    input: windows::core::GUID,
    output: windows::core::GUID,
    width: u32,
    height: u32,
) -> Result<()> {
    let output_type = create_video_media_type(output, width, height, 30, None, false)?;
    let input_type = create_video_media_type(input, width, height, 30, None, false)?;
    unsafe {
        transform.SetOutputType(0, &output_type, 0)?;
        transform.SetInputType(0, &input_type, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
    }
    Ok(())
}

fn force_keyframe(transform: &IMFTransform) {
    unsafe {
        if let Ok(api) = transform.cast::<ICodecAPI>() {
            let mut variant = VARIANT::default();
            let inner = &mut *variant.Anonymous.Anonymous;
            inner.vt = VT_BOOL;
            inner.Anonymous.boolVal = VARIANT_TRUE;
            let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant);
            let _ = VariantClear(&mut variant);
        }
    }
}

struct MfColorConverter {
    transform: IMFTransform,
    width: u32,
    height: u32,
    output_is_nv12: bool,
    scratch: Vec<u8>,
}

impl MfColorConverter {
    fn try_new(
        input: windows::core::GUID,
        output: windows::core::GUID,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_SORTANDFILTER.0 | MFT_ENUM_FLAG_SYNCMFT.0);
        let transform = find_transform(MFT_CATEGORY_VIDEO_PROCESSOR, input, output, flags)?;
        configure_processor(&transform, input, output, width, height)?;
        let output_is_nv12 = output == MFVideoFormat_NV12;
        let scratch_len = if output_is_nv12 {
            (width * height * 3 / 2) as usize
        } else {
            (width * height * 4) as usize
        };
        Ok(Self {
            transform,
            width,
            height,
            output_is_nv12,
            scratch: vec![0u8; scratch_len],
        })
    }

    fn convert(&mut self, input: &[u8], timestamp: i64, duration: i64) -> Result<Option<&[u8]>> {
        let sample = create_media_sample(input, timestamp, duration)?;
        let mut queued_output = None;
        match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            Ok(()) => {}
            Err(error) if error.code() == MF_E_NOTACCEPTING => {
                // A synchronous converter may retain one frame. Drain that frame,
                // then queue the current input instead of surfacing NOT_ACCEPTING
                // as a decoder failure.
                queued_output = process_output_once_sample(&self.transform, 0)?;
                unsafe {
                    self.transform.ProcessInput(0, &sample, 0)?;
                }
            }
            Err(error) => return Err(error.into()),
        }
        let out_sample = match queued_output {
            Some(sample) => Some(sample),
            None => process_output_once_sample(&self.transform, 0)?,
        };
        let Some(out_sample) = out_sample else {
            return Ok(None);
        };
        // Normalize orientation: MF image buffers are often bottom-up, so read
        // the output top-down regardless of which direction we are converting.
        let out = read_sample_topdown(&out_sample, self.width, self.height, self.output_is_nv12)?;
        if out.len() > self.scratch.len() {
            self.scratch.resize(out.len(), 0);
        }
        self.scratch[..out.len()].copy_from_slice(&out);
        Ok(Some(&self.scratch[..out.len()]))
    }
}

pub struct MfH264Encoder {
    transform: IMFTransform,
    input_stream_id: u32,
    output_stream_id: u32,
    width: u32,
    height: u32,
    nv12_stride: u32,
    nv12: Vec<u8>,
    bgra_to_nv12: Option<MfColorConverter>,
    frame_index: i64,
    frame_duration: i64,
    hardware: bool,
    async_mode: bool,
    pending_input: u32,
    d3d: Option<Arc<MfD3d>>,
    gpu_nv12: Option<GpuNv12Frames>,
    // The MF allocator's textures are encoder inputs and are not guaranteed to have
    // D3D11_BIND_RENDER_TARGET. Render into our own NV12 target, then copy into the
    // allocator surface at the frame boundary.
    gpu_processor: Option<(u32, u32, GpuVideoProcessor, ID3D11Texture2D)>,
}

fn read_sample_topdown_into(
    sample: &IMFSample,
    width: u32,
    height: u32,
    is_nv12: bool,
    out: &mut Vec<u8>,
) -> Result<()> {
    let buffer: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer()? };
    let w = width as usize;
    let h = height as usize;
    let out_len = if is_nv12 { w * h * 3 / 2 } else { w * h * 4 };
    out.resize(out_len, 0);

    if let Ok(buf2d) = buffer.cast::<IMF2DBuffer>() {
        let mut ptr = std::ptr::null_mut();
        let mut stride: i32 = 0;
        unsafe { buf2d.Lock2D(&mut ptr, &mut stride)? };
        let bottom_up = stride < 0;
        let stride = stride.unsigned_abs() as usize;
        let src = unsafe { std::slice::from_raw_parts(ptr as *const u8, (h + h / 2) * stride) };
        if is_nv12 {
            for y in 0..h {
                let src_row = if bottom_up {
                    (h - 1 - y) * stride
                } else {
                    y * stride
                };
                out[y * w..(y + 1) * w].copy_from_slice(&src[src_row..src_row + w]);
            }
            for y in 0..h / 2 {
                let src_row = if bottom_up {
                    (h + h / 2 - 1 - y) * stride
                } else {
                    (h + y) * stride
                };
                let dst = w * h + y * w;
                out[dst..dst + w].copy_from_slice(&src[src_row..src_row + w]);
            }
        } else {
            for y in 0..h {
                let src_row = if bottom_up {
                    (h - 1 - y) * stride
                } else {
                    y * stride
                };
                let dst = y * w * 4;
                out[dst..dst + w * 4].copy_from_slice(&src[src_row..src_row + w * 4]);
            }
        }
        unsafe { buf2d.Unlock2D()? };
        Ok(())
    } else {
        let mut ptr = std::ptr::null_mut();
        let mut max_len = 0u32;
        let mut cur_len = 0u32;
        unsafe {
            buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))?;
            let len = (cur_len as usize).min(out.len());
            out[..len].copy_from_slice(std::slice::from_raw_parts(ptr as *const u8, len));
            buffer.Unlock()?;
        }
        Ok(())
    }
}

impl MfH264Encoder {
    pub fn try_new_on_device(config: &VideoConfig, device: &ID3D11Device) -> Result<Self> {
        ensure_media_foundation()?;
        let width = config.resolution.width();
        let height = config.resolution.height();
        let bitrate = config.effective_bitrate();
        let d3d = Arc::new(MfD3d::from_device(device)?);
        let mut last_err = None;
        for candidate in enumerate_video_encoders(true)? {
            if candidate.name.to_ascii_lowercase().contains("dx12") {
                continue;
            }
            let transform = match activate_transform(&candidate.activate) {
                Ok(transform) => transform,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            match Self::build_with_d3d(
                transform,
                Some(d3d.clone()),
                config,
                width,
                height,
                bitrate,
                true,
            ) {
                Ok(encoder) => {
                    info!(
                        "MF encoder '{}' is sharing the WGC D3D11 device",
                        candidate.name
                    );
                    return Ok(encoder);
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("no encoder accepted the WGC D3D11 device")))
    }

    pub fn try_new(config: &VideoConfig) -> Result<Self> {
        ensure_media_foundation()?;
        let width = config.resolution.width();
        let height = config.resolution.height();
        let bitrate = config.effective_bitrate();
        let mut last_err = None;

        for (prefer_hardware, label) in [(true, "hardware"), (false, "software")] {
            let candidates = match enumerate_video_encoders(prefer_hardware) {
                Ok(c) => c,
                Err(e) => {
                    warn!("MF {label} H.264 encoder enumeration failed: {e:?}");
                    last_err = Some(e);
                    continue;
                }
            };
            for candidate in candidates {
                if prefer_hardware && candidate.name.to_ascii_lowercase().contains("dx12") {
                    info!("skipping DX12 encoder '{}'", candidate.name);
                    continue;
                }
                if !prefer_hardware {
                    let transform = activate_transform(&candidate.activate)?;
                    if is_async_transform(&transform).unwrap_or(false) {
                        info!(
                            "skipping async MFT '{}' on software encode path",
                            candidate.name
                        );
                        continue;
                    }
                }
                if prefer_hardware {
                    let mut candidate_err = None;
                    let mut d3d_targets: Vec<Result<Arc<MfD3d>, anyhow::Error>> = Vec::new();
                    if let Some(luid) = candidate.adapter_luid {
                        d3d_targets.push(MfD3d::try_new(Some(luid)).map(Arc::new));
                    } else if let Ok(adapters) = enumerate_adapters() {
                        for adapter in adapters {
                            d3d_targets.push(MfD3d::try_new_with_adapter(&adapter).map(Arc::new));
                        }
                    } else {
                        d3d_targets.push(MfD3d::try_new(None).map(Arc::new));
                    }
                    for d3d in d3d_targets {
                        match d3d {
                            Ok(d3d) => {
                                let transform = match activate_transform(&candidate.activate) {
                                    Ok(t) => t,
                                    Err(e) => {
                                        candidate_err = Some(e);
                                        continue;
                                    }
                                };
                                match Self::build_with_d3d(
                                    transform,
                                    Some(d3d),
                                    config,
                                    width,
                                    height,
                                    bitrate,
                                    true,
                                ) {
                                    Ok(enc) => return Ok(enc),
                                    Err(e) => {
                                        warn!(
                                            "MF hardware H.264 encoder '{}' failed on one adapter: {e:#}",
                                            candidate.name
                                        );
                                        candidate_err = Some(e);
                                    }
                                }
                            }
                            Err(e) => candidate_err = Some(e),
                        }
                    }
                    match activate_transform(&candidate.activate).and_then(|transform| {
                        Self::build_with_d3d(transform, None, config, width, height, bitrate, true)
                    }) {
                        Ok(enc) => return Ok(enc),
                        Err(e) => {
                            warn!(
                                "MF hardware H.264 encoder '{}' failed with system-memory samples: {e:#}",
                                candidate.name
                            );
                            candidate_err = Some(e);
                        }
                    }
                    if let Some(e) = candidate_err {
                        warn!("MF {label} H.264 encoder candidate failed: {e:#}");
                        last_err = Some(e);
                    }
                } else {
                    let transform = activate_transform(&candidate.activate)?;
                    match Self::build_with_d3d(
                        transform, None, config, width, height, bitrate, false,
                    ) {
                        Ok(enc) => return Ok(enc),
                        Err(e) => {
                            warn!("MF {label} H.264 encoder candidate failed: {e:#}");
                            last_err = Some(e);
                        }
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("no Media Foundation encoder found")))
    }

    fn build_with_d3d(
        transform: IMFTransform,
        d3d: Option<Arc<MfD3d>>,
        config: &VideoConfig,
        width: u32,
        height: u32,
        bitrate: u32,
        hardware: bool,
    ) -> Result<Self> {
        let async_mode = is_async_transform(&transform)?;

        let (input_stream_id, output_stream_id) = configure_encoder(
            &transform,
            width,
            height,
            config.framerate,
            bitrate,
            async_mode,
            d3d.as_deref(),
        )?;
        // Software encoders use MF color converter when available; GPU uses D3D surfaces.
        let bgra_to_nv12 = if hardware {
            None
        } else {
            match MfColorConverter::try_new(MFVideoFormat_RGB32, MFVideoFormat_NV12, width, height)
            {
                Ok(conv) => {
                    info!("MF color converter ready (BGRA -> NV12)");
                    Some(conv)
                }
                Err(e) => {
                    info!("MF color converter unavailable, using CPU YUV: {e:?}");
                    None
                }
            }
        };

        let gpu_nv12 = if let Some(d3d) = d3d.as_ref() {
            let input_type = unsafe { transform.GetInputCurrentType(input_stream_id)? };
            Some(GpuNv12Frames::new(d3d, &input_type, width, height)?)
        } else {
            None
        };

        let frame_duration = 10_000_000i64 / config.framerate.max(1) as i64;
        let nv12_stride = if hardware {
            width
        } else {
            read_nv12_stride(&transform, input_stream_id, width)
        };
        let nv12_len = (nv12_stride * (height + height / 2)) as usize;
        info!(
            "MF encoder negotiated NV12 stride {nv12_stride} ({}x{height}, hw={hardware}, async={async_mode})",
            width
        );

        let mut encoder = Self {
            transform,
            input_stream_id,
            output_stream_id,
            width,
            height,
            nv12_stride,
            nv12: vec![0u8; nv12_len],
            bgra_to_nv12,
            frame_index: 0,
            frame_duration,
            hardware,
            async_mode,
            pending_input: if async_mode { 1 } else { 0 },
            d3d,
            gpu_nv12,
            gpu_processor: None,
        };

        let probe = vec![0u8; (width * height * 4) as usize];
        let mut probe_bytes = 0usize;
        for _ in 0..8 {
            let data = encoder
                .encode_bgra_inner(&probe)
                .context("MF encoder probe encode failed")?;
            probe_bytes += data.len();
            if probe_bytes != 0 {
                info!(
                    "MF H.264 encoder ready ({}x{} @ {}fps, {} kbps, hw={}, async={}, probe={} bytes)",
                    width,
                    height,
                    config.framerate,
                    bitrate / 1000,
                    hardware,
                    async_mode,
                    probe_bytes
                );
                encoder.force_keyframe();
                return Ok(encoder);
            }
        }
        Err(anyhow!("MF encoder probe produced empty bitstream"))
    }

    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    pub fn force_keyframe(&mut self) {
        force_keyframe(&self.transform);
    }

    fn handle_async_event(&mut self, event: TransformEvent, out: &mut Vec<u8>) -> Result<()> {
        match event {
            TransformEvent::NeedInput => {
                self.pending_input += 1;
            }
            TransformEvent::HaveOutput => {
                if let Some(packet) = process_output_once(&self.transform, self.output_stream_id)? {
                    out.extend_from_slice(&packet);
                }
            }
            TransformEvent::Other(_) => {}
        }
        Ok(())
    }

    fn pump_async_events(&mut self, out: &mut Vec<u8>) -> Result<()> {
        if !self.async_mode {
            return Ok(());
        }
        while let Some(event) = read_transform_event(&self.transform, MF_EVENT_FLAG_NO_WAIT)? {
            self.handle_async_event(event, out)?;
        }
        Ok(())
    }

    fn wait_for_async_event(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let event = read_transform_event(&self.transform, MF_EVENT_FLAG_NONE)?
            .context("MFT event stream ended")?;
        self.handle_async_event(event, out)
    }

    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
        self.encode_bgra_inner(bgra)
    }

    pub fn encode_texture_bgra(
        &mut self,
        texture: &ID3D11Texture2D,
        src_width: u32,
        src_height: u32,
    ) -> Result<Vec<u8>> {
        let timestamp = self.frame_index * self.frame_duration;
        let d3d = self
            .d3d
            .as_ref()
            .context("GPU texture encode requires a D3D device")?;
        let gpu = self
            .gpu_nv12
            .as_ref()
            .context("GPU texture encode requires DXGI encoder samples")?;
        let recreate = self
            .gpu_processor
            .as_ref()
            .map(|(w, h, _, _)| *w != src_width || *h != src_height)
            .unwrap_or(true);
        if recreate {
            let processor = GpuVideoProcessor::new(
                d3d,
                src_width,
                src_height,
                self.width,
                self.height,
                (10_000_000i64 / self.frame_duration.max(1)) as u32,
            )?;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.width,
                Height: self.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut target = None;
            unsafe {
                d3d.device
                    .CreateTexture2D(&desc, None, Some(&mut target))
                    .context("creating GPU-native NV12 render target")?;
            }
            self.gpu_processor = Some((
                src_width,
                src_height,
                processor,
                target.context("GPU-native NV12 render target was null")?,
            ));
        }
        let sample = gpu.allocate_sample(timestamp, self.frame_duration)?;
        let output_texture = GpuNv12Frames::sample_texture(&sample)?;
        let (_, _, processor, render_target) = self.gpu_processor.as_ref().unwrap();
        processor
            .convert(texture, render_target)
            .context("D3D11 BGRA-to-NV12 video processing")?;
        unsafe {
            d3d.context.CopyResource(&output_texture, render_target);
        }
        self.frame_index += 1;
        self.process_sample(&sample)
    }

    fn pack_nv12_for_encoder(
        width: u32,
        height: u32,
        stride: u32,
        dst: &mut [u8],
        src: &[u8],
    ) -> usize {
        let tight_len = nv12_buffer_size(width, height) as usize;
        if stride == width {
            dst[..tight_len].copy_from_slice(&src[..tight_len]);
            tight_len
        } else {
            let packed = pack_nv12_strided(src, width, height, stride);
            dst[..packed.len()].copy_from_slice(&packed);
            packed.len()
        }
    }

    fn encode_bgra_inner(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
        let timestamp = self.frame_index * self.frame_duration;
        let frame_duration = self.frame_duration;
        let tight_len = nv12_buffer_size(self.width, self.height) as usize;
        let nv12_len = if let Some(conv) = &mut self.bgra_to_nv12 {
            match conv.convert(bgra, timestamp, frame_duration)? {
                Some(nv12) => Self::pack_nv12_for_encoder(
                    self.width,
                    self.height,
                    self.nv12_stride,
                    &mut self.nv12,
                    nv12,
                ),
                None => {
                    self.frame_index += 1;
                    return Ok(Vec::new());
                }
            }
        } else if self.nv12_stride == self.width {
            bgra_to_nv12(bgra, self.width, self.height, &mut self.nv12[..tight_len]);
            tight_len
        } else {
            let mut tight = vec![0u8; tight_len];
            bgra_to_nv12(bgra, self.width, self.height, &mut tight);
            Self::pack_nv12_for_encoder(
                self.width,
                self.height,
                self.nv12_stride,
                &mut self.nv12,
                &tight,
            )
        };
        let nv12 = &self.nv12[..nv12_len];
        let sample = if let (Some(d3d), Some(gpu)) = (&self.d3d, &self.gpu_nv12) {
            gpu.create_sample(
                d3d,
                &nv12[..tight_len.min(nv12.len())],
                timestamp,
                frame_duration,
            )
            .context("gpu allocator sample")?
        } else {
            create_media_sample(nv12, timestamp, frame_duration).context("cpu nv12 sample")?
        };
        self.frame_index += 1;
        self.process_sample(&sample)
    }

    fn process_sample(&mut self, sample: &IMFSample) -> Result<Vec<u8>> {
        let mut out = Vec::new();

        if self.async_mode {
            self.pump_async_events(&mut out)?;
            if self.pending_input == 0 {
                while self.pending_input == 0 {
                    self.wait_for_async_event(&mut out)?;
                }
            }
            self.pending_input -= 1;
            unsafe {
                self.transform
                    .ProcessInput(self.input_stream_id, sample, 0)
                    .context("async ProcessInput")?;
            }
            self.wait_for_async_event(&mut out)
                .context("async wait for encoder event after input")?;
            self.pump_async_events(&mut out)?;
        } else {
            unsafe {
                self.transform
                    .ProcessInput(self.input_stream_id, sample, 0)
                    .context("sync ProcessInput")?;
            }
            for packet in drain_output(&self.transform, self.output_stream_id)? {
                out.extend_from_slice(&packet);
            }
        }
        Ok(out)
    }
}

fn decoder_nv12_output_type(transform: &IMFTransform) -> Result<IMFMediaType> {
    for index in 0..64 {
        match unsafe { transform.GetOutputAvailableType(0, index) } {
            Ok(media_type) => {
                if unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }.ok() == Some(MFVideoFormat_NV12) {
                    return Ok(media_type);
                }
            }
            Err(error) if error.code() == MF_E_NO_MORE_TYPES => break,
            Err(error) => return Err(error.into()),
        }
    }
    Err(anyhow!("MF H.264 decoder did not offer NV12 output"))
}

fn video_aperture(media_type: &IMFMediaType, key: &GUID) -> Option<RECT> {
    let blob_size = unsafe { media_type.GetBlobSize(key) }.ok()? as usize;
    if blob_size < std::mem::size_of::<MFVideoArea>() {
        return None;
    }
    let mut blob = vec![0u8; blob_size];
    unsafe { media_type.GetBlob(key, &mut blob, None) }.ok()?;
    let area = unsafe { std::ptr::read_unaligned(blob.as_ptr().cast::<MFVideoArea>()) };
    Some(RECT {
        left: area.OffsetX.value as i32,
        top: area.OffsetY.value as i32,
        right: area.OffsetX.value as i32 + area.Area.cx,
        bottom: area.OffsetY.value as i32 + area.Area.cy,
    })
}

fn decoder_display_rect(media_type: &IMFMediaType, width: u32, height: u32) -> RECT {
    for key in [
        &MF_MT_MINIMUM_DISPLAY_APERTURE,
        &MF_MT_GEOMETRIC_APERTURE,
        &MF_MT_PAN_SCAN_APERTURE,
    ] {
        if let Some(rect) = video_aperture(media_type, key) {
            let (display_width, display_height) = rect_size(rect);
            if rect.left >= 0
                && rect.top >= 0
                && rect.right <= width as i32
                && rect.bottom <= height as i32
                && display_width > 0
                && display_height > 0
            {
                return rect;
            }
        }
    }

    fallback_display_rect(width, height)
}

fn fallback_display_rect(width: u32, height: u32) -> RECT {
    // Some H.264 decoders expose only the macroblock-aligned coded size. Wire's
    // negotiated stream sizes are 16:9, so trim a sub-macroblock bottom pad such
    // as 1920x1088 back to its 1920x1080 display area when no aperture is given.
    let expected_height = width.saturating_mul(9) / 16;
    let display_height = if width.saturating_mul(9) % 16 == 0
        && height >= expected_height
        && height - expected_height < 16
    {
        expected_height
    } else {
        height
    };
    RECT {
        left: 0,
        top: 0,
        right: width as i32,
        bottom: display_height as i32,
    }
}

pub struct MfH264Decoder {
    transform: IMFTransform,
    width: u32,
    height: u32,
    display_rect: RECT,
    configured: bool,
    async_mode: bool,
    nv12: Vec<u8>,
    rgba: Vec<u8>,
    presentation_pool: Option<DecoderPresentationPool>,
    presentation_generation: u64,
    native_output_logged: bool,
    native_output_failed: bool,
    frame_index: i64,
    frame_duration: i64,
    _d3d: Option<Arc<MfD3d>>,
}

impl MfH264Decoder {
    pub fn try_new() -> Result<Self> {
        ensure_media_foundation()?;
        let flags = MFT_ENUM_FLAG(
            MFT_ENUM_FLAG_SORTANDFILTER.0 | MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_ASYNCMFT.0,
        );
        let transform = find_transform(
            MFT_CATEGORY_VIDEO_DECODER,
            MFVideoFormat_H264,
            MFVideoFormat_NV12,
            flags,
        )
        .or_else(|_| {
            let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_SORTANDFILTER.0 | MFT_ENUM_FLAG_SYNCMFT.0);
            find_transform(
                MFT_CATEGORY_VIDEO_DECODER,
                MFVideoFormat_H264,
                MFVideoFormat_NV12,
                flags,
            )
        })?;

        // Async hardware MFTs must be unlocked before any other call, and we run in
        // low-latency mode so output is produced per input frame (no B-frame reordering).
        let async_mode = is_async_transform(&transform).unwrap_or(false);
        if async_mode {
            unlock_async_transform(&transform)?;
        }
        let d3d = match MfD3d::try_new(None) {
            Ok(d3d) => {
                let d3d = Arc::new(d3d);
                match d3d.attach_to_transform(&transform) {
                    Ok(()) => {
                        info!("MF decoder attached to a D3D11 device manager");
                        Some(d3d)
                    }
                    Err(error) => {
                        warn!("MF decoder rejected D3D11 device manager: {error:#}");
                        None
                    }
                }
            }
            Err(error) => {
                warn!("MF decoder D3D11 device creation failed: {error:#}");
                None
            }
        };
        if let Ok(attrs) = unsafe { transform.GetAttributes() } {
            unsafe {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }
        }
        if let Ok(api) = transform.cast::<ICodecAPI>() {
            set_codecapi_bool(&api, &CODECAPI_AVDecVideoAcceleration_H264, d3d.is_some());
            set_codecapi_bool(&api, &CODECAPI_AVLowLatencyMode, true);
        }

        let input_type = unsafe { MFCreateMediaType()? };
        unsafe {
            input_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            transform.SetInputType(0, &input_type, 0)?;
            // With only the two required H.264 input attributes, Windows exposes
            // a placeholder output type. It still must be selected before the
            // first ProcessInput call; the decoder later reports STREAM_CHANGE
            // once SPS/PPS reveal the actual dimensions.
            let output_type = decoder_nv12_output_type(&transform)?;
            transform.SetOutputType(0, &output_type, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        if async_mode {
            wait_for_transform_event(&transform, 0, METransformNeedInput)?;
        }

        info!(
            "MF H.264 decoder ready (async={async_mode}, d3d={})",
            d3d.is_some()
        );
        Ok(Self {
            transform,
            width: 0,
            height: 0,
            display_rect: RECT::default(),
            configured: false,
            async_mode,
            nv12: Vec::new(),
            rgba: Vec::new(),
            presentation_pool: None,
            presentation_generation: 0,
            native_output_logged: false,
            native_output_failed: false,
            frame_index: 0,
            frame_duration: 10_000_000 / 60,
            _d3d: d3d,
        })
    }

    fn renegotiate_output_type(&mut self) -> Result<()> {
        let output_type = decoder_nv12_output_type(&self.transform)?;
        let frame_size = unsafe { output_type.GetUINT64(&MF_MT_FRAME_SIZE)? };
        let width = (frame_size >> 32) as u32;
        let height = frame_size as u32;
        let display_rect = decoder_display_rect(&output_type, width, height);
        unsafe {
            self.transform.SetOutputType(0, &output_type, 0)?;
        }
        let dimensions_changed =
            self.width != width || self.height != height || self.display_rect != display_rect;
        self.width = width;
        self.height = height;
        self.display_rect = display_rect;
        self.configured = true;
        if dimensions_changed {
            self.nv12.resize((width * height * 3 / 2) as usize, 0);
            self.rgba.resize((width * height * 4) as usize, 0);
            self.presentation_generation = self.presentation_generation.wrapping_add(1);
            self.presentation_pool = self._d3d.as_ref().map(|d3d| {
                DecoderPresentationPool::new(
                    d3d.clone(),
                    width,
                    height,
                    display_rect,
                    self.presentation_generation,
                )
            });
            self.native_output_failed = false;
        }
        let (display_width, display_height) = rect_size(display_rect);
        info!(
            "MF decoder output negotiated: {width}x{height} coded, {display_width}x{display_height} display at ({}, {})",
            display_rect.left, display_rect.top
        );
        Ok(())
    }

    fn sample_to_frame(
        &mut self,
        sample: &IMFSample,
    ) -> Result<Option<crate::video_decode::DecodedFrame>> {
        let w = self.width;
        let h = self.height;
        let display_rect = self.display_rect;
        let (display_width, display_height) = rect_size(display_rect);
        if !self.native_output_failed {
            if let Some(pool) = &mut self.presentation_pool {
                match pool.copy_sample(sample) {
                    Ok(Some(frame)) => {
                        if !self.native_output_logged {
                            info!(
                                "MF decoder native D3D11 presentation enabled ({} pooled NV12 surfaces)",
                                DECODE_PRESENTATION_TEXTURES
                            );
                            self.native_output_logged = true;
                        }
                        return Ok(Some(crate::video_decode::DecodedFrame {
                            data: crate::video_decode::DecodedFrameData::D3d11(frame),
                            width: display_width,
                            height: display_height,
                        }));
                    }
                    Ok(None) => {
                        // All bounded presentation surfaces are still owned by the UI.
                        // Dropping this display frame is safe: the decoder has already
                        // consumed it and retained its reference picture internally.
                        return Ok(None);
                    }
                    Err(error) => {
                        warn!(
                            "MF native decoder surface handoff failed; using CPU presentation fallback: {error:#}"
                        );
                        self.native_output_failed = true;
                        self.presentation_pool = None;
                    }
                }
            }
        }
        let len = (w * h * 4) as usize;
        read_sample_topdown_into(sample, w, h, true, &mut self.nv12)?;
        if self.rgba.len() != len {
            self.rgba.resize(len, 0);
        }
        nv12_to_rgba(&self.nv12, w, h, &mut self.rgba);
        let out = crop_rgba(&self.rgba, w, h, display_rect);
        Ok(Some(crate::video_decode::DecodedFrame {
            data: crate::video_decode::DecodedFrameData::Rgba(Arc::new(out)),
            width: display_width,
            height: display_height,
        }))
    }

    fn drain_available_output(&mut self) -> Result<Vec<crate::video_decode::DecodedFrame>> {
        let mut frames = Vec::new();
        loop {
            match process_decoder_output_once(&self.transform, 0)? {
                DecoderOutput::Sample(sample) => {
                    if !self.configured {
                        self.renegotiate_output_type()?;
                    }
                    if let Some(frame) = self.sample_to_frame(&sample)? {
                        frames.push(frame);
                    }
                }
                DecoderOutput::StreamChange => {
                    self.configured = false;
                    self.renegotiate_output_type()?;
                }
                DecoderOutput::NeedMoreInput => break,
            }
        }
        Ok(frames)
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<crate::video_decode::DecodedFrame>> {
        let data = wire::video::bitstream::normalize_h264_for_decode(data);
        let timestamp = self.frame_index * self.frame_duration;
        let sample = create_h264_sample(&data, timestamp, self.frame_duration)?;
        self.frame_index += 1;
        let mut frames = Vec::new();
        let mut accepted = false;
        for _ in 0..4 {
            match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                Ok(()) => {
                    accepted = true;
                    break;
                }
                Err(error) if error.code() == MF_E_NOTACCEPTING => {
                    frames.extend(self.drain_available_output()?);
                }
                Err(error) => return Err(error.into()),
            }
        }
        if !accepted {
            return Err(anyhow!(
                "MF decoder remained NOT_ACCEPTING after draining output"
            ));
        }

        if self.async_mode {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
            loop {
                match read_transform_event(&self.transform, MF_EVENT_FLAG_NO_WAIT)? {
                    Some(TransformEvent::HaveOutput) => break,
                    Some(TransformEvent::NeedInput) => break,
                    Some(TransformEvent::Other(_)) => continue,
                    None => {
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
            }
        }
        frames.extend(self.drain_available_output()?);
        Ok(frames)
    }
}

use windows::Win32::System::Com::CoTaskMemFree;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crops_macroblock_padding_from_rgba_frames() {
        let width = 4u32;
        let height = 4u32;
        let src: Vec<u8> = (0..width * height * 4).map(|value| value as u8).collect();
        let rect = RECT {
            left: 0,
            top: 1,
            right: 4,
            bottom: 3,
        };
        let cropped = crop_rgba(&src, width, height, rect);
        assert_eq!(cropped.len(), 4 * 2 * 4);
        assert_eq!(&cropped[..16], &src[16..32]);
        assert_eq!(&cropped[16..], &src[32..48]);
    }

    #[test]
    fn trims_h264_macroblock_padding_without_aperture_metadata() {
        assert_eq!(rect_size(fallback_display_rect(1920, 1088)), (1920, 1080));
        assert_eq!(rect_size(fallback_display_rect(1280, 720)), (1280, 720));
    }
}
