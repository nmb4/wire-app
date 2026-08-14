//! Capture the computer's playback audio for an outgoing screen share.
//!
//! Windows uses WASAPI process loopback so Wire's own voice playback and UI
//! sounds stay out of the shared mix. Other platforms look for a loopback or
//! monitor input device. The result is encoded as a standalone Opus track so
//! mute still only silences the microphone.

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};
use wire::audio::{AudioQuality, AudioSink, ENGINE_FORMAT};
use wire::codec::opus::MediaTrackOpusEncoder;
use wire::rtc::MediaTrack;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(4);
const CONTENT_QUALITY: AudioQuality = AudioQuality::Ultra;

pub struct SystemAudioShare {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    track: MediaTrack,
}

impl SystemAudioShare {
    pub fn start() -> Result<Self> {
        let (encoder, track) =
            MediaTrackOpusEncoder::new_for_content(16, ENGINE_FORMAT, CONTENT_QUALITY)
                .context("failed to create system audio encoder")?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let (ready_tx, ready_rx) = mpsc::channel();

        let thread = std::thread::Builder::new()
            .name("wire-system-audio".into())
            .spawn(
                move || match run_capture_loop(encoder, stop_for_thread, ready_tx) {
                    Ok(()) => {}
                    Err(error) => warn!("system audio capture ended: {error:#}"),
                },
            )
            .context("failed to start system audio thread")?;

        match ready_rx.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(description)) => {
                info!("system audio capture started ({description})");
                Ok(Self {
                    stop,
                    thread: Some(thread),
                    track,
                })
            }
            Ok(Err(error)) => {
                stop.store(true, Ordering::Relaxed);
                let _ = thread.join();
                Err(error)
            }
            Err(RecvTimeoutError::Timeout) => {
                stop.store(true, Ordering::Relaxed);
                let _ = thread.join();
                Err(anyhow!("system audio capture did not start in time"))
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                Err(anyhow!("system audio capture thread exited during startup"))
            }
        }
    }

    pub fn track(&self) -> MediaTrack {
        self.track.clone()
    }
}

impl Drop for SystemAudioShare {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_capture_loop(
    encoder: MediaTrackOpusEncoder,
    stop: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<String>>,
) -> Result<()> {
    #[cfg(windows)]
    {
        return windows::run(encoder, stop, ready_tx);
    }
    #[cfg(not(windows))]
    {
        return fallback::run(encoder, stop, ready_tx);
    }
}

fn push_samples(encoder: &mut MediaTrackOpusEncoder, samples: &[f32]) -> Result<bool> {
    match encoder.tick(samples)? {
        ControlFlow::Continue(()) => Ok(true),
        ControlFlow::Break(()) => Ok(false),
    }
}

#[cfg(not(windows))]
fn stereoize_f32(input: &[f32], channels: u16, output: &mut Vec<f32>) {
    output.clear();
    match channels {
        1 => {
            output.reserve(input.len() * 2);
            for sample in input {
                output.extend_from_slice(&[*sample, *sample]);
            }
        }
        2 => output.extend_from_slice(input),
        channels => {
            let frames = input.len() / channels as usize;
            output.reserve(frames * 2);
            for frame in input.chunks(channels as usize) {
                output.extend_from_slice(&[frame[0], frame.get(1).copied().unwrap_or(frame[0])]);
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::mem::{size_of, ManuallyDrop};
    use std::ops::Deref;
    use std::pin::Pin;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::Duration;

    use anyhow::{anyhow, Context, Result};
    use tracing::{info, warn};
    use windows::core::{IUnknown, Interface, Ref};
    use windows::Win32::Media::Audio::{
        eConsole, eRender, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
        IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
        IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
        AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
        AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
        PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        WAVEFORMATEX,
    };
    use windows::Win32::System::Com::StructuredStorage::{
        PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Variant::VT_BLOB;
    use windows_core::implement;
    use wire::codec::opus::MediaTrackOpusEncoder;

    use super::push_samples;

    const WAVE_FORMAT_PCM: u16 = 1;
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    const BUFFER_HNS: i64 = 200_000;
    const POLL_INTERVAL: Duration = Duration::from_millis(5);
    const AUDCLNT_E_INVALID_STREAM_FLAG: u32 = 0x8889_0021;

    struct OpenedCapture {
        client: IAudioClient,
        capture: IAudioCaptureClient,
        description: String,
        channels: u16,
        is_float: bool,
        bits: u16,
    }

    pub fn run(
        mut encoder: MediaTrackOpusEncoder,
        stop: Arc<AtomicBool>,
        ready_tx: Sender<Result<String>>,
    ) -> Result<()> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().ok();
        }

        let opened = match open_capture() {
            Ok(opened) => opened,
            Err(error) => {
                let _ = ready_tx.send(Err(anyhow!("{error:#}")));
                return Err(error);
            }
        };
        if let Err(error) = unsafe { opened.client.Start() } {
            let error = anyhow!(error).context("failed to start system audio capture");
            let _ = ready_tx.send(Err(anyhow!("{error:#}")));
            return Err(error);
        }
        let _ = ready_tx.send(Ok(opened.description.clone()));

        let mut interleaved = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            if !drain_capture(&opened, &mut encoder, &mut interleaved)? {
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }

        let _ = unsafe { opened.client.Stop() };
        Ok(())
    }

    fn open_capture() -> Result<OpenedCapture> {
        let float = wave_format(WAVE_FORMAT_IEEE_FLOAT, 2, 48_000, 32);
        let pcm16 = wave_format(WAVE_FORMAT_PCM, 2, 48_000, 16);

        match try_process_loopback(&float, 0, 32, true, "WASAPI process loopback") {
            Ok(opened) => return Ok(opened),
            Err(error) => warn!("process loopback float init failed: {error:#}"),
        }
        match try_process_loopback(&pcm16, 0, 16, false, "WASAPI process loopback (pcm16)") {
            Ok(opened) => return Ok(opened),
            Err(error) => warn!("process loopback pcm16 init failed: {error:#}"),
        }

        let autoconvert = AUDCLNT_STREAMFLAGS_LOOPBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        match try_device_loopback(&float, autoconvert, 32, true, "WASAPI device loopback") {
            Ok(opened) => return Ok(opened),
            Err(error) => warn!("device loopback autoconvert init failed: {error:#}"),
        }
        try_device_loopback(
            &float,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            32,
            true,
            "WASAPI device loopback (native)",
        )
        .or_else(|error| {
            try_device_loopback(
                &pcm16,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                16,
                false,
                "WASAPI device loopback (pcm16)",
            )
            .map_err(|pcm_error| {
                anyhow!("could not capture system audio ({error:#}; {pcm_error:#})")
            })
        })
    }

    fn try_process_loopback(
        format: &WAVEFORMATEX,
        flags: u32,
        bits: u16,
        is_float: bool,
        description: &str,
    ) -> Result<OpenedCapture> {
        let client = activate_process_loopback()?;
        initialize_client(&client, flags, format)
            .with_context(|| format!("{description} initialize"))?;
        finish_open(client, format.nChannels, bits, is_float, description)
    }

    fn try_device_loopback(
        format: &WAVEFORMATEX,
        flags: u32,
        bits: u16,
        is_float: bool,
        description: &str,
    ) -> Result<OpenedCapture> {
        let client = activate_device_loopback()?;
        initialize_client(&client, flags, format)
            .with_context(|| format!("{description} initialize"))?;
        finish_open(client, format.nChannels, bits, is_float, description)
    }

    fn initialize_client(client: &IAudioClient, flags: u32, format: &WAVEFORMATEX) -> Result<()> {
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_HNS, 0, format, None) }
            .map_err(|error| {
                if error.code().0 as u32 == AUDCLNT_E_INVALID_STREAM_FLAG {
                    anyhow!("{error} (AUDCLNT_E_INVALID_STREAM_FLAG; flags={flags:#x})")
                } else {
                    anyhow!("{error}")
                }
            })
    }

    fn finish_open(
        client: IAudioClient,
        channels: u16,
        bits: u16,
        is_float: bool,
        description: &str,
    ) -> Result<OpenedCapture> {
        let capture: IAudioCaptureClient =
            unsafe { client.GetService() }.context("failed to open system audio capture client")?;
        info!(
            "{description} ready ({}ch, {}-bit {}, 48 kHz)",
            channels,
            bits,
            if is_float { "float" } else { "pcm" }
        );
        Ok(OpenedCapture {
            client,
            capture,
            description: description.to_owned(),
            channels,
            is_float,
            bits,
        })
    }

    fn drain_capture(
        opened: &OpenedCapture,
        encoder: &mut MediaTrackOpusEncoder,
        interleaved: &mut Vec<f32>,
    ) -> Result<bool> {
        loop {
            let mut data = ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            if unsafe {
                opened
                    .capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
            }
            .is_err()
            {
                return Ok(true);
            }

            if frames == 0 || data.is_null() {
                let _ = unsafe { opened.capture.ReleaseBuffer(0) };
                return Ok(true);
            }

            interleaved.clear();
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                interleaved.resize(frames as usize * 2, 0.0);
            } else {
                copy_to_stereo_f32(
                    data,
                    frames as usize,
                    opened.channels,
                    opened.is_float,
                    opened.bits,
                    interleaved,
                );
            }
            unsafe { opened.capture.ReleaseBuffer(frames) }
                .context("system audio ReleaseBuffer failed")?;

            if !push_samples(encoder, interleaved)? {
                return Ok(false);
            }
        }
    }

    fn copy_to_stereo_f32(
        data: *mut u8,
        frames: usize,
        channels: u16,
        is_float: bool,
        bits: u16,
        output: &mut Vec<f32>,
    ) {
        let channels = channels.max(1) as usize;
        output.reserve(frames * 2);
        if is_float && bits == 32 {
            let raw = unsafe { std::slice::from_raw_parts(data as *const f32, frames * channels) };
            for frame in raw.chunks(channels) {
                let left = frame[0];
                let right = frame.get(1).copied().unwrap_or(left);
                output.extend_from_slice(&[left, right]);
            }
            return;
        }
        if bits == 16 {
            let raw = unsafe { std::slice::from_raw_parts(data as *const i16, frames * channels) };
            for frame in raw.chunks(channels) {
                let left = frame[0] as f32 / i16::MAX as f32;
                let right = frame.get(1).copied().unwrap_or(frame[0]) as f32 / i16::MAX as f32;
                output.extend_from_slice(&[left, right]);
            }
        }
    }

    fn wave_format(tag: u16, channels: u16, rate: u32, bits: u16) -> WAVEFORMATEX {
        let block_align = channels * (bits / 8);
        WAVEFORMATEX {
            wFormatTag: tag,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * u32::from(block_align),
            nBlockAlign: block_align,
            wBitsPerSample: bits,
            cbSize: 0,
        }
    }

    fn activate_device_loopback() -> Result<IAudioClient> {
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .context("failed to create audio device enumerator")?;
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .context("failed to open the default playback device")?;
        unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }
            .context("failed to activate WASAPI loopback on the playback device")
    }

    fn activate_process_loopback() -> Result<IAudioClient> {
        unsafe {
            let mut activation_params = AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                    ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                        TargetProcessId: std::process::id(),
                        ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                    },
                },
            };
            let pinned_params = Pin::new(&mut activation_params);
            let raw_prop = PROPVARIANT {
                Anonymous: PROPVARIANT_0 {
                    Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                        vt: VT_BLOB,
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: PROPVARIANT_0_0_0 {
                            blob: BLOB {
                                cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                                pBlobData: ptr::from_mut(pinned_params.get_mut()).cast(),
                            },
                        },
                    }),
                },
            };
            let activation_prop = ManuallyDrop::new(raw_prop);
            let pinned_prop = Pin::new(activation_prop.deref());
            let activation_params = Some(ptr::from_ref(pinned_prop.get_ref()));

            let ready = Arc::new((Mutex::new(false), Condvar::new()));
            let callback: IActivateAudioInterfaceCompletionHandler =
                ActivationHandler::new(ready.clone()).into();
            let operation = ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                activation_params,
                &callback,
            )
            .context("ActivateAudioInterfaceAsync failed")?;

            let (lock, signal) = &*ready;
            let mut completed = lock.lock().expect("system audio activation lock");
            while !*completed {
                completed = signal
                    .wait_timeout(completed, Duration::from_secs(3))
                    .expect("system audio activation wait")
                    .0;
                if !*completed {
                    anyhow::bail!("process loopback activation timed out");
                }
            }
            drop(completed);

            let mut result = windows::core::HRESULT(0);
            let mut unknown: Option<IUnknown> = None;
            operation
                .GetActivateResult(&mut result, &mut unknown)
                .context("GetActivateResult failed")?;
            result.ok().context("process loopback activation failed")?;
            unknown
                .context("process loopback returned no audio client")?
                .cast::<IAudioClient>()
                .context("process loopback did not yield IAudioClient")
        }
    }

    #[implement(IActivateAudioInterfaceCompletionHandler)]
    struct ActivationHandler(Arc<(Mutex<bool>, Condvar)>);

    impl ActivationHandler {
        fn new(ready: Arc<(Mutex<bool>, Condvar)>) -> Self {
            Self(ready)
        }
    }

    impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
        fn ActivateCompleted(
            &self,
            _operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
        ) -> windows::core::Result<()> {
            let (lock, signal) = &*self.0;
            let mut completed = lock.lock().expect("system audio activation lock");
            *completed = true;
            signal.notify_one();
            Ok(())
        }
    }
}

#[cfg(not(windows))]
mod fallback {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use anyhow::{anyhow, Context, Result};
    use wire::codec::opus::MediaTrackOpusEncoder;
    use wire::cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use wire::cpal::{SampleFormat, SampleRate, StreamConfig};

    use super::{push_samples, stereoize_f32};

    pub fn run(
        mut encoder: MediaTrackOpusEncoder,
        stop: Arc<AtomicBool>,
        ready_tx: Sender<Result<String>>,
    ) -> Result<()> {
        let host = wire::cpal::default_host();
        let device = find_loopback_device(&host)
            .context("no system-audio loopback or monitor device is available")?;
        let name = device.name().unwrap_or_else(|_| "loopback".to_owned());
        let supported = device
            .default_input_config()
            .with_context(|| format!("failed to query input config for {name}"))?;
        let mut config: StreamConfig = supported.clone().into();
        config.sample_rate = SampleRate(48_000);
        let sample_format = supported.sample_format();
        let channels = config.channels;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(8);

        let stream = match sample_format {
            SampleFormat::F32 => build_stream::<f32>(&device, &config, tx)?,
            SampleFormat::I16 => build_stream::<i16>(&device, &config, tx)?,
            SampleFormat::I32 => build_stream::<i32>(&device, &config, tx)?,
            other => {
                let error = anyhow!("unsupported loopback sample format {other}");
                let _ = ready_tx.send(Err(anyhow!("{error:#}")));
                return Err(error);
            }
        };
        stream.play().context("failed to start loopback stream")?;
        let _ = ready_tx.send(Ok(format!("loopback device {name}")));

        let mut stereo = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(samples) => {
                    stereoize_f32(&samples, channels, &mut stereo);
                    if !push_samples(&mut encoder, &stereo)? {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        drop(stream);
        thread::sleep(Duration::from_millis(1));
        Ok(())
    }

    fn find_loopback_device(host: &wire::cpal::Host) -> Result<wire::cpal::Device> {
        let mut devices = host
            .input_devices()
            .context("failed to list audio input devices")?;
        devices
            .find(|device| {
                device
                    .name()
                    .map(|name| looks_like_loopback(&name))
                    .unwrap_or(false)
            })
            .ok_or_else(|| anyhow!("no monitor or loopback input device found"))
    }

    fn looks_like_loopback(name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        name.contains("monitor")
            || name.contains("loopback")
            || name.contains("blackhole")
            || name.contains("soundflower")
            || name.contains("stereo mix")
            || name.contains("what u hear")
    }

    fn build_stream<S>(
        device: &wire::cpal::Device,
        config: &StreamConfig,
        tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    ) -> Result<wire::cpal::Stream>
    where
        S: wire::cpal::SizedSample + Copy,
        f32: FromSample<S>,
    {
        device
            .build_input_stream(
                config,
                move |data: &[S], _| {
                    let samples: Vec<f32> = data.iter().copied().map(f32::from_sample).collect();
                    let _ = tx.try_send(samples);
                },
                |error| tracing::warn!("system audio loopback stream error: {error}"),
                None,
            )
            .context("failed to build loopback input stream")
    }

    trait FromSample<S> {
        fn from_sample(sample: S) -> Self;
    }

    impl FromSample<f32> for f32 {
        fn from_sample(sample: f32) -> Self {
            sample
        }
    }

    impl FromSample<i16> for f32 {
        fn from_sample(sample: i16) -> Self {
            sample as f32 / i16::MAX as f32
        }
    }

    impl FromSample<i32> for f32 {
        fn from_sample(sample: i32) -> Self {
            sample as f32 / i32::MAX as f32
        }
    }
}
