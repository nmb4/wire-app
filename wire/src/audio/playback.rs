use std::{
    num::NonZeroUsize,
    ops::ControlFlow,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Result};
use cpal::{
    traits::{DeviceTrait, StreamTrait},
    Device, Sample, SampleFormat,
};
use fixed_resample::FixedResampler;
use ringbuf::{
    traits::{Consumer as _, Observer as _, Producer as _, Split},
    HeapCons as Consumer, HeapProd as Producer,
};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, info, trace, trace_span, warn, Level};

use super::{
    device::{find_device, find_output_stream_config, Direction, StreamConfigWithFormat},
    device_resampler, AudioFormat, WebrtcAudioProcessor, DURATION_10MS, DURATION_20MS,
    ENGINE_FORMAT, SAMPLE_RATE,
};
use crate::{
    codec::opus::MediaTrackOpusDecoder,
    rtc::{MediaFrame, MediaTrack},
};

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
}

impl AudioSource for GainSource {
    fn tick(&mut self, buf: &mut [f32]) -> Result<ControlFlow<(), usize>> {
        let result = self.inner.tick(buf)?;
        if let ControlFlow::Continue(count) = &result {
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
    #[cfg(all(feature = "audio-processing", target_os = "macos"))]
    processor.init_playback(ENGINE_FORMAT.channel_count as usize)?;
    #[cfg(all(feature = "audio-processing", not(target_os = "macos")))]
    processor.init_playback(config.channels as usize)?;
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

fn build_playback_stream<S: dasp_sample::FromSample<f32> + cpal::SizedSample + Default>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut state: PlaybackState,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    #[cfg(target_os = "macos")]
    let frame_size = ENGINE_FORMAT.sample_count(DURATION_10MS);
    #[cfg(not(target_os = "macos"))]
    let frame_size = state.format.sample_count(DURATION_10MS);
    let mut unprocessed: Vec<f32> = Vec::with_capacity(frame_size);
    let mut processed: Vec<f32> = Vec::with_capacity(frame_size);
    #[cfg(target_os = "macos")]
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

            // CoreAudio may invoke this callback with a buffer smaller than the
            // playback ring's latency cushion. Pulling the entire ring moves that
            // cushion into `resampled`, where the producer can no longer see it
            // and refills it on every callback. Only pull enough engine audio for
            // this callback on macOS, keeping the latency bounded in the ring.
            #[cfg(target_os = "macos")]
            {
                let missing_output = data.len().saturating_sub(resampled.len());
                let required_input = required_engine_samples(missing_output, state.format)
                    .saturating_sub(unprocessed.len());
                unprocessed.extend(state.consumer.pop_iter().take(required_input));
            }
            #[cfg(not(target_os = "macos"))]
            unprocessed.extend(state.consumer.pop_iter());

            // process
            let mut chunks = unprocessed.chunks_exact_mut(frame_size);
            for chunk in &mut chunks {
                #[cfg(feature = "audio-processing")]
                state.processor.process_render_frame(chunk).unwrap();
                processed.extend_from_slice(chunk);
            }
            // cleanup
            let remainder_len = chunks.into_remainder().len();
            let end = unprocessed.len() - remainder_len;
            unprocessed.copy_within(end.., 0);
            unprocessed.truncate(remainder_len);

            // The mixer ring uses the 48 kHz stereo engine format. Adapt its
            // channels to the selected macOS device before resampling; a mono
            // Bluetooth output must not interpret interleaved stereo samples as
            // twice as many mono frames.
            #[cfg(target_os = "macos")]
            {
                match state.format.channel_count {
                    1 => resample_input.extend(
                        processed
                            .chunks_exact(2)
                            .map(|frame| (frame[0] + frame[1]) * 0.5),
                    ),
                    2 => resample_input.extend_from_slice(&processed),
                    _ => unreachable!("audio device channel count validated at startup"),
                }
                state.resampler.process_interleaved(
                    &resample_input,
                    |samples| {
                        resampled.extend_from_slice(samples);
                    },
                    None,
                    false,
                );
                resample_input.clear();
            }
            #[cfg(not(target_os = "macos"))]
            state.resampler.process_interleaved(&processed, |samples|{
                resampled.extend_from_slice(samples);
            } , None, false);
            processed.clear();


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

#[cfg(target_os = "macos")]
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

#[cfg(all(test, target_os = "macos"))]
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
}
