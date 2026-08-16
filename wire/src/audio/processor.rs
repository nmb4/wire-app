use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::{bail, ensure, Result};
use sonora::{
    config::{EchoCanceller, HighPassFilter},
    AudioProcessing, Config, StreamConfig,
};
use tracing::{debug, info};

use super::SAMPLE_RATE;

const FRAME_DURATION_MS: usize = 10;
const MAX_STREAM_DELAY_MS: u64 = 500;

#[derive(Clone, Debug)]
pub struct WebrtcAudioProcessor(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    processor: Mutex<Option<ProcessorState>>,
    capture_delay: AtomicU64,
    playback_delay: AtomicU64,
    enabled: AtomicBool,
    capture_channels: AtomicUsize,
    playback_channels: AtomicUsize,
}

#[derive(Debug)]
struct ProcessorState {
    processor: AudioProcessing,
    capture_frame: PlanarFrame,
    render_frame: PlanarFrame,
}

#[derive(Debug)]
enum PlanarFrame {
    Mono {
        input: Vec<f32>,
        output: Vec<f32>,
    },
    Stereo {
        input_left: Vec<f32>,
        input_right: Vec<f32>,
        output_left: Vec<f32>,
        output_right: Vec<f32>,
    },
}

impl PlanarFrame {
    fn new(channels: usize) -> Result<Self> {
        let samples_per_channel = SAMPLE_RATE.0 as usize * FRAME_DURATION_MS / 1000;
        match channels {
            1 => Ok(Self::Mono {
                input: vec![0.0; samples_per_channel],
                output: vec![0.0; samples_per_channel],
            }),
            2 => Ok(Self::Stereo {
                input_left: vec![0.0; samples_per_channel],
                input_right: vec![0.0; samples_per_channel],
                output_left: vec![0.0; samples_per_channel],
                output_right: vec![0.0; samples_per_channel],
            }),
            channels => bail!("audio processing supports one or two channels, got {channels}"),
        }
    }

    fn process_capture(
        &mut self,
        processor: &mut AudioProcessing,
        frame: &mut [f32],
    ) -> Result<()> {
        self.process(frame, |input, output| {
            processor.process_capture_f32(input, output)
        })
    }

    fn process_render(&mut self, processor: &mut AudioProcessing, frame: &mut [f32]) -> Result<()> {
        self.process(frame, |input, output| {
            processor.process_render_f32(input, output)
        })
    }

    fn process(
        &mut self,
        frame: &mut [f32],
        process: impl FnOnce(&[&[f32]], &mut [&mut [f32]]) -> Result<(), sonora::Error>,
    ) -> Result<()> {
        match self {
            Self::Mono { input, output } => {
                ensure!(
                    frame.len() == input.len(),
                    "expected {} mono samples, got {}",
                    input.len(),
                    frame.len()
                );
                input.copy_from_slice(frame);
                process(&[input], &mut [output])?;
                frame.copy_from_slice(output);
            }
            Self::Stereo {
                input_left,
                input_right,
                output_left,
                output_right,
            } => {
                let expected_len = input_left.len() * 2;
                ensure!(
                    frame.len() == expected_len,
                    "expected {expected_len} interleaved stereo samples, got {}",
                    frame.len()
                );
                for (sample, (left, right)) in frame
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .zip(input_left.iter_mut().zip(input_right.iter_mut()))
                {
                    *left = sample[0];
                    *right = sample[1];
                }
                process(&[input_left, input_right], &mut [output_left, output_right])?;
                for (sample, (left, right)) in frame
                    .as_chunks_mut::<2>()
                    .0
                    .iter_mut()
                    .zip(output_left.iter().zip(output_right.iter()))
                {
                    sample[0] = *left;
                    sample[1] = *right;
                }
            }
        }
        Ok(())
    }
}

impl WebrtcAudioProcessor {
    pub fn new(enabled: bool) -> Result<Self> {
        info!("init audio processor (enabled={enabled})");
        Ok(Self(Arc::new(Inner {
            processor: Mutex::new(None),
            capture_delay: AtomicU64::default(),
            playback_delay: AtomicU64::default(),
            enabled: AtomicBool::new(enabled),
            capture_channels: AtomicUsize::default(),
            playback_channels: AtomicUsize::default(),
        })))
    }

    pub fn is_enabled(&self) -> bool {
        self.0.enabled.load(Ordering::SeqCst)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.0.enabled.store(enabled, Ordering::SeqCst);
    }

    pub fn init_capture(&self, channels: usize) -> Result<()> {
        self.0.capture_channels.store(channels, Ordering::SeqCst);
        if self.0.playback_channels.load(Ordering::SeqCst) > 0 {
            self.init()?;
        }
        Ok(())
    }

    pub fn init_playback(&self, channels: usize) -> Result<()> {
        self.0.playback_channels.store(channels, Ordering::SeqCst);
        if self.0.capture_channels.load(Ordering::SeqCst) > 0 {
            self.init()?;
        }
        Ok(())
    }

    fn init(&self) -> Result<()> {
        let capture_channels = self.0.capture_channels.load(Ordering::SeqCst);
        let playback_channels = self.0.playback_channels.load(Ordering::SeqCst);
        let capture_frame = PlanarFrame::new(capture_channels)?;
        let render_frame = PlanarFrame::new(playback_channels)?;
        let config = Config {
            high_pass_filter: Some(HighPassFilter::default()),
            echo_canceller: Some(EchoCanceller::default()),
            ..Config::default()
        };
        let mut processor = AudioProcessing::builder()
            .config(config)
            .capture_config(StreamConfig::new(
                SAMPLE_RATE.0,
                capture_channels.try_into()?,
            ))
            .render_config(StreamConfig::new(
                SAMPLE_RATE.0,
                playback_channels.try_into()?,
            ))
            .build();
        processor.set_stream_delay_ms(self.total_stream_delay_ms() as i32)?;
        *self.0.processor.lock().unwrap() = Some(ProcessorState {
            processor,
            capture_frame,
            render_frame,
        });
        info!(capture_channels, playback_channels, "audio processor ready");
        Ok(())
    }

    pub fn process_capture_frame(&self, frame: &mut [f32]) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }
        if let Some(state) = self.0.processor.lock().unwrap().as_mut() {
            state
                .capture_frame
                .process_capture(&mut state.processor, frame)?;
        }
        Ok(())
    }

    pub fn process_render_frame(&self, frame: &mut [f32]) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }
        if let Some(state) = self.0.processor.lock().unwrap().as_mut() {
            state
                .render_frame
                .process_render(&mut state.processor, frame)?;
        }
        Ok(())
    }

    pub fn set_capture_delay(&self, stream_delay: Duration) {
        self.set_delay(&self.0.capture_delay, stream_delay, "capture");
    }

    pub fn set_playback_delay(&self, stream_delay: Duration) {
        self.set_delay(&self.0.playback_delay, stream_delay, "playback");
    }

    fn set_delay(&self, delay: &AtomicU64, stream_delay: Duration, name: &str) {
        let new_value = stream_delay.as_millis().try_into().unwrap_or(u64::MAX);
        if let Ok(old_value) = delay.try_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
            (new_value.abs_diff(value) > 1).then_some(new_value)
        }) {
            debug!("changing {name} delay from {old_value} to {new_value}");
            self.update_stream_delay();
        }
    }

    fn total_stream_delay_ms(&self) -> u64 {
        self.0
            .playback_delay
            .load(Ordering::Relaxed)
            .saturating_add(self.0.capture_delay.load(Ordering::Relaxed))
            .min(MAX_STREAM_DELAY_MS)
    }

    fn update_stream_delay(&self) {
        let total = self.total_stream_delay_ms();
        if let Some(state) = self.0.processor.lock().unwrap().as_mut() {
            state
                .processor
                .set_stream_delay_ms(total as i32)
                .expect("clamped audio processor delay must be valid");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializes_distinct_capture_and_render_channel_counts() -> Result<()> {
        let processor = WebrtcAudioProcessor::new(true)?;
        processor.init_capture(1)?;
        processor.init_playback(2)?;

        processor.process_render_frame(&mut vec![0.0; 960])?;
        processor.process_capture_frame(&mut vec![0.0; 480])?;
        Ok(())
    }

    #[test]
    fn disabled_processor_is_a_bit_exact_bypass() -> Result<()> {
        let processor = WebrtcAudioProcessor::new(false)?;
        processor.init_capture(2)?;
        processor.init_playback(2)?;
        let mut frame = vec![0.25; 960];
        let expected = frame.clone();

        processor.process_capture_frame(&mut frame)?;

        assert_eq!(frame, expected);
        Ok(())
    }

    #[test]
    fn combined_delay_is_clamped_to_sonoras_supported_range() -> Result<()> {
        let processor = WebrtcAudioProcessor::new(true)?;
        processor.init_capture(2)?;
        processor.init_playback(2)?;
        processor.set_capture_delay(Duration::from_millis(400));
        processor.set_playback_delay(Duration::from_millis(300));

        let guard = processor.0.processor.lock().unwrap();
        assert_eq!(guard.as_ref().unwrap().processor.stream_delay_ms(), 500);
        Ok(())
    }
}
