use std::{num::NonZeroUsize, ops::ControlFlow, time::Duration};

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use fixed_resample::{FixedResampler, ResampleQuality};
use ringbuf::{traits::Consumer as _, HeapCons as Consumer};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::{self, error::TryRecvError};
use tracing::{debug, error, info, trace};

use super::Codec;
use crate::{
    audio::{AudioFormat, AudioSink, AudioSource},
    rtc::{MediaFrame, MediaTrack, TrackKind},
};

pub const OPUS_SAMPLE_RATE: u32 = 48_000;
pub const OPUS_STREAM_PARAMS: AudioFormat = AudioFormat::new2(OPUS_SAMPLE_RATE, 2);

/// Opus stereo is two channels; expose it as a `NonZeroUsize` for the resampler.
const STEREO_CHANNELS: NonZeroUsize = NonZeroUsize::new(2).unwrap();

const DURATION_20MS: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum OpusChannels {
    Mono = 1,
    Stereo = 2,
}

impl From<OpusChannels> for ::opus::Channels {
    fn from(value: OpusChannels) -> Self {
        match value {
            OpusChannels::Mono => ::opus::Channels::Mono,
            OpusChannels::Stereo => ::opus::Channels::Stereo,
        }
    }
}

impl OpusChannels {
    /// The channel count as a `NonZeroUsize` for resampler construction.
    fn resampler_channels(self) -> NonZeroUsize {
        match self {
            OpusChannels::Mono => NonZeroUsize::MIN,
            OpusChannels::Stereo => STEREO_CHANNELS,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum AudioQuality {
    Low,
    Medium,
    #[default]
    High,
    Ultra,
}

impl AudioQuality {
    pub fn bitrate(&self) -> i32 {
        match self {
            AudioQuality::Low => 16_000,
            AudioQuality::Medium => 40_000,
            AudioQuality::High => 80_000,
            AudioQuality::Ultra => 160_000,
        }
    }

    pub fn channels(&self) -> OpusChannels {
        match self {
            AudioQuality::Low | AudioQuality::Medium => OpusChannels::Mono,
            AudioQuality::High | AudioQuality::Ultra => OpusChannels::Stereo,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        match self {
            AudioQuality::Low => 16_000,
            AudioQuality::Medium => 48_000,
            AudioQuality::High => 48_000,
            AudioQuality::Ultra => 48_000,
        }
    }

    pub fn bandwidth_human(&self) -> &'static str {
        match self {
            AudioQuality::Low => "~16 kbps",
            AudioQuality::Medium => "~40 kbps",
            AudioQuality::High => "~80 kbps",
            AudioQuality::Ultra => "~160 kbps",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            AudioQuality::Low => "Low (16 kbps, mono)",
            AudioQuality::Medium => "Medium (40 kbps, mono)",
            AudioQuality::High => "High (80 kbps, stereo)",
            AudioQuality::Ultra => "Ultra (160 kbps, stereo)",
        }
    }
}

impl std::fmt::Display for AudioQuality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioQuality::Low => write!(f, "low"),
            AudioQuality::Medium => write!(f, "medium"),
            AudioQuality::High => write!(f, "high"),
            AudioQuality::Ultra => write!(f, "ultra"),
        }
    }
}

impl std::str::FromStr for AudioQuality {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "low" => Ok(AudioQuality::Low),
            "medium" => Ok(AudioQuality::Medium),
            "high" => Ok(AudioQuality::High),
            "ultra" => Ok(AudioQuality::Ultra),
            _ => Err(format!(
                "unknown quality '{s}', expected low, medium, high, or ultra"
            )),
        }
    }
}

pub struct MediaTrackOpusDecoder {
    track: MediaTrack,
    decoder: opus::Decoder,
    audio_buf: Vec<f32>,
    decode_buf: Vec<f32>,
    underflows: usize,
    remaining_silence_ticks: usize,
    audio_format: AudioFormat,
}

impl MediaTrackOpusDecoder {
    pub fn new(track: MediaTrack) -> Result<Self> {
        let channel_count = match track.codec() {
            Codec::Opus { channels } => channels,
        };
        let audio_format = AudioFormat::new2(OPUS_SAMPLE_RATE, channel_count as u16);
        let decoder = opus::Decoder::new(OPUS_STREAM_PARAMS.sample_rate.0, channel_count.into())
            .context("failed to create the Opus decoder")?;
        let buffer_size = audio_format.sample_count(DURATION_20MS);
        let decode_buf = vec![0.; buffer_size];
        let audio_buf = vec![];
        Ok(Self {
            track,
            decoder,
            audio_buf,
            decode_buf,
            underflows: 0,
            remaining_silence_ticks: 0,
            audio_format,
        })
    }

    pub fn decode(&mut self, buf: &[u8]) -> Result<usize> {
        let block_count = self
            .decoder
            .decode_float(buf, &mut self.decode_buf, false)?;
        let sample_count = block_count * self.audio_format.channel_count as usize;
        let decoded = &self.decode_buf[..sample_count];
        // we need to upscale to two channels, AudioSource tick always expects stereo.
        match self.audio_format.channel_count {
            1 => self.audio_buf.extend(decoded.iter().flat_map(|s| [s, s])),
            2 => self.audio_buf.extend(decoded),
            channels => {
                anyhow::bail!("cannot upscale a {channels}-channel Opus frame to stereo")
            }
        }
        Ok(sample_count)
    }

    pub fn peek(&self) -> &[f32] {
        &self.audio_buf
    }

    pub fn advance(&mut self, n: usize) {
        // Callers only advance by samples they peeked from this buffer, so a
        // larger `n` is a decoder-loop bug, not bad input. Fail fast rather
        // than silently desynchronizing the audio stream.
        assert!(
            n <= self.audio_buf.len(),
            "requested advance of {n} beyond buffer length {}",
            self.audio_buf.len()
        );
        self.audio_buf.copy_within(n.., 0);
        self.audio_buf.truncate(self.audio_buf.len() - n);
    }
}

impl AudioSource for MediaTrackOpusDecoder {
    fn tick(&mut self, buf: &mut [f32]) -> Result<ControlFlow<(), usize>> {
        // decode everything that is ready to recv'd on the track channel.
        loop {
            let (skipped_frames, payload) = match self.track.try_recv() {
                Ok(frame) => {
                    let MediaFrame {
                        payload,
                        skipped_frames,
                        ..
                    } = frame;
                    trace!("opus decoder: mediatrack recv frame");
                    (skipped_frames, Some(payload))
                }
                Err(TryRecvError::Empty) => {
                    trace!("opus decoder: mediatrack recv empty");
                    break;
                }
                Err(TryRecvError::Lagged(count)) => {
                    trace!("opus decoder: mediatrack recv lagged {count}");
                    (Some(count as u32), None)
                }
                Err(TryRecvError::Closed) => {
                    info!("stop opus to audio loop: media track sender dropped");
                    return Ok(ControlFlow::Break(()));
                }
            };
            if let Some(skipped_count) = skipped_frames {
                for _ in 0..skipped_count {
                    let sample_count = self.decode(&[])?;
                    trace!(
                        "decoder: {sample_count} samples from skipped frames, now at {}",
                        self.audio_buf.len()
                    );
                }
            }
            if let Some(payload) = payload {
                let sample_count = self.decode(&payload)?;
                trace!(
                    "decoder: {sample_count} samples from payload, now at {}",
                    self.audio_buf.len()
                );
            }
        }

        // TODO: right now a very hacky way to add some latency if we don't get enough packets.
        if self.remaining_silence_ticks > 0 {
            self.remaining_silence_ticks -= 1;
            return Ok(ControlFlow::Continue(0));
        } else if self.audio_buf.len() < buf.len() {
            self.underflows += 1;
            if self.underflows > 2 {
                self.remaining_silence_ticks = 4;
                tracing::debug!("increase silence");
                self.underflows = 0;
            }
            return Ok(ControlFlow::Continue(0));
        }

        // TODO: a very hacky way to decrease latency if we buffered too much
        if self
            .audio_format
            .duration_from_sample_count(self.audio_buf.len())
            > Duration::from_secs(1)
        {
            self.advance(self.audio_format.sample_count(Duration::from_millis(500)));
        }

        let count = buf.len().min(self.audio_buf.len());
        buf.copy_from_slice(&self.audio_buf[..count]);
        self.advance(count);

        Ok(ControlFlow::Continue(count))
    }
}

pub struct MediaTrackOpusEncoder {
    sender: broadcast::Sender<MediaFrame>,
    encoder: OpusEncoder,
    input_channels: usize,
    output_channels: usize,
    input_pending: Vec<f32>,
    channel_input: Vec<f32>,
    resampled_input: Vec<f32>,
    resampler: Option<FixedResampler<f32, 2>>,
}

impl MediaTrackOpusEncoder {
    pub fn new(
        track_channel_cap: usize,
        audio_format: AudioFormat,
        quality: AudioQuality,
    ) -> Result<(Self, MediaTrack)> {
        Self::new_with_application(
            track_channel_cap,
            audio_format,
            quality,
            opus::Application::Voip,
        )
    }

    /// Encode full-band stereo content such as shared system audio.
    pub fn new_for_content(
        track_channel_cap: usize,
        audio_format: AudioFormat,
        quality: AudioQuality,
    ) -> Result<(Self, MediaTrack)> {
        Self::new_with_application(
            track_channel_cap,
            audio_format,
            quality,
            opus::Application::Audio,
        )
    }

    fn new_with_application(
        track_channel_cap: usize,
        audio_format: AudioFormat,
        quality: AudioQuality,
        application: opus::Application,
    ) -> Result<(Self, MediaTrack)> {
        anyhow::ensure!(
            matches!(audio_format.channel_count, 1 | 2) && audio_format.sample_rate.0 > 0,
            "Opus input requires mono or stereo audio with a nonzero sample rate"
        );
        let (sender, receiver) = broadcast::channel(track_channel_cap);
        let channels = quality.channels();
        let track = MediaTrack::new(receiver, Codec::Opus { channels }, TrackKind::Audio);
        let encoder = MediaTrackOpusEncoder {
            sender,
            encoder: OpusEncoder::new_with_application(quality, application),
            input_channels: audio_format.channel_count as usize,
            output_channels: channels as usize,
            input_pending: Vec::new(),
            channel_input: Vec::new(),
            resampled_input: Vec::new(),
            resampler: (audio_format.sample_rate.0 != quality.sample_rate()).then(|| {
                FixedResampler::new(
                    channels.resampler_channels(),
                    audio_format.sample_rate.0,
                    quality.sample_rate(),
                    ResampleQuality::High,
                    true,
                )
            }),
        };
        info!(
            input_format = ?audio_format,
            quality = %quality,
            output_sample_rate = quality.sample_rate(),
            output_channels = channels as u16,
            application = ?application,
            "configured Opus audio track"
        );
        Ok((encoder, track))
    }
}

impl AudioSink for MediaTrackOpusEncoder {
    fn tick(&mut self, buf: &[f32]) -> Result<ControlFlow<(), ()>> {
        // Capture uses engine-format audio regardless of the selected quality.
        // Convert channels and rate before framing it for the Opus encoder.
        self.input_pending.extend_from_slice(buf);
        let complete = self.input_pending.len() / self.input_channels * self.input_channels;
        for frame in self.input_pending[..complete].chunks_exact(self.input_channels) {
            match (self.input_channels, self.output_channels) {
                (2, 1) => self.channel_input.push((frame[0] + frame[1]) * 0.5),
                (1, 2) => self.channel_input.extend_from_slice(&[frame[0], frame[0]]),
                _ => self.channel_input.extend_from_slice(frame),
            }
        }
        self.input_pending.copy_within(complete.., 0);
        self.input_pending
            .truncate(self.input_pending.len() - complete);
        let samples = if let Some(resampler) = self.resampler.as_mut() {
            resampler.process_interleaved(
                &self.channel_input,
                |samples| self.resampled_input.extend_from_slice(samples),
                None,
                false,
            );
            &self.resampled_input
        } else {
            &self.channel_input
        };
        for (payload, sample_count) in self.encoder.push_slice(samples) {
            let payload_len = payload.len();
            let frame = MediaFrame {
                payload,
                sample_count: Some(sample_count),
                skipped_frames: None,
                skipped_samples: None,
            };
            match self.sender.send(frame) {
                Err(_) => {
                    info!("closing encoder loop: track receiver closed.");
                    return Ok(ControlFlow::Break(()));
                }
                Ok(_) => {
                    trace!("sent opus {sample_count}S {payload_len}B")
                }
            }
        }
        self.channel_input.clear();
        self.resampled_input.clear();
        Ok(ControlFlow::Continue(()))
    }
}

pub struct OpusEncoder {
    encoder: opus::Encoder,
    samples: Vec<f32>,
    out_buf: BytesMut,
    samples_per_frame: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::ENGINE_FORMAT;

    #[test]
    fn every_quality_preserves_engine_audio_duration() {
        for quality in [
            AudioQuality::Low,
            AudioQuality::Medium,
            AudioQuality::High,
            AudioQuality::Ultra,
        ] {
            let (mut encoder, mut track) =
                MediaTrackOpusEncoder::new(128, ENGINE_FORMAT, quality).unwrap();
            let mut packets = 0;
            for _ in 0..100 {
                assert!(matches!(
                    encoder.tick(&[0.25; 1_920]).unwrap(),
                    ControlFlow::Continue(())
                ));
                while track.try_recv().is_ok() {
                    packets += 1;
                }
            }
            assert!(
                (98..=100).contains(&packets),
                "{quality}: {packets} packets for two seconds"
            );
        }
    }

    #[test]
    fn opus_timestamps_use_the_48khz_clock_for_every_quality() {
        for quality in [
            AudioQuality::Low,
            AudioQuality::Medium,
            AudioQuality::High,
            AudioQuality::Ultra,
        ] {
            let mut encoder = OpusEncoder::new(quality);
            let samples = vec![
                0.25;
                AudioFormat::new2(quality.sample_rate(), quality.channels() as u16)
                    .sample_count(DURATION_20MS)
            ];
            let packets: Vec<_> = encoder.push_slice(&samples).collect();
            assert_eq!(packets.len(), 1);
            assert_eq!(packets[0].1, 960, "{quality}: wrong RTP clock increment");
        }
    }
}

impl OpusEncoder {
    pub fn new(quality: AudioQuality) -> Self {
        Self::new_with_application(quality, opus::Application::Voip)
    }

    pub fn new_with_application(quality: AudioQuality, application: opus::Application) -> Self {
        let channels = quality.channels();
        let sample_rate = quality.sample_rate();
        let format = AudioFormat::new2(sample_rate, channels as u16);
        // Opus only rejects an invalid sample rate or channel count, and both
        // come from the fixed `AudioQuality` table, so construction cannot
        // fail. This public constructor returns `Self`, so the error cannot be
        // propagated; assert the invariant at the narrowest site instead.
        #[allow(clippy::unwrap_used)]
        let mut encoder = opus::Encoder::new(sample_rate, channels.into(), application).unwrap();
        encoder
            .set_bitrate(opus::Bitrate::Bits(quality.bitrate()))
            .ok();
        debug!(
            "initialized opus encoder: channels {} bitrate {} bandwidth {:?}",
            channels as u16,
            quality.bitrate(),
            encoder.get_bandwidth()
        );
        let mut out_buf = BytesMut::new();
        let samples_per_frame = format.sample_count(DURATION_20MS);
        out_buf.resize(samples_per_frame, 0);
        let samples = Vec::new();
        Self {
            encoder,
            out_buf,
            samples,
            samples_per_frame,
        }
    }

    pub fn pop_from_consumer<'a>(
        &'a mut self,
        consumer: &'a mut Consumer<f32>,
    ) -> impl Iterator<Item = (Bytes, u32)> + 'a {
        std::iter::from_fn(|| {
            for sample in consumer.pop_iter() {
                if let Some((payload, sample_count)) = self.push_sample(sample) {
                    return Some((payload, sample_count));
                }
            }
            None
        })
    }

    pub fn push_slice<'a>(
        &'a mut self,
        samples: &'a [f32],
    ) -> impl Iterator<Item = (Bytes, u32)> + 'a {
        let mut iter = samples.iter();
        std::iter::from_fn(move || {
            for sample in iter.by_ref() {
                if let Some((payload, sample_count)) = self.push_sample(*sample) {
                    return Some((payload, sample_count));
                }
            }
            None
        })
    }

    pub fn push_sample(&mut self, sample: f32) -> Option<(Bytes, u32)> {
        self.samples.push(sample);
        if self.samples.len() >= self.samples_per_frame {
            // Opus RTP always uses a 48 kHz clock, independent of encoded
            // sample rate and channel count. Every packet here spans 20 ms.
            let sample_count = OPUS_SAMPLE_RATE / 50;
            let size = match self.encoder.encode_float(&self.samples, &mut self.out_buf) {
                Ok(size) => size,
                Err(error) => {
                    // The output buffer is sized for a full frame, so an
                    // encode failure means the encoder is unusable. Drop this
                    // frame instead of panicking the audio thread.
                    error!("failed to encode an Opus frame, dropping it: {error}");
                    self.samples.clear();
                    self.out_buf.resize(self.samples_per_frame, 0);
                    return None;
                }
            };
            self.samples.clear();
            let encoded = self.out_buf.split_to(size).freeze();
            self.out_buf.resize(self.samples_per_frame, 0);
            Some((encoded, sample_count))
        } else {
            None
        }
    }
}
