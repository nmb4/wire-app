use anyhow::{Context, Result};
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, Level, Profile,
    RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, RgbaSliceU8, YUVBuffer, YUVSource};
use openh264::OpenH264API;
use tracing::info;

use crate::video::{default_bitrate, VideoConfig, VideoResolution};

fn h264_level(resolution: VideoResolution, framerate: u32) -> Level {
    match resolution {
        VideoResolution::P720 => Level::Level_3_1,
        VideoResolution::P1080 if framerate > 30 => Level::Level_4_1,
        VideoResolution::P1080 => Level::Level_4_0,
        VideoResolution::P1440 if framerate > 30 => Level::Level_5_1,
        VideoResolution::P1440 => Level::Level_5_0,
    }
}

pub struct VideoEncoder {
    encoder: Encoder,
    width: u32,
    height: u32,
}

impl VideoEncoder {
    pub fn new(config: &VideoConfig) -> Result<Self> {
        let width = config.resolution.width();
        let height = config.resolution.height();
        let bitrate_bps = default_bitrate(config.resolution, config.framerate);
        let enc_config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bitrate_bps))
            .complexity(Complexity::Low)
            .skip_frames(false)
            .num_threads(0)
            .max_frame_rate(FrameRate::from_hz(config.framerate as f32))
            .intra_frame_period(IntraFramePeriod::from_num_frames(config.framerate * 2))
            .profile(Profile::High)
            .level(h264_level(config.resolution, config.framerate))
            .scene_change_detect(true)
            .background_detection(false);
        info!(
            "OpenH264 encoder: {width}x{height} @ {} fps, {} kbps",
            config.framerate,
            bitrate_bps / 1000
        );
        let encoder =
            Encoder::with_api_config(OpenH264API::from_source(), enc_config)
                .context("failed to create H.264 encoder")?;
        let mut this = Self {
            encoder,
            width,
            height,
        };
        this.force_keyframe();
        Ok(this)
    }

    pub fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    pub fn encode(&mut self, rgba_data: &[u8]) -> Result<Vec<u8>> {
        let rgb_slice =
            RgbaSliceU8::new(rgba_data, (self.width as usize, self.height as usize));
        let yuv = YUVBuffer::from_rgb_source(rgb_slice);
        let bitstream = self.encoder.encode(&yuv)?;
        Ok(bitstream.to_vec())
    }

    pub fn encode_bgra(&mut self, bgra_data: &[u8]) -> Result<Vec<u8>> {
        let bgra = BgraSliceU8::new(bgra_data, (self.width as usize, self.height as usize));
        let yuv = YUVBuffer::from_rgb_source(bgra);
        let bitstream = self.encoder.encode(&yuv)?;
        Ok(bitstream.to_vec())
    }
}

pub struct VideoDecoder {
    decoder: Decoder,
}

impl VideoDecoder {
    pub fn new() -> Result<Self> {
        let decoder = Decoder::new().context("failed to create H.264 decoder")?;
        Ok(Self { decoder })
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
        let data = crate::video::bitstream::normalize_h264_for_decode(data);
        for packet in openh264::nal_units(&data) {
            if let Ok(Some(image)) = self.decoder.decode(packet) {
                let (w, h) = image.dimensions();
                let mut rgba = vec![0u8; image.rgba8_len()];
                image.write_rgba8(&mut rgba);
                return Ok((rgba, w as u32, h as u32));
            }
        }
        anyhow::bail!("no decodable frame found in H.264 data");
    }
}
