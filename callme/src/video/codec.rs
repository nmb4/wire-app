use anyhow::{Context, Result};
use openh264::decoder::Decoder;
use openh264::encoder::Encoder;
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};

use crate::video::VideoConfig;

pub struct VideoEncoder {
    encoder: Encoder,
    width: u32,
    height: u32,
}

impl VideoEncoder {
    pub fn new(config: &VideoConfig) -> Result<Self> {
        let width = config.resolution.width();
        let height = config.resolution.height();
        let encoder = Encoder::new().context("failed to create H.264 encoder")?;
        Ok(Self {
            encoder,
            width,
            height,
        })
    }

    pub fn encode(&mut self, rgba_data: &[u8]) -> Result<Vec<u8>> {
        let rgb_slice =
            RgbaSliceU8::new(rgba_data, (self.width as usize, self.height as usize));
        let yuv = YUVBuffer::from_rgb_source(rgb_slice);
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
        for packet in openh264::nal_units(data) {
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
