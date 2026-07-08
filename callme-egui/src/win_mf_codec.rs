//! Windows Media Foundation hardware H.264 encoder/decoder.

use std::sync::Once;

use anyhow::{anyhow, Context, Result};
use callme::video::VideoConfig;
use tracing::{info, warn};
use windows::core::Interface;
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VariantClear, VT_BOOL, VARIANT};

use crate::yuv_convert::{bgra_to_nv12, bgra_to_rgba, nv12_to_rgba};

static MF_INIT: Once = Once::new();

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
            info!("found MFT candidate #{}", i + 1);
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

fn enumerate_video_encoders(prefer_hardware: bool) -> Result<Vec<IMFTransform>> {
    let base = MFT_ENUM_FLAG_SORTANDFILTER;
    let flags = if prefer_hardware {
        MFT_ENUM_FLAG(base.0 | MFT_ENUM_FLAG_HARDWARE.0)
    } else {
        MFT_ENUM_FLAG(base.0 | MFT_ENUM_FLAG_SYNCMFT.0)
    };
    enumerate_transforms(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFVideoFormat_NV12,
        MFVideoFormat_H264,
        flags,
    )
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
    expected: MF_EVENT_TYPE,
) -> Result<()> {
    let events: IMFMediaEventGenerator = transform.cast()?;
    loop {
        let event = unsafe { events.GetEvent(MF_EVENT_FLAG_NONE)? };
        let event_type = unsafe { event.GetType()? };
        if event_type == expected.0 as u32 {
            return Ok(());
        }
        if event_type == METransformHaveOutput.0 as u32 {
            let _ = drain_output(transform)?;
        }
    }
}

const H264_PROFILE_BASELINE: u32 = 66;

fn configure_encoder(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    framerate: u32,
    bitrate: u32,
    async_mode: bool,
) -> Result<()> {
    // Async hardware encoders must be unlocked before SetInputType (and related calls).
    if async_mode {
        unlock_async_transform(transform)?;
    }

    let output_type = create_video_media_type(
        MFVideoFormat_H264,
        width,
        height,
        framerate,
        Some(bitrate),
        true,
    )?;
    let input_type = create_video_media_type(
        MFVideoFormat_NV12,
        width,
        height,
        framerate,
        None,
        false,
    )?;

    unsafe {
        output_type.SetUINT32(&MF_MT_MPEG2_PROFILE, H264_PROFILE_BASELINE)?;
        transform.SetOutputType(0, &output_type, 0)?;
        transform.SetInputType(0, &input_type, 0)?;
    }
    enable_low_latency(transform);
    send_transform_message(transform, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING)?;
    send_transform_message(transform, MFT_MESSAGE_NOTIFY_START_OF_STREAM)?;
    if async_mode {
        wait_for_transform_event(transform, METransformNeedInput)?;
    }
    Ok(())
}

fn send_transform_message(transform: &IMFTransform, message: MFT_MESSAGE_TYPE) -> Result<()> {
    unsafe {
        transform
            .ProcessMessage(message, 0)
            .map_err(|e| anyhow!("MFT message {:?} failed: {e}", message.0))?;
    }
    Ok(())
}

fn enable_low_latency(transform: &IMFTransform) {
    unsafe {
        if let Ok(api) = transform.cast::<ICodecAPI>() {
            let mut variant = VARIANT::default();
            let inner = &mut *variant.Anonymous.Anonymous;
            inner.vt = VT_BOOL;
            inner.Anonymous.boolVal = VARIANT_TRUE;
            let _ = api.SetValue(&CODECAPI_AVEncCommonLowLatency, &variant);
            let _ = VariantClear(&mut variant);
        }
    }
}

fn nv12_buffer_size(width: u32, height: u32) -> u32 {
    width * (height + height / 2)
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

fn create_h264_sample(data: &[u8]) -> Result<IMFSample> {
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
    }
    Ok(sample)
}

fn drain_output(transform: &IMFTransform) -> Result<Vec<Vec<u8>>> {
    let mut packets = Vec::new();
    loop {
        let mut buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        let result = unsafe { transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
        match result {
            Ok(()) => {
                let sample = unsafe { buffer.pSample.take() }
                    .ok_or_else(|| anyhow!("MFT output missing sample"))?;
                packets.push(read_sample_bytes(&sample)?);
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
            Err(e) => return Err(e.into()),
        }
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
        let transform = find_transform(
            MFT_CATEGORY_VIDEO_PROCESSOR,
            input,
            output,
            flags,
        )?;
        configure_processor(&transform, input, output, width, height)?;
        let scratch_len = if output == MFVideoFormat_NV12 {
            (width * height * 3 / 2) as usize
        } else {
            (width * height * 4) as usize
        };
        Ok(Self {
            transform,
            scratch: vec![0u8; scratch_len],
        })
    }

    fn convert(&mut self, input: &[u8], timestamp: i64, duration: i64) -> Result<Option<&[u8]>> {
        let sample = create_media_sample(input, timestamp, duration)?;
        unsafe {
            self.transform.ProcessInput(0, &sample, 0)?;
        }
        let packets = drain_output(&self.transform)?;
        let Some(packet) = packets.into_iter().next() else {
            return Ok(None);
        };
        if packet.len() > self.scratch.len() {
            self.scratch.resize(packet.len(), 0);
        }
        self.scratch[..packet.len()].copy_from_slice(&packet);
        Ok(Some(&self.scratch[..packet.len()]))
    }
}

pub struct MfH264Encoder {
    transform: IMFTransform,
    width: u32,
    height: u32,
    nv12: Vec<u8>,
    bgra_to_nv12: Option<MfColorConverter>,
    frame_index: i64,
    frame_duration: i64,
    hardware: bool,
    async_mode: bool,
    pending_input: u32,
}

impl MfH264Encoder {
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
            for transform in candidates {
                match Self::build(
                    transform,
                    config,
                    width,
                    height,
                    bitrate,
                    prefer_hardware,
                ) {
                    Ok(enc) => return Ok(enc),
                    Err(e) => {
                        warn!("MF {label} H.264 encoder candidate failed: {e:?}");
                        last_err = Some(e);
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("no Media Foundation encoder found")))
    }

    fn build(
        transform: IMFTransform,
        config: &VideoConfig,
        width: u32,
        height: u32,
        bitrate: u32,
        prefer_hardware: bool,
    ) -> Result<Self> {
        let async_mode = is_async_transform(&transform)?;
        let hardware = prefer_hardware
            && unsafe {
                transform
                    .GetAttributes()
                    .ok()
                    .and_then(|attrs| attrs.GetUINT32(&MFT_ENUM_HARDWARE_URL_Attribute).ok())
                    .is_some()
            };

        configure_encoder(
            &transform,
            width,
            height,
            config.framerate,
            bitrate,
            async_mode,
        )?;
        force_keyframe(&transform);

        let bgra_to_nv12 = match MfColorConverter::try_new(
            MFVideoFormat_RGB32,
            MFVideoFormat_NV12,
            width,
            height,
        ) {
            Ok(conv) => {
                info!("MF color converter ready (BGRA -> NV12)");
                Some(conv)
            }
            Err(e) => {
                info!("MF color converter unavailable, using CPU YUV: {e:?}");
                None
            }
        };

        info!(
            "MF H.264 encoder ready ({}x{} @ {}fps, {} kbps, hw={}, async={})",
            width,
            height,
            config.framerate,
            bitrate / 1000,
            hardware,
            async_mode
        );

        let frame_duration = 10_000_000i64 / config.framerate.max(1) as i64;
        let nv12_len = nv12_buffer_size(width, height) as usize;

        Ok(Self {
            transform,
            width,
            height,
            nv12: vec![0u8; nv12_len],
            bgra_to_nv12,
            frame_index: 0,
            frame_duration,
            hardware,
            async_mode,
            pending_input: if async_mode { 1 } else { 0 },
        })
    }

    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    pub fn force_keyframe(&mut self) {
        force_keyframe(&self.transform);
    }

    fn pump_async_events(&mut self) -> Result<()> {
        if !self.async_mode {
            return Ok(());
        }
        let events: IMFMediaEventGenerator = self.transform.cast()?;
        loop {
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(e) => return Err(e.into()),
            };
            let event_type = unsafe { event.GetType()? };
            if event_type == METransformNeedInput.0 as u32 {
                self.pending_input += 1;
            } else if event_type == METransformHaveOutput.0 as u32 {
                // handled by the caller via ProcessOutput
            }
        }
        Ok(())
    }

    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
        let timestamp = self.frame_index * self.frame_duration;
        let nv12: &[u8] = if let Some(conv) = &mut self.bgra_to_nv12 {
            match conv.convert(bgra, timestamp, self.frame_duration)? {
                Some(nv12) => nv12,
                None => {
                    self.frame_index += 1;
                    return Ok(Vec::new());
                }
            }
        } else {
            bgra_to_nv12(bgra, self.width, self.height, &mut self.nv12);
            &self.nv12
        };
        let expected_len = nv12_buffer_size(self.width, self.height) as usize;
        if nv12.len() < expected_len {
            return Err(anyhow!(
                "NV12 buffer too small: got {} bytes, expected {expected_len}",
                nv12.len()
            ));
        }
        let sample = create_media_sample(&nv12[..expected_len], timestamp, self.frame_duration)?;
        self.frame_index += 1;

        if self.async_mode {
            self.pump_async_events()?;
            if self.pending_input == 0 {
                wait_for_transform_event(&self.transform, METransformNeedInput)?;
                self.pending_input = 1;
            }
            self.pending_input -= 1;
            unsafe {
                self.transform.ProcessInput(0, &sample, 0)?;
            }
            wait_for_transform_event(&self.transform, METransformHaveOutput)?;
        } else {
            unsafe {
                self.transform.ProcessInput(0, &sample, 0)?;
            }
        }

        let mut out = Vec::new();
        for packet in drain_output(&self.transform)? {
            out.extend_from_slice(&packet);
        }
        Ok(out)
    }
}

pub struct MfH264Decoder {
    transform: IMFTransform,
    width: u32,
    height: u32,
    configured: bool,
    nv12_to_rgb: Option<MfColorConverter>,
    rgba: Vec<u8>,
}

impl MfH264Decoder {
    pub fn try_new() -> Result<Self> {
        ensure_media_foundation()?;
        let flags = MFT_ENUM_FLAG(
            MFT_ENUM_FLAG_SORTANDFILTER.0
                | MFT_ENUM_FLAG_HARDWARE.0
                | MFT_ENUM_FLAG_ASYNCMFT.0,
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

        let input_type = unsafe { MFCreateMediaType()? };
        unsafe {
            input_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            transform.SetInputType(0, &input_type, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        info!("MF H.264 decoder ready");
        Ok(Self {
            transform,
            width: 0,
            height: 0,
            configured: false,
            nv12_to_rgb: None,
            rgba: Vec::new(),
        })
    }

    fn ensure_output_type(&mut self) -> Result<()> {
        if self.configured {
            return Ok(());
        }
        let output_type = unsafe { self.transform.GetOutputAvailableType(0, 0)? };
        let frame_size = unsafe { output_type.GetUINT64(&MF_MT_FRAME_SIZE)? };
        let width = (frame_size >> 32) as u32;
        let height = frame_size as u32;
        unsafe {
            self.transform.SetOutputType(0, &output_type, 0)?;
        }
        self.width = width;
        self.height = height;
        self.configured = true;
        self.nv12_to_rgb = match MfColorConverter::try_new(
            MFVideoFormat_NV12,
            MFVideoFormat_RGB32,
            width,
            height,
        ) {
            Ok(conv) => {
                info!("MF color converter ready (NV12 -> RGBA)");
                Some(conv)
            }
            Err(e) => {
                info!("MF NV12->RGB converter unavailable, using CPU YUV: {e:?}");
                None
            }
        };
        info!("MF decoder output negotiated: {width}x{height}");
        Ok(())
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
        let data = callme::video::bitstream::normalize_h264_for_decode(data);
        let sample = create_h264_sample(&data)?;
        unsafe {
            self.transform.ProcessInput(0, &sample, 0)?;
        }
        if !self.configured {
            self.ensure_output_type()?;
        }

        let packets = drain_output(&self.transform)?;
        let nv12 = packets
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("MF decoder produced no output"))?;

        let w = self.width;
        let h = self.height;
        let len = (w * h * 4) as usize;
        if self.rgba.len() != len {
            self.rgba.resize(len, 0);
        }
        if let Some(conv) = &mut self.nv12_to_rgb {
            let bgra = conv
                .convert(&nv12, 0, 0)?
                .ok_or_else(|| anyhow!("MF NV12->RGB converter produced no output"))?;
            bgra_to_rgba(bgra, &mut self.rgba);
        } else {
            nv12_to_rgba(&nv12, w, h, &mut self.rgba);
        }
        let mut out = Vec::with_capacity(len);
        std::mem::swap(&mut self.rgba, &mut out);
        self.rgba.resize(len, 0);
        Ok((out, w, h))
    }
}

use windows::Win32::System::Com::CoTaskMemFree;