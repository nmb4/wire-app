use std::{num::NonZeroUsize, time::Duration};

use anyhow::Result;
use bytes::Bytes;
use cpal::{ChannelCount, SampleRate};
use fixed_resample::{FixedResampler, ResampleQuality};

use self::{
    capture::AudioCapture, device::list_devices, playback::AudioPlayback,
    ringbuf_pipe::ringbuf_pipe,
};
pub use self::{
    capture::AudioSink,
    device::{AudioConfig, Devices, Direction},
    playback::{AudioSource, VolumeHandle},
};
pub use crate::codec::opus::AudioQuality;
use crate::rtc::MediaTrack;

#[cfg(feature = "audio-processing")]
mod processor;
#[cfg(feature = "audio-processing")]
pub use processor::WebrtcAudioProcessor;

#[cfg(not(feature = "audio-processing"))]
#[derive(Debug, Clone)]
pub struct WebrtcAudioProcessor;

mod capture;
mod device;
mod playback;

pub const SAMPLE_RATE: SampleRate = SampleRate(48_000);
pub const ENGINE_FORMAT: AudioFormat = AudioFormat::new(SAMPLE_RATE, 2);

const DURATION_10MS: Duration = Duration::from_millis(10);
const DURATION_20MS: Duration = Duration::from_millis(20);

fn device_resampler(
    channels: NonZeroUsize,
    input_rate: u32,
    output_rate: u32,
) -> FixedResampler<f32, 2> {
    #[cfg(target_os = "macos")]
    {
        use fixed_resample::rubato::{FastFixedIn, PolynomialDegree};

        // fixed-resample's convenience constructor uses 1,024-frame blocks,
        // which is 64 ms at a 16 kHz Bluetooth device. Use 10 ms blocks so
        // capture and playback can continuously feed the 20 ms call pipeline.
        let input_block_frames = (input_rate as usize / 100).max(1);
        let inner = FastFixedIn::<f32>::new(
            output_rate as f64 / input_rate as f64,
            1.0,
            PolynomialDegree::Linear,
            input_block_frames,
            channels.get(),
        )
        .expect("valid fixed device resampling ratio");
        FixedResampler::from_custom(inner, input_rate, output_rate, true)
    }

    #[cfg(not(target_os = "macos"))]
    {
        FixedResampler::new(
            channels,
            input_rate,
            output_rate,
            ResampleQuality::High,
            true,
        )
    }
}

#[derive(Debug, Clone)]
pub struct AudioContext {
    playback: AudioPlayback,
    capture: AudioCapture,
}

impl AudioContext {
    pub async fn list_devices() -> Result<Devices> {
        tokio::task::spawn_blocking(list_devices).await?
    }

    pub fn list_devices_sync() -> Result<Devices> {
        list_devices()
    }

    /// Create a new [`AudioContext`].
    pub async fn new(config: AudioConfig) -> Result<Self> {
        let host = cpal::default_host();

        #[cfg(feature = "audio-processing")]
        let processor = WebrtcAudioProcessor::new(config.processing_enabled)?;
        #[cfg(not(feature = "audio-processing"))]
        let processor = WebrtcAudioProcessor;

        let capture = AudioCapture::build(
            &host,
            config.input_device.as_deref(),
            processor.clone(),
            config.quality,
        )
        .await?;
        let playback =
            AudioPlayback::build(&host, config.output_device.as_deref(), processor.clone()).await?;
        Ok(Self { playback, capture })
    }

    pub async fn capture_track(&self) -> Result<MediaTrack> {
        self.capture.create_opus_track().await
    }

    pub async fn play_track(&self, track: MediaTrack) -> Result<()> {
        self.playback.add_track(track).await?;
        Ok(())
    }

    pub async fn play_track_with_volume(
        &self,
        track: MediaTrack,
        volume: VolumeHandle,
    ) -> Result<()> {
        self.playback.add_track_with_volume(track, volume).await?;
        Ok(())
    }

    /// Stop or resume sending microphone samples to active call tracks.
    pub fn set_muted(&self, muted: bool) {
        self.capture.set_muted(muted);
    }

    /// Stop or resume the mixed remote-call audio output.
    pub fn set_deafened(&self, deafened: bool) {
        self.playback.set_deafened(deafened);
    }

    pub async fn feedback_encoded(&self) -> Result<()> {
        let track = self.capture_track().await?;
        self.play_track(track).await?;
        Ok(())
    }

    pub async fn feedback_raw(&self) -> Result<()> {
        let buffer_size = ENGINE_FORMAT.sample_count(DURATION_20MS * 16);
        let (sink, source) = ringbuf_pipe(buffer_size);
        self.capture.add_sink(sink).await?;
        self.playback.add_source(source).await?;
        Ok(())
    }
}

mod ringbuf_pipe {
    use std::ops::ControlFlow;

    use anyhow::Result;
    use ringbuf::{
        traits::{Consumer as _, Observer, Producer as _, Split},
        HeapCons as Consumer, HeapProd as Producer,
    };
    use tracing::warn;

    use super::{AudioSink, AudioSource};

    pub struct RingbufSink(Producer<f32>);
    pub struct RingbufSource(Consumer<f32>);

    pub fn ringbuf_pipe(buffer_size: usize) -> (RingbufSink, RingbufSource) {
        let (producer, consumer) = ringbuf::HeapRb::<f32>::new(buffer_size).split();
        (RingbufSink(producer), RingbufSource(consumer))
    }

    impl AudioSink for RingbufSink {
        fn tick(&mut self, buf: &[f32]) -> Result<ControlFlow<(), ()>> {
            let len = self.0.push_slice(buf);
            if len < buf.len() {
                warn!("ringbuf sink xrun: failed to send {}", buf.len() - len);
            }
            Ok(ControlFlow::Continue(()))
        }
    }

    impl AudioSource for RingbufSource {
        fn tick(&mut self, buf: &mut [f32]) -> Result<ControlFlow<(), usize>> {
            let len = self.0.pop_slice(buf);
            if len < buf.len() {
                warn!("ringbuf source xrun: failed to recv {}", buf.len() - len);
            }
            Ok(ControlFlow::Continue(len))
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AudioFormat {
    pub sample_rate: SampleRate,
    pub channel_count: ChannelCount,
}

impl AudioFormat {
    pub const fn new(sample_rate: SampleRate, channel_count: ChannelCount) -> Self {
        Self {
            sample_rate,
            channel_count,
        }
    }
    pub const fn new2(sample_rate: u32, channel_count: u16) -> Self {
        Self {
            sample_rate: SampleRate(sample_rate),
            channel_count,
        }
    }

    pub fn duration_from_sample_count(&self, sample_count: usize) -> Duration {
        Duration::from_secs_f32(
            (sample_count as f32 / self.channel_count as f32) / self.sample_rate.0 as f32,
        )
    }

    pub const fn block_count(&self, duration: Duration) -> usize {
        (self.sample_rate.0 as usize / 1000) * duration.as_millis() as usize
    }

    pub const fn sample_count(&self, duration: Duration) -> usize {
        self.block_count(duration) * self.channel_count as usize
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn distinguishes_device_frames_from_interleaved_samples() {
        assert_eq!(ENGINE_FORMAT.block_count(DURATION_20MS), 960);
        assert_eq!(ENGINE_FORMAT.sample_count(DURATION_20MS), 1_920);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn device_resampler_emits_twenty_ms_without_batching() {
        let mut capture = device_resampler(NonZeroUsize::new(2).unwrap(), 16_000, 48_000);
        let capture_input = vec![0.0; 320 * 2];
        let mut capture_output = 0;
        capture.process_interleaved(
            &capture_input,
            |samples| capture_output += samples.len(),
            None,
            false,
        );
        assert!((960..=1_920).contains(&capture_output));

        let mut playback = device_resampler(NonZeroUsize::new(1).unwrap(), 48_000, 16_000);
        let playback_input = vec![0.0; 960];
        let mut playback_output = 0;
        playback.process_interleaved(
            &playback_input,
            |samples| playback_output += samples.len(),
            None,
            false,
        );
        assert!((160..=320).contains(&playback_output));
    }
}
