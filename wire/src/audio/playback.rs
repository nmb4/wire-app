use std::{
    num::NonZeroUsize,
    ops::ControlFlow,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use cpal::{
    traits::{DeviceTrait, StreamTrait},
    Device, Sample, SampleFormat,
};
use fixed_resample::FixedResampler;
use ringbuf::{
    traits::{Consumer as _, Observer as _, Producer as _, Split},
    HeapCons as Consumer, HeapProd as Producer,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, trace, trace_span, warn, Level};

use super::{
    device::{find_device, find_output_stream_config, Direction, StreamConfigWithFormat},
    device_resampler, update_audio_level, AudioFormat, AudioLevelHandle, WebrtcAudioProcessor,
    DURATION_10MS, DURATION_20MS, ENGINE_FORMAT, SAMPLE_RATE,
};
use crate::{codec::opus::MediaTrackOpusDecoder, rtc::MediaTrack};

#[cfg(target_os = "macos")]
const PLAYBACK_PREBUFFER_CHUNKS: usize = 3;

pub trait AudioSource: Send + 'static {
    fn tick(&mut self, buf: &mut [f32]) -> Result<ControlFlow<(), usize>>;
}

/// Shared volume control for a playback source. 1.0 = normal.
pub type VolumeHandle = Arc<AtomicU32>;

pub struct GainSource {
    inner: Box<dyn AudioSource>,
    volume: VolumeHandle,
    level: Option<AudioLevelHandle>,
}

impl AudioSource for GainSource {
    fn tick(&mut self, buf: &mut [f32]) -> Result<ControlFlow<(), usize>> {
        let result = self.inner.tick(buf)?;
        if let ControlFlow::Continue(count) = &result {
            if let Some(level) = &self.level {
                update_audio_level(level, &buf[..*count]);
            }
            let gain = f32::from_bits(self.volume.load(Ordering::Relaxed));
            for sample in buf[..*count].iter_mut() {
                *sample *= gain;
            }
        }
        Ok(result)
    }
}

#[derive(derive_more::Debug, Clone)]
pub struct AudioPlayback {
    source_sender: mpsc::Sender<Box<dyn AudioSource>>,
    deafened: Arc<AtomicBool>,
}

impl AudioPlayback {
    pub async fn build(
        host: &cpal::Host,
        device: Option<&str>,
        processor: WebrtcAudioProcessor,
    ) -> Result<Self> {
        let device = find_device(host, Direction::Playback, device)?;
        let stream_config = find_output_stream_config(&device, &ENGINE_FORMAT)?;

        let buffer_size = ENGINE_FORMAT.sample_count(DURATION_20MS) * 32;
        #[allow(unused_mut)]
        let (mut producer, consumer) = ringbuf::HeapRb::<f32>::new(buffer_size).split();

        #[cfg(target_os = "macos")]
        {
            // Prime the device before stream.play() can invoke its first callback.
            // Three 20 ms chunks cover one CoreAudio callback plus scheduler jitter.
            let prebuffer =
                vec![0.0; ENGINE_FORMAT.sample_count(DURATION_20MS) * PLAYBACK_PREBUFFER_CHUNKS];
            let primed = producer.push_slice(&prebuffer);
            debug_assert_eq!(primed, prebuffer.len());
        }

        let (source_sender, source_receiver) = mpsc::channel(16);
        let deafened = Arc::new(AtomicBool::new(false));
        let deafened_for_thread = deafened.clone();
        let (init_tx, init_rx) = oneshot::channel();

        std::thread::spawn(move || {
            if let Err(err) = audio_thread_priority::promote_current_thread_to_real_time(
                buffer_size as u32,
                ENGINE_FORMAT.sample_rate.0,
            ) {
                #[cfg(target_os = "macos")]
                debug!("macOS kept the playback worker at normal priority: {err:?}");
                #[cfg(not(target_os = "macos"))]
                warn!("failed to set playback thread to realtime priority: {err:?}");
            }
            let stream = match start_playback_stream(&device, &stream_config, processor, consumer) {
                Ok(stream) => {
                    init_tx.send(Ok(())).unwrap();
                    stream
                }
                Err(err) => {
                    init_tx.send(Err(err)).unwrap();
                    return;
                }
            };
            playback_loop(producer, source_receiver, deafened_for_thread);
            drop(stream);
        });

        init_rx.await??;
        Ok(Self {
            source_sender,
            deafened,
        })
    }

    pub async fn add_track(&self, track: MediaTrack) -> Result<()> {
        let decoder = MediaTrackOpusDecoder::new(track)?;
        self.add_source(decoder).await
    }

    pub async fn add_track_with_volume(
        &self,
        track: MediaTrack,
        volume: VolumeHandle,
    ) -> Result<()> {
        let decoder = MediaTrackOpusDecoder::new(track)?;
        self.add_source(GainSource {
            inner: Box::new(decoder),
            volume,
            level: None,
        })
        .await
    }

    pub async fn add_track_with_volume_and_level(
        &self,
        track: MediaTrack,
        volume: VolumeHandle,
        level: AudioLevelHandle,
    ) -> Result<()> {
        let decoder = MediaTrackOpusDecoder::new(track)?;
        self.add_source(GainSource {
            inner: Box::new(decoder),
            volume,
            level: Some(level),
        })
        .await
    }

    pub async fn add_source(&self, source: impl AudioSource) -> Result<()> {
        self.source_sender
            .send(Box::new(source))
            .await
            .map_err(|_| anyhow!("failed to add audio source: playback loop dead"))?;
        Ok(())
    }

    pub fn set_deafened(&self, deafened: bool) {
        self.deafened.store(deafened, Ordering::Relaxed);
    }
}

#[cfg(target_os = "macos")]
fn playback_loop(
    mut producer: Producer<f32>,
    mut source_receiver: mpsc::Receiver<Box<dyn AudioSource>>,
    deafened: Arc<AtomicBool>,
) {
    let span = tracing::span!(Level::TRACE, "playback-loop");
    let _guard = span.enter();
    info!("playback loop start");

    let tick_duration = DURATION_20MS;
    let buffer_size = ENGINE_FORMAT.sample_count(tick_duration);
    let mut work_buf = vec![0.; buffer_size];
    let mut out_buf = vec![0.; buffer_size];
    let mut sources: Vec<Box<dyn AudioSource>> = vec![];

    let mut tick = 0;
    loop {
        // pull incoming sources
        loop {
            match source_receiver.try_recv() {
                Ok(source) => {
                    info!("add new track to decoder");
                    sources.push(source);
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    info!("stop playback mixer loop: channel closed");
                    return;
                }
            }
        }

        // Refill based on what CoreAudio actually consumed. This keeps a small
        // stable cushion without relying on a second clock that can drift.
        let target_samples = buffer_size * PLAYBACK_PREBUFFER_CHUNKS;
        let missing_frames = target_samples
            .saturating_sub(producer.occupied_len())
            .div_ceil(buffer_size);
        if missing_frames == 0 {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }

        for _ in 0..missing_frames {
            let start = Instant::now();
            out_buf.fill(0.0);
            sources.retain_mut(|source| match source.tick(&mut work_buf) {
                Ok(ControlFlow::Continue(count)) => {
                    for i in 0..count {
                        out_buf[i] += work_buf[i];
                    }
                    if count < work_buf.len() {
                        debug!(
                            "audio source xrun: missing {} of {}",
                            work_buf.len() - count,
                            work_buf.len()
                        );
                    }
                    true
                }
                Ok(ControlFlow::Break(())) => {
                    debug!("remove decoder: closed");
                    false
                }
                Err(err) => {
                    warn!("remove decoder: failed {err:?}");
                    false
                }
            });

            if deafened.load(Ordering::Relaxed) {
                out_buf.fill(0.0);
            }

            let len = producer.push_slice(&out_buf);
            if len < out_buf.len() {
                warn!(
                    "playback xrun: failed to queue {} of {} samples",
                    out_buf.len() - len,
                    out_buf.len()
                );
            }

            trace!("tick {tick} took {:?} pushed {len}", start.elapsed());
            if start.elapsed() > tick_duration {
                warn!(
                    "playback thread tick exceeded interval (took {:?})",
                    start.elapsed()
                );
            }
            tick += 1;
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn playback_loop(
    mut producer: Producer<f32>,
    mut source_receiver: mpsc::Receiver<Box<dyn AudioSource>>,
    deafened: Arc<AtomicBool>,
) {
    let span = tracing::span!(Level::TRACE, "playback-loop");
    let _guard = span.enter();
    info!("playback loop start");

    let tick_duration = DURATION_20MS;
    let buffer_size = ENGINE_FORMAT.sample_count(tick_duration);
    let mut work_buf = vec![0.; buffer_size];
    let mut out_buf = vec![0.; buffer_size];
    let mut sources: Vec<Box<dyn AudioSource>> = vec![];

    let initial_silence = vec![0.; buffer_size];
    let n = producer.push_slice(&initial_silence);
    debug_assert_eq!(n, initial_silence.len());

    let mut tick = 0;
    loop {
        let start = Instant::now();

        loop {
            match source_receiver.try_recv() {
                Ok(source) => {
                    info!("add new track to decoder");
                    sources.push(source);
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    info!("stop playback mixer loop: channel closed");
                    return;
                }
            }
        }

        out_buf.fill(0.0);
        sources.retain_mut(|source| match source.tick(&mut work_buf) {
            Ok(ControlFlow::Continue(count)) => {
                for i in 0..count {
                    out_buf[i] += work_buf[i];
                }
                if count < work_buf.len() {
                    debug!(
                        "audio source xrun: missing {} of {}",
                        work_buf.len() - count,
                        work_buf.len()
                    );
                }
                true
            }
            Ok(ControlFlow::Break(())) => {
                debug!("remove decoder: closed");
                false
            }
            Err(err) => {
                warn!("remove decoder: failed {err:?}");
                false
            }
        });

        if deafened.load(Ordering::Relaxed) {
            out_buf.fill(0.0);
        }

        let len = producer.push_slice(&out_buf);
        if len < out_buf.len() {
            warn!(
                "xrun: failed to push {} of {}",
                out_buf.len() - len,
                out_buf.len()
            );
        }

        trace!("tick {tick} took {:?} pushed {len}", start.elapsed());
        if start.elapsed() > tick_duration {
            warn!(
                "playback thread tick exceeded interval (took {:?})",
                start.elapsed()
            );
        } else {
            spin_sleep::sleep(tick_duration.saturating_sub(start.elapsed()));
        }
        tick += 1;
    }
}

fn start_playback_stream(
    device: &Device,
    stream_config: &StreamConfigWithFormat,
    processor: WebrtcAudioProcessor,
    consumer: Consumer<f32>,
) -> Result<cpal::Stream> {
    let config = &stream_config.config;
    let format = stream_config.audio_format();
    validate_output_format(format)?;
    #[cfg(feature = "audio-processing")]
    processor.init_playback(ENGINE_FORMAT.channel_count as usize)?;
    let resampler = device_resampler(
        NonZeroUsize::new(format.channel_count as usize).unwrap(),
        SAMPLE_RATE.0,
        format.sample_rate.0,
    );
    let state = PlaybackState {
        consumer,
        format,
        processor,
        resampler,
    };
    let stream = match stream_config.sample_format {
        SampleFormat::I8 => build_playback_stream::<i8>(device, config, state),
        SampleFormat::I16 => build_playback_stream::<i16>(device, config, state),
        SampleFormat::I32 => build_playback_stream::<i32>(device, config, state),
        SampleFormat::F32 => build_playback_stream::<f32>(device, config, state),
        sample_format => {
            tracing::error!("Unsupported sample format '{sample_format}'");
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        }
    }?;
    info!(
        "start playback stream on {} with {format:?}",
        device.name()?
    );
    stream.play()?;
    Ok(stream)
}

struct PlaybackState {
    format: AudioFormat,
    resampler: FixedResampler<f32, 2>,
    #[allow(unused)]
    processor: WebrtcAudioProcessor,
    consumer: Consumer<f32>,
}

fn validate_output_format(format: AudioFormat) -> Result<()> {
    anyhow::ensure!(
        matches!(format.channel_count, 1 | 2) && format.sample_rate.0 > 0,
        "audio playback requires a mono or stereo device with a nonzero sample rate, got {format:?}"
    );
    Ok(())
}

impl PlaybackState {
    fn prepare_output(
        &mut self,
        output_samples: usize,
        unprocessed: &mut Vec<f32>,
        device_input: &mut Vec<f32>,
        resampled: &mut Vec<f32>,
    ) -> Result<()> {
        while resampled.len() < output_samples {
            let missing_output = output_samples - resampled.len();
            let required_input = required_engine_samples(missing_output, self.format)
                .saturating_sub(unprocessed.len());
            let before = unprocessed.len();
            unprocessed.extend(self.consumer.pop_iter().take(required_input));
            if unprocessed.len() == before {
                break;
            }
            if let Err(error) = self.process_audio(unprocessed, device_input, resampled) {
                // Do not replay partially processed chunks on the next callback.
                unprocessed.clear();
                device_input.clear();
                resampled.clear();
                self.resampler.reset();
                return Err(error);
            }
            // Fixed-block resamplers may need another engine chunk before
            // producing output. Pull only until this callback is full.
        }
        Ok(())
    }

    /// The mixer and echo canceller always use 48 kHz stereo. Only adapt to
    /// the output device after processing complete engine frames.
    fn process_audio(
        &mut self,
        unprocessed: &mut Vec<f32>,
        device_input: &mut Vec<f32>,
        resampled: &mut Vec<f32>,
    ) -> Result<()> {
        let frame_size = ENGINE_FORMAT.sample_count(DURATION_10MS);
        let mut chunks = unprocessed.chunks_exact_mut(frame_size);
        for chunk in &mut chunks {
            #[cfg(feature = "audio-processing")]
            self.processor.process_render_frame(chunk)?;
            match self.format.channel_count {
                1 => device_input.extend(
                    chunk
                        .chunks_exact(2)
                        .map(|frame| (frame[0] + frame[1]) * 0.5),
                ),
                2 => device_input.extend_from_slice(chunk),
                _ => anyhow::bail!("audio playback supports one or two device channels"),
            }
        }
        let remainder_len = chunks.into_remainder().len();
        let end = unprocessed.len() - remainder_len;
        unprocessed.copy_within(end.., 0);
        unprocessed.truncate(remainder_len);

        self.resampler.process_interleaved(
            device_input,
            |samples| resampled.extend_from_slice(samples),
            None,
            false,
        );
        device_input.clear();
        Ok(())
    }
}

fn build_playback_stream<S: dasp_sample::FromSample<f32> + cpal::SizedSample + Default>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut state: PlaybackState,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let frame_size = ENGINE_FORMAT.sample_count(DURATION_10MS);
    let mut unprocessed: Vec<f32> = Vec::with_capacity(frame_size);
    let mut resample_input: Vec<f32> = Vec::with_capacity(frame_size);
    let mut resampled: Vec<f32> = Vec::with_capacity(frame_size);
    let mut tick = 0;
    let mut last_warning = Instant::now();
    let mut underflows = 0;
    let span = trace_span!("playback-cb");

    device.build_output_stream::<S, _, _>(
        config,
        move |data: &mut [S], info: &_| {
            let _guard = span.enter();
            let delay = {
                let output_delay = info
                    .timestamp()
                    .callback
                    .duration_since(&info.timestamp().playback)
                    .unwrap_or_default();
                let resampler_delay = Duration::from_secs_f32(state.resampler.output_delay() as f32 / state.format.sample_rate.0 as f32);
                output_delay + resampler_delay
            };

            if tick % 100 == 0 {
                trace!(
                    "callback tick {tick} len={} delay={delay:?} resampled={} ring={}",
                    data.len(),
                    resampled.len(),
                    state.consumer.occupied_len()
                );
            }


            #[cfg(feature = "audio-processing")]
            state.processor.set_playback_delay(delay);

            // Leave the latency cushion in the ring, visible to the producer.
            // This also avoids processing a whole backlog after a delayed callback.
            if let Err(error) = state.prepare_output(data.len(), &mut unprocessed, &mut resample_input, &mut resampled) {
                data.fill(S::default());
                let now = Instant::now();
                if now.duration_since(last_warning) > Duration::from_secs(1) {
                    warn!("playback processing failed: {error:#}");
                    last_warning = now;
                }
                tick += 1;
                return;
            }

            // copy to out
            let out_len = resampled.len().min(data.len());
            let remaining = resampled.len() - out_len;
            for (i, sample) in data[..out_len].iter_mut().enumerate() {
                *sample = resampled[i].to_sample()
            }
            data[out_len..].fill(S::default());
            resampled.copy_within(out_len.., 0);
            resampled.truncate(remaining);

            // trace!("out_len {out_len} resampled_remaining {} processed_remaining {}", resampled.len(), processed.len());
            if out_len < data.len() {
                let now = Instant::now();
                if now.duration_since(last_warning) > Duration::from_secs(1) {
                    warn!(
                        "[tick {tick}] playback xrun: {} of {} samples missing (buffered {}) (+ {} previous)",
                        data.len() - out_len,
                        data.len(),
                        unprocessed.len() + state.consumer.occupied_len(),
                        underflows
                    );
                    underflows += 1;
                    last_warning = now;
                }
            }
            tick += 1;
        },
        |err| {
            error!("an error occurred on output stream: {}", err);
        },
        None,
    )
}

fn required_engine_samples(output_samples: usize, output_format: AudioFormat) -> usize {
    if output_samples == 0 {
        return 0;
    }

    let output_channels = output_format.channel_count as usize;
    let output_frames = output_samples.div_ceil(output_channels);
    let engine_frames =
        (output_frames * SAMPLE_RATE.0 as usize).div_ceil(output_format.sample_rate.0 as usize);
    let engine_samples = engine_frames * ENGINE_FORMAT.channel_count as usize;
    let processor_frame_size = ENGINE_FORMAT.sample_count(DURATION_10MS);
    engine_samples.div_ceil(processor_frame_size) * processor_frame_size
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_bluetooth_callback_to_one_engine_chunk() {
        let bluetooth_format = AudioFormat::new2(16_000, 1);
        assert_eq!(
            required_engine_samples(320, bluetooth_format),
            ENGINE_FORMAT.sample_count(DURATION_20MS)
        );
    }

    #[test]
    fn unsupported_output_formats_are_errors_before_constructing_the_resampler() {
        for format in [
            AudioFormat::new2(48_000, 0),
            AudioFormat::new2(48_000, 6),
            AudioFormat::new2(0, 2),
        ] {
            assert!(validate_output_format(format).is_err());
        }
    }

    #[test]
    #[cfg(feature = "audio-processing")]
    fn processing_error_discards_partial_audio_and_next_callback_recovers() {
        let mut state = state_for_format(ENGINE_FORMAT, true);
        let (mut producer, consumer) = ringbuf::HeapRb::<f32>::new(3_840).split();
        state.consumer = consumer;
        // Force a processor format error without needing a native audio device.
        state.processor.init_playback(1).unwrap();
        producer.push_slice(&[0.25; 960]);
        let mut pending = Vec::new();
        let mut input = Vec::new();
        let mut output = Vec::new();
        assert!(state
            .prepare_output(960, &mut pending, &mut input, &mut output)
            .is_err());
        assert!(pending.is_empty() && input.is_empty() && output.is_empty());
        state.processor.init_playback(2).unwrap();
        producer.push_slice(&[0.5; 2_880]);
        state
            .prepare_output(960, &mut pending, &mut input, &mut output)
            .unwrap();
        assert!(output.len() >= 960);
        // The FFT resampler has a startup transient after reset.
        assert!(output.iter().any(|sample| sample.abs() > 0.4));
        assert!(output
            .iter()
            .rev()
            .take(128)
            .all(|sample| (*sample - 0.5).abs() < 0.01));
    }

    #[test]
    fn callbacks_leave_backlog_in_the_ring_and_keep_output_continuous() {
        for format in [
            AudioFormat::new2(16_000, 1),
            AudioFormat::new2(44_100, 1),
            AudioFormat::new2(44_100, 2),
            ENGINE_FORMAT,
        ] {
            let mut state = state_for_format(format, true);
            let (mut producer, consumer) = ringbuf::HeapRb::<f32>::new(61_440).split();
            state.consumer = consumer;
            assert_eq!(producer.push_slice(&vec![0.25; 30_720]), 30_720);
            let mut pending = Vec::new();
            let mut input = Vec::new();
            let mut output = Vec::new();
            let callback_samples = format.sample_count(DURATION_10MS);
            for tick in 0..200 {
                state
                    .prepare_output(callback_samples, &mut pending, &mut input, &mut output)
                    .unwrap();
                if tick == 0 {
                    assert!(
                        state.consumer.occupied_len() >= 25_920,
                        "drained the latency cushion"
                    );
                }
                if tick > 10 {
                    assert!(
                        output.len() >= callback_samples,
                        "{format:?}: callback {tick} underflow"
                    );
                }
                assert!(
                    output.len() <= format.sample_count(Duration::from_millis(50)),
                    "moved backlog outside the ring"
                );
                let consumed = output.len().min(callback_samples);
                output.drain(..consumed);
                producer.push_slice(&[0.25; 960]);
                assert!(pending.len() < 960);
            }
        }
    }

    fn state_for_format(format: AudioFormat, processing: bool) -> PlaybackState {
        #[cfg(feature = "audio-processing")]
        let processor = {
            let processor = WebrtcAudioProcessor::new(processing).unwrap();
            processor.init_capture(2).unwrap();
            processor.init_playback(2).unwrap();
            processor
        };
        #[cfg(not(feature = "audio-processing"))]
        let processor = {
            let _ = processing;
            WebrtcAudioProcessor
        };
        let (_, consumer) = ringbuf::HeapRb::<f32>::new(1_920).split();
        PlaybackState {
            format,
            processor,
            consumer,
            resampler: device_resampler(
                NonZeroUsize::new(format.channel_count as usize).unwrap(),
                SAMPLE_RATE.0,
                format.sample_rate.0,
            ),
        }
    }

    #[test]
    fn playback_processes_engine_frames_for_mono_and_non_48khz_devices() {
        for format in [
            AudioFormat::new2(16_000, 1),
            AudioFormat::new2(44_100, 1),
            AudioFormat::new2(44_100, 2),
            ENGINE_FORMAT,
        ] {
            let mut state = state_for_format(format, true);
            let mut pending = Vec::new();
            let mut device_input = Vec::new();
            let mut output = Vec::new();
            for _ in 0..100 {
                pending.extend_from_slice(&[0.25; 960]);
                state
                    .process_audio(&mut pending, &mut device_input, &mut output)
                    .unwrap();
                assert!(pending.is_empty());
            }
            let expected = format.sample_count(Duration::from_secs(1));
            // The fixed resampler can retain one input block at the end.
            assert!(
                output.len() <= expected,
                "{format:?}: {} samples",
                output.len()
            );
            assert!(
                output.len() >= expected * 9 / 10,
                "{format:?}: {} samples",
                output.len()
            );
            assert!(
                output.iter().any(|sample| sample.abs() > 0.1),
                "{format:?}: silent output"
            );
        }
    }

    #[test]
    fn mono_downmix_preserves_timing_and_partial_engine_frames() {
        let mut state = state_for_format(AudioFormat::new2(48_000, 1), true);
        let mut pending = vec![0.25, 0.75].repeat(240);
        let mut device_input = Vec::new();
        let mut output = Vec::new();
        state
            .process_audio(&mut pending, &mut device_input, &mut output)
            .unwrap();
        assert!(output.is_empty());
        assert_eq!(pending.len(), 480);
        pending.extend_from_slice(&vec![0.25, 0.75].repeat(240));
        state
            .process_audio(&mut pending, &mut device_input, &mut output)
            .unwrap();
        assert!(pending.is_empty());
        // The device resampler can buffer an input block, even at equal rates.
        for _ in 0..40 {
            pending.extend_from_slice(&[0.25, 0.75].repeat(480));
            state
                .process_audio(&mut pending, &mut device_input, &mut output)
                .unwrap();
        }
        assert!((18_000..=19_680).contains(&output.len()));
        assert!(output
            .iter()
            .skip(1_024)
            .all(|sample| (*sample - 0.5).abs() < 1e-6));
    }
}
