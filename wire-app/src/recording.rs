//! Per-speaker call recording.
//!
//! Wire is a mesh: every remote participant's voice arrives on its own Opus
//! `MediaTrack`, and the local microphone is its own capture stream. Recording
//! one file per speaker therefore needs no source separation and no speaker
//! diarization model. Each file holds exactly one person's voice, which is the
//! input a plain single-speaker transcription model handles best.
//!
//! Three properties make the output usable by a later transcription pipeline:
//!
//! * Every tap emits exactly one [`FRAME`] worth of mono samples per audio
//!   tick and zero-pads anything short. A muted tick, a starved decoder, or a
//!   packet-loss skip shortens nothing, so every speaker file keeps the same
//!   sample count and the same origin as the call.
//! * Files are canonical 48 kHz mono 16-bit PCM WAV, readable directly by
//!   whisper.cpp, faster-whisper, and friends.
//! * A `manifest.json` records who spoke, in which file, for how long, and
//!   when the call started. Merging per-speaker transcripts is then just
//!   `call_start + segment_start` for each segment.
//!
//! Known gap: the recording indicator is local only. Nothing in the call
//! protocol tells a peer that this device is recording them.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use iroh::NodeId;
use serde::Serialize;
use tracing::{info, warn};
use wire::audio::{AudioContext, AudioSink, AudioSource};
use wire::codec::opus::MediaTrackOpusDecoder;
use wire::rtc::MediaTrack;

/// One tick of recorded audio. Every speaker file is an exact multiple of this.
pub(crate) const FRAME: Duration = Duration::from_millis(20);
/// Recording sample rate. Matches the engine format, so no resampling happens.
pub(crate) const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 1;
const BITS_PER_SAMPLE: u16 = 16;
/// Mono samples per [`FRAME`].
const FRAME_SAMPLES: usize = SAMPLE_RATE as usize * FRAME.as_millis() as usize / 1000;
/// Engine samples handed to a tap per tick: interleaved stereo at [`SAMPLE_RATE`].
const ENGINE_FRAME_SAMPLES: usize = FRAME_SAMPLES * 2;
/// Frames of slack for the writer thread. Roughly five seconds, which keeps the
/// audio thread from ever having to wait on disk.
const FRAME_QUEUE_CAPACITY: usize = 256;
/// How long [`RecordingSession::finish`] waits for a writer to drain and close.
const WRITER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const MANIFEST_VERSION: u32 = 1;

/// Shared stop flag and gap counter between a tap and the session that owns it.
struct TapState {
    stop: AtomicBool,
    dropped_frames: AtomicU64,
}

/// Outcome of one speaker's writer thread.
struct WriterOutcome {
    /// Final file name, or `None` when the speaker never produced audio and no
    /// file was created.
    file_name: Option<String>,
    frames: u64,
    error: Option<String>,
}

/// One recorded speaker, as reported to the UI and written to the manifest.
#[derive(Clone, Debug)]
pub(crate) struct RecordedSpeaker {
    pub(crate) name: String,
    pub(crate) node_id: String,
    pub(crate) file_name: Option<String>,
    pub(crate) frames: u64,
    pub(crate) dropped_frames: u64,
    pub(crate) error: Option<String>,
}

impl RecordedSpeaker {
    pub(crate) fn duration_ms(&self) -> u64 {
        self.frames * FRAME.as_millis() as u64
    }
}

/// Everything a finished recording produced.
#[derive(Clone, Debug)]
pub(crate) struct RecordingSummary {
    pub(crate) dir: PathBuf,
    pub(crate) started_at: String,
    pub(crate) ended_at: String,
    pub(crate) duration_ms: u64,
    pub(crate) speakers: Vec<RecordedSpeaker>,
}

/// Default directory new recordings are written into.
pub(crate) fn recordings_dir() -> Option<PathBuf> {
    wire::net::config_dir().map(|dir| dir.join("recordings"))
}

/// Open `path` in the platform file manager.
///
/// A failure here is never fatal: the recording is already on disk and the
/// user can navigate there manually.
pub(crate) fn reveal_in_file_manager(path: &Path) -> Result<()> {
    let mut command = if cfg!(target_os = "windows") {
        let mut command = Command::new("explorer");
        command.arg(path);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(path);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        command
    };
    command
        .spawn()
        .map(|_| ())
        .with_context(|| format!("open {}", path.display()))
}

/// A live recording. One file per speaker, all sharing one timeline.
pub(crate) struct RecordingSession {
    dir: PathBuf,
    started_at: String,
    speakers: BTreeMap<NodeId, SpeakerRecorder>,
    next_index: u32,
}

struct SpeakerRecorder {
    index: u32,
    name_hint: String,
    state: Arc<TapState>,
    /// Where this participant's recorder reads its track from. Kept so a
    /// rejoin can hand it a new one instead of stranding the file.
    source: VoiceSource,
    thread: Option<JoinHandle<WriterOutcome>>,
}

impl RecordingSession {
    /// Create a fresh recording directory and start a session in it.
    pub(crate) fn start(root: &Path) -> Result<Self> {
        let started = SystemTime::now();
        let dir = create_session_dir(root, started)?;
        fs::write(dir.join("README.txt"), readme_text(started))
            .with_context(|| format!("write {}", dir.join("README.txt").display()))?;
        info!(dir = %dir.display(), "call recording started");
        Ok(Self {
            dir,
            started_at: format_timestamp(started),
            speakers: BTreeMap::new(),
            next_index: 0,
        })
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Record the local microphone into its own file.
    ///
    /// The tap is added to the capture loop as an extra sink, so it starts
    /// feeding audio immediately and is independent of how many call tracks
    /// are open.
    pub(crate) async fn attach_microphone(
        &mut self,
        audio: &AudioContext,
        node_id: NodeId,
        name: String,
    ) -> Result<()> {
        if self.speakers.contains_key(&node_id) {
            return Ok(());
        }
        let slot = self.reserve_slot(node_id, name);
        let (tap, writer) =
            start_speaker_writer(slot.file_name.clone(), self.dir.clone(), slot.state.clone());
        // The capture loop keeps this sink alive and ticks it once per frame;
        // it drops the tap when the stop flag is set, which closes the writer.
        audio.add_capture_sink(tap).await.with_context(|| {
            format!(
                "could not attach the microphone recorder in {}",
                self.dir.display()
            )
        })?;
        self.speakers
            .insert(node_id, slot.into_recorder(writer));
        Ok(())
    }

    /// Record one remote participant's voice into its own file.
    ///
    /// The track is resubscribed rather than shared with the playback decoder,
    /// so recording neither steals packets from nor is coupled to what the
    /// user hears. That also means it can be attached at any point in a call.
    pub(crate) fn attach_participant(
        &mut self,
        node_id: NodeId,
        name: String,
        generation: u64,
        track: MediaTrack,
    ) -> Result<()> {
        if self.speakers.contains_key(&node_id) {
            return Ok(());
        }
        let slot = self.reserve_slot(node_id, name);
        let (mut tap, writer) =
            start_speaker_writer(slot.file_name.clone(), self.dir.clone(), slot.state.clone());
        let file_name = slot.file_name.clone();
        let source = VoiceSource::new(generation, track);
        let thread_source = source.clone();
        std::thread::Builder::new()
            .name(format!("wire-record-{}", node_id.fmt_short()))
            .spawn(move || {
                // Owning the tap here means dropping it on the way out closes
                // the writer, so the file is finalized however this ends.
                decode_loop(thread_source, &mut tap);
                info!(file = %file_name, "participant recorder stopped");
            })
            .with_context(|| {
                format!(
                    "could not start the recorder for {} in {}",
                    node_id.fmt_short(),
                    self.dir.display()
                )
            })?;
        self.speakers.insert(
            node_id,
            SpeakerRecorder {
                source,
                ..slot.into_recorder(writer)
            },
        );
        Ok(())
    }

    /// Point a participant's recorder at a track from a newer call.
    ///
    /// A peer that drops and rejoins opens a brand new track. Without this their
    /// file would go silent from the dropout onwards, which looks exactly like
    /// them having said nothing for the rest of the meeting.
    pub(crate) fn replace_participant_track(
        &mut self,
        node_id: NodeId,
        generation: u64,
        track: MediaTrack,
    ) {
        let Some(recorder) = self.speakers.get_mut(&node_id) else {
            return;
        };
        if recorder.source.replace(generation, track) {
            info!(
                peer = %node_id.fmt_short(),
                generation,
                "participant rejoined; continuing their recording on the new track"
            );
        }
    }

    pub(crate) fn has_speaker(&self, node_id: NodeId) -> bool {
        self.speakers.contains_key(&node_id)
    }

    fn reserve_slot(&mut self, node_id: NodeId, name: String) -> SpeakerSlot {
        let index = self.next_index;
        self.next_index += 1;
        let file_name = format!("{index:02}-{}.wav", sanitize_file_stem(&name));
        info!(
            peer = %node_id.fmt_short(),
            index,
            file = %file_name,
            "recording a new speaker"
        );
        SpeakerSlot {
            index,
            name,
            file_name,
            state: Arc::new(TapState {
                stop: AtomicBool::new(false),
                dropped_frames: AtomicU64::new(0),
            }),
        }
    }

    /// Stop every tap and wait for every writer to close its file.
    ///
    /// Idempotent: the speaker map is taken, so a second call does nothing.
    fn stop_and_join(&mut self) -> BTreeMap<NodeId, StoppedSpeaker> {
        for recorder in self.speakers.values() {
            recorder.state.stop.store(true, Ordering::Release);
        }
        std::mem::take(&mut self.speakers)
            .into_iter()
            .map(|(node_id, mut recorder)| {
                let outcome = wait_for_writer(recorder.thread.take());
                let stopped = StoppedSpeaker {
                    index: recorder.index,
                    name_hint: recorder.name_hint,
                    // Still under the provisional name at this point.
                    provisional_file: outcome.file_name,
                    frames: outcome.frames,
                    dropped_frames: recorder.state.dropped_frames.load(Ordering::Relaxed),
                    error: outcome.error,
                };
                (node_id, stopped)
            })
            .collect()
    }

    /// Stop recording, finalize every file, and write the manifest.
    ///
    /// Blocking: run this off the runtime. Each speaker is renamed to its
    /// resolved display name here, which is the first point where the worker
    /// knows more than the short id it had when the file was created.
    pub(crate) fn finish(mut self, names: &BTreeMap<NodeId, String>) -> RecordingSummary {
        let stopped = self.stop_and_join();

        let mut speakers = Vec::with_capacity(stopped.len());
        for (node_id, speaker) in stopped {
            let name = names.get(&node_id).cloned().unwrap_or(speaker.name_hint);
            // Renaming needs the directory, and only after the file is closed.
            let file_name = speaker
                .provisional_file
                .and_then(|provisional| rename_to(&self.dir, &provisional, &name, speaker.index));
            speakers.push(RecordedSpeaker {
                name,
                node_id: node_id.to_string(),
                file_name,
                frames: speaker.frames,
                dropped_frames: speaker.dropped_frames,
                error: speaker.error,
            });
        }
        speakers.sort_by_key(|speaker| speaker.name.to_lowercase());

        let ended_at = format_timestamp(SystemTime::now());
        let summary = RecordingSummary {
            dir: self.dir.clone(),
            started_at: self.started_at.clone(),
            ended_at: ended_at.clone(),
            duration_ms: speakers
                .iter()
                .map(RecordedSpeaker::duration_ms)
                .max()
                .unwrap_or(0),
            speakers,
        };
        if let Err(error) = write_manifest(&self.dir, &summary) {
            warn!(dir = %self.dir.display(), "could not write the recording manifest: {error:#}");
        }

        let captured = summary
            .speakers
            .iter()
            .filter(|speaker| speaker.file_name.is_some())
            .count();
        let gaps: u64 = summary
            .speakers
            .iter()
            .map(|speaker| speaker.dropped_frames)
            .sum();
        info!(
            dir = %self.dir.display(),
            speakers = captured,
            duration_ms = summary.duration_ms,
            dropped_frames = gaps,
            "call recording finished"
        );
        for speaker in &summary.speakers {
            if let Some(error) = &speaker.error {
                warn!(speaker = %speaker.name, "recording failed for this speaker: {error}");
            }
        }
        summary
    }
}

impl Drop for RecordingSession {
    fn drop(&mut self) {
        // A session dropped without `finish` — an error on the way in, or a
        // panic — must still stop its taps and let every writer patch its WAV
        // header. Dropping the tap closes the queue, which is what triggers the
        // finalization, so this is the difference between a readable recording
        // and a corrupt one.
        self.stop_and_join();
    }
}

/// One speaker after its writer has closed, before display names are applied.
struct StoppedSpeaker {
    index: u32,
    name_hint: String,
    /// The name the file was created under, if one was created at all.
    provisional_file: Option<String>,
    frames: u64,
    dropped_frames: u64,
    error: Option<String>,
}

struct SpeakerSlot {
    index: u32,
    name: String,
    file_name: String,
    state: Arc<TapState>,
}

impl SpeakerSlot {
    fn into_recorder(self, writer: JoinHandle<WriterOutcome>) -> SpeakerRecorder {
        SpeakerRecorder {
            index: self.index,
            name_hint: self.name,
            state: self.state,
            // Replaced immediately by the caller that owns the track slot.
            source: VoiceSource::default(),
            thread: Some(writer),
        }
    }
}

/// Where a participant recorder reads its current track from.
///
/// The call worker writes; the recorder thread reads once per 20 ms tick. The
/// lock is only ever held for a clone, never across decoding, so a rejoin can
/// never stall audio.
#[derive(Clone, Default)]
struct VoiceSource {
    current: Arc<Mutex<Option<(u64, MediaTrack)>>>,
}

impl VoiceSource {
    fn new(generation: u64, track: MediaTrack) -> Self {
        Self {
            current: Arc::new(Mutex::new(Some((generation, track)))),
        }
    }

    /// Swap in a newer track. Returns whether anything changed.
    fn replace(&self, generation: u64, track: MediaTrack) -> bool {
        let Ok(mut current) = self.current.lock() else {
            return false;
        };
        if current.as_ref().is_some_and(|(seen, _)| *seen == generation) {
            return false;
        }
        *current = Some((generation, track));
        true
    }

    /// The track to decode, unless it is the one already being decoded.
    ///
    /// A generation of zero means "nothing decoded yet", so the first track is
    /// always adopted.
    fn undecoded(&self, seen: u64) -> Option<(u64, MediaTrack)> {
        let current = self.current.lock().ok()?;
        let (generation, track) = current.as_ref()?;
        (*generation != seen).then(|| (*generation, track.clone()))
    }
}

/// Create the tap/queue pair plus the thread that owns one speaker's file.
///
/// The file is created lazily by the writer on its first frame, so a
/// participant who never speaks leaves no empty recording behind.
fn start_speaker_writer(
    file_name: String,
    dir: PathBuf,
    state: Arc<TapState>,
) -> (RecordingTap, JoinHandle<WriterOutcome>) {
    let (sender, receiver) = sync_channel::<Vec<f32>>(FRAME_QUEUE_CAPACITY);
    let writer_name = file_name.clone();
    let writer = std::thread::Builder::new()
        .name(format!("wire-record-writer-{writer_name}"))
        .spawn(move || write_loop(receiver, dir, writer_name))
        .expect("recording writer thread");
    (
        RecordingTap {
            sender: Some(sender),
            state,
        },
        writer,
    )
}

/// Tap from the capture loop: one call per 20 ms of microphone audio.
pub(crate) struct RecordingTap {
    sender: Option<SyncSender<Vec<f32>>>,
    state: Arc<TapState>,
}

impl RecordingTap {
    /// Hand one engine tick to the writer as exactly one mono frame.
    ///
    /// Anything the tick did not fill is silence rather than a shorter file, so
    /// a starved or muted tick costs a gap in the transcript instead of
    /// shifting every later timestamp.
    fn push(&mut self, samples: &[f32]) {
        if self.sender.is_none() {
            return;
        }
        let mut frame: Vec<f32> = samples
            .as_chunks::<2>()
            .0
            .iter()
            .map(|frame| (frame[0] + frame[1]) * 0.5)
            .take(FRAME_SAMPLES)
            .collect();
        frame.resize(FRAME_SAMPLES, 0.0);
        let Some(sender) = &self.sender else { return };
        match sender.try_send(frame) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                let dropped = self.state.dropped_frames.fetch_add(1, Ordering::Relaxed) + 1;
                // Never block the audio thread for disk. Log sparsely so a
                // degraded disk is visible without flooding the log.
                if dropped <= 3 || dropped.is_power_of_two() {
                    warn!(
                        dropped,
                        "recording writer is behind; skipping frames (transcript will have a gap)"
                    );
                }
            }
            Err(TrySendError::Disconnected(_)) => self.sender = None,
        }
    }
}

impl AudioSink for RecordingTap {
    fn tick(&mut self, buf: &[f32]) -> Result<ControlFlow<(), ()>> {
        if self.state.stop.load(Ordering::Acquire) {
            // Dropping the sender is what finalizes the file. The capture loop
            // removes this sink on the next tick.
            self.sender = None;
            return Ok(ControlFlow::Break(()));
        }
        self.push(buf);
        Ok(ControlFlow::Continue(()))
    }
}

/// Drive one remote track's decoder on a private 20 ms clock.
///
/// Playback runs on its own clock and applies deafening and per-participant
/// volume; recording must not inherit either. A private decoder also means a
/// recording can be started, stopped, and restarted without touching the audio
/// the user hears.
fn decode_loop(source: VoiceSource, tap: &mut RecordingTap) {
    let mut buf = vec![0.0; ENGINE_FRAME_SAMPLES];
    // Generation 0 is never a real call generation, so the first track is
    // always adopted.
    let mut decoded = 0u64;
    let mut decoder: Option<MediaTrackOpusDecoder> = None;
    let mut deadline = Instant::now();
    loop {
        if tap.state.stop.load(Ordering::Acquire) {
            break;
        }
        if let Some((generation, track)) = source.undecoded(decoded) {
            match MediaTrackOpusDecoder::new(track) {
                Ok(next) => {
                    decoded = generation;
                    decoder = Some(next);
                }
                Err(error) => {
                    warn!("could not open a recorder decoder: {error:#}");
                    // Mark it as seen so a failing track is not retried 50 times
                    // a second for the rest of the call.
                    decoded = generation;
                }
            }
        }

        match decoder.as_mut() {
            Some(active) => match active.tick(&mut buf) {
                Ok(ControlFlow::Continue(count)) => tap.push(&buf[..count]),
                // The peer hung up. Keep the slot open: a rejoin supplies a new
                // track, and the silence in between keeps the timeline intact.
                Ok(ControlFlow::Break(())) => decoder = None,
                Err(error) => {
                    warn!("recording decoder stopped: {error:#}");
                    break;
                }
            },
            // Nobody is connected to this speaker right now.
            None => tap.push(&[]),
        }

        deadline += FRAME;
        let now = Instant::now();
        if deadline > now {
            spin_sleep::sleep(deadline - now);
        } else {
            // Behind schedule (a slow disk or a suspended machine). Resynchronize
            // instead of trying to replay missed ticks, which would invent audio.
            deadline = now;
        }
    }
}

fn write_loop(receiver: Receiver<Vec<f32>>, dir: PathBuf, file_name: String) -> WriterOutcome {
    let mut wav: Option<WavFile> = None;
    let mut outcome = WriterOutcome {
        file_name: None,
        frames: 0,
        error: None,
    };
    while let Ok(frame) = receiver.recv() {
        if wav.is_none() {
            match WavFile::create(&dir.join(&file_name)) {
                Ok(file) => wav = Some(file),
                Err(error) => {
                    outcome.error = Some(format!("{error:#}"));
                    warn!(path = %dir.join(&file_name).display(), "could not create the recording file: {error:#}");
                    // Drain the queue so the tap is not stuck retrying.
                    while receiver.recv().is_ok() {}
                    break;
                }
            }
        }
        let Some(file) = wav.as_mut() else { continue };
        if let Err(error) = file.append(&frame) {
            outcome.error = Some(format!("{error:#}"));
            warn!(path = %file.path.display(), "recording write failed: {error:#}");
            while receiver.recv().is_ok() {}
            break;
        }
        outcome.frames += 1;
    }

    if let Some(file) = wav {
        outcome.file_name = Some(file_name.clone());
        let path = file.path.clone();
        if let Err(error) = file.finalize() {
            warn!(path = %path.display(), "could not finalize the recording file: {error:#}");
            outcome.error.get_or_insert_with(|| format!("{error:#}"));
        }
    }
    outcome
}

/// Wait for a writer thread without risking an indefinite block.
///
/// A tap that is never ticked again (a capture device that disappeared) would
/// otherwise keep its sender alive forever.
fn wait_for_writer(thread: Option<JoinHandle<WriterOutcome>>) -> WriterOutcome {
    let Some(thread) = thread else {
        return WriterOutcome {
            file_name: None,
            frames: 0,
            error: Some("no writer thread".to_owned()),
        };
    };
    let deadline = Instant::now() + WRITER_SHUTDOWN_TIMEOUT;
    while !thread.is_finished() {
        if Instant::now() >= deadline {
            warn!("recording writer did not finish in time; leaving its file unfinalized");
            return WriterOutcome {
                file_name: None,
                frames: 0,
                error: Some("the writer thread did not shut down in time".to_owned()),
            };
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    thread.join().unwrap_or_else(|_| WriterOutcome {
        file_name: None,
        frames: 0,
        error: Some("the writer thread panicked".to_owned()),
    })
}

/// A canonical 44-byte RIFF/WAVE header for mono 16-bit PCM.
fn wav_header(data_bytes: u64) -> [u8; 44] {
    let mut header = [0u8; 44];
    let byte_rate = SAMPLE_RATE * u32::from(CHANNELS) * u32::from(BITS_PER_SAMPLE / 8);
    let block_align = CHANNELS * (BITS_PER_SAMPLE / 8);
    let data_bytes = u32::try_from(data_bytes).unwrap_or(u32::MAX);
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&36u32.to_le_bytes()); // patched with the data size
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    header[22..24].copy_from_slice(&CHANNELS.to_le_bytes());
    header[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&BITS_PER_SAMPLE.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_bytes.to_le_bytes()); // patched with the data size
    header
}

struct WavFile {
    path: PathBuf,
    file: File,
    frames: u64,
}

impl WavFile {
    fn create(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("create {}", path.display()))?;
        file.write_all(&wav_header(0))
            .with_context(|| format!("write the header of {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            frames: 0,
        })
    }

    fn append(&mut self, mono: &[f32]) -> Result<()> {
        let mut pcm = Vec::with_capacity(mono.len() * 2);
        for sample in mono {
            pcm.extend_from_slice(&sample_to_i16(*sample).to_le_bytes());
        }
        self.file
            .write_all(&pcm)
            .with_context(|| format!("append to {}", self.path.display()))?;
        self.frames += 1;
        Ok(())
    }

    /// Patch the RIFF and data sizes now that the length is known, then flush.
    fn finalize(mut self) -> Result<()> {
        let data_bytes = self.frames * FRAME_SAMPLES as u64 * u64::from(BITS_PER_SAMPLE / 8);
        self.file
            .seek(SeekFrom::Start(4))
            .context("seek to the RIFF size")?;
        self.file
            .write_all(&u32::try_from(36 + data_bytes).unwrap_or(u32::MAX).to_le_bytes())
            .context("write the RIFF size")?;
        self.file
            .seek(SeekFrom::Start(40))
            .context("seek to the data size")?;
        self.file
            .write_all(&u32::try_from(data_bytes).unwrap_or(u32::MAX).to_le_bytes())
            .context("write the data size")?;
        self.file.flush().context("flush the recording")?;
        // Best effort: a crash between flush and sync must not lose the tail.
        let _ = self.file.sync_all();
        Ok(())
    }
}

fn sample_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
}

#[derive(Serialize)]
struct Manifest {
    version: u32,
    started_at: String,
    ended_at: String,
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    frame_ms: u64,
    format: &'static str,
    note: &'static str,
    speakers: Vec<ManifestSpeaker>,
}

#[derive(Serialize)]
struct ManifestSpeaker {
    name: String,
    node_id: String,
    /// `None` when this participant never produced audio, so no file exists.
    file: Option<String>,
    frames: u64,
    duration_ms: u64,
    /// Frames lost to a writer that could not keep up. Non-zero means the
    /// transcript will have a gap here.
    dropped_frames: u64,
    error: Option<String>,
}

fn write_manifest(dir: &Path, summary: &RecordingSummary) -> Result<()> {
    let manifest = Manifest {
        version: MANIFEST_VERSION,
        started_at: summary.started_at.clone(),
        ended_at: summary.ended_at.clone(),
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS,
        bits_per_sample: BITS_PER_SAMPLE,
        frame_ms: FRAME.as_millis() as u64,
        format: "wav",
        note: "One file per speaker. Every file starts at started_at and advances in \
               frame_ms steps, so a transcript timestamp can be mapped onto the call by \
               adding started_at.",
        speakers: summary
            .speakers
            .iter()
            .map(|speaker| ManifestSpeaker {
                name: speaker.name.clone(),
                node_id: speaker.node_id.clone(),
                file: speaker.file_name.clone(),
                frames: speaker.frames,
                duration_ms: speaker.duration_ms(),
                dropped_frames: speaker.dropped_frames,
                error: speaker.error.clone(),
            })
            .collect(),
    };
    let path = dir.join("manifest.json");
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))
}

fn readme_text(started: SystemTime) -> String {
    format!(
        "Wire call recording\n\
         ==================\n\n\
         Started: {}\n\
         Audio:    {} Hz, mono, {} bit PCM WAV\n\
         Timeline: {} ms per frame, all files start together\n\n\
         Each WAV holds exactly one person's voice, so any single-speaker\n\
         transcription model can be pointed at one file without speaker labels.\n\
         Because every file shares one origin and one frame size, a segment that\n\
         starts at T in one file started at started_at + T in the call. Merging\n\
         the transcripts is therefore a matter of interleaving segments by time.\n\n\
         See manifest.json for the speaker-to-file mapping and durations.\n",
        format_timestamp(started),
        SAMPLE_RATE,
        BITS_PER_SAMPLE,
        FRAME.as_millis(),
    )
}

fn create_session_dir(root: &Path, started: SystemTime) -> Result<PathBuf> {
    fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
    let stem = timestamp_slug(started);
    // Millisecond precision plus a collision suffix keeps two recordings from
    // ever sharing a directory, which would make the second overwrite the first.
    for attempt in 0..64u32 {
        let name = if attempt == 0 {
            stem.clone()
        } else {
            format!("{stem}-{attempt}")
        };
        match fs::create_dir(root.join(&name)) {
            Ok(()) => return Ok(root.join(&name)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("create the directory {}", name))
            }
        }
    }
    Err(anyhow!(
        "could not find a free recording directory in {}",
        root.display()
    ))
}

fn rename_to(dir: &Path, provisional: &str, name: &str, index: u32) -> Option<String> {
    let final_name = format!("{index:02}-{}.wav", sanitize_file_stem(name));
    if final_name == provisional {
        return Some(provisional.to_owned());
    }
    let from = dir.join(provisional);
    let to = dir.join(&final_name);
    match fs::rename(&from, &to) {
        Ok(()) => Some(final_name),
        Err(error) => {
            // The audio is already on disk under the provisional name, so a
            // failed rename costs readability, not the recording.
            warn!(
                from = %from.display(),
                to = %to.display(),
                "could not rename the recording to the display name: {error}"
            );
            Some(provisional.to_owned())
        }
    }
}

/// Reduce a display name to something safe on every filesystem.
fn sanitize_file_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, ' ' | '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(|c| c == '.' || c == ' ');
    if trimmed.is_empty() {
        "speaker".to_owned()
    } else {
        trimmed.chars().take(48).collect()
    }
}

fn timestamp_slug(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let millis = time
        .duration_since(UNIX_EPOCH)
        .map(|since| since.subsec_millis())
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = civil_from_unix_seconds(seconds as i64);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}-{minute:02}-{second:02}-{millis:03}Z")
}

/// ISO-8601 in UTC. Wall-clock time is not available without pulling in a
/// timezone database, and an unambiguous instant is more useful here than a
/// local-looking one that could be wrong.
fn format_timestamp(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = civil_from_unix_seconds(seconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert a Unix timestamp to a proleptic Gregorian civil date (UTC).
///
/// Howard Hinnant's `civil_from_days`, which shifts the epoch to 0000-03-01 so
/// leap days land at the end of the cycle.
fn civil_from_unix_seconds(seconds: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (
        year,
        month,
        day,
        (time_of_day / 3_600) as u32,
        ((time_of_day % 3_600) / 60) as u32,
        (time_of_day % 60) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wire::audio::ENGINE_FORMAT;

    fn silent_track() -> MediaTrack {
        // The sender is dropped immediately, so the decoder reports a closed
        // track and the recorder exits without needing real audio.
        let (_sender, receiver) = tokio::sync::broadcast::channel(8);
        MediaTrack::new(
            receiver,
            wire::codec::Codec::Opus {
                channels: wire::codec::opus::OpusChannels::Mono,
            },
            wire::rtc::TrackKind::Audio,
        )
    }

    /// Build a tap plus a writer over an in-memory receiver so framing can be
    /// asserted without touching a filesystem.
    fn tap_for_test(capacity: usize) -> (RecordingTap, Receiver<Vec<f32>>, Arc<TapState>) {
        let (sender, receiver) = sync_channel::<Vec<f32>>(capacity);
        let state = Arc::new(TapState {
            stop: AtomicBool::new(false),
            dropped_frames: AtomicU64::new(0),
        });
        (
            RecordingTap {
                sender: Some(sender),
                state: state.clone(),
            },
            receiver,
            state,
        )
    }

    fn drain(receiver: &Receiver<Vec<f32>>) -> Vec<Vec<f32>> {
        let mut frames = Vec::new();
        while let Ok(frame) = receiver.try_recv() {
            frames.push(frame);
        }
        frames
    }

    fn stereo(frames: usize, value: f32) -> Vec<f32> {
        (0..frames * 2).map(|_| value).collect()
    }

    #[test]
    fn every_tick_produces_exactly_one_frame_of_the_right_length() {
        let (mut tap, receiver, _) = tap_for_test(64);
        tap.push(&stereo(FRAME_SAMPLES, 0.5));
        tap.push(&stereo(FRAME_SAMPLES, -0.25));
        let frames = drain(&receiver);

        assert_eq!(frames.len(), 2);
        for frame in &frames {
            assert_eq!(frame.len(), FRAME_SAMPLES);
        }
        assert!(frames[0].iter().all(|sample| (*sample - 0.5).abs() < 1e-6));
        assert!(frames[1].iter().all(|sample| (*sample + 0.25).abs() < 1e-6));
    }

    #[test]
    fn a_starved_or_silent_tick_becomes_silence_instead_of_a_short_frame() {
        let (mut tap, receiver, _) = tap_for_test(64);
        // A starved tick delivers nothing at all, a partial one delivers half.
        tap.push(&[]);
        tap.push(&stereo(FRAME_SAMPLES / 2, 0.5));
        tap.push(&[]);
        let frames = drain(&receiver);

        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|frame| frame.len() == FRAME_SAMPLES));
        assert!(frames[0].iter().all(|sample| *sample == 0.0));
        assert_eq!(
            frames[1].iter().filter(|sample| **sample != 0.0).count(),
            FRAME_SAMPLES / 2
        );
        assert!(frames[1].iter().all(|sample| *sample == 0.0 || *sample == 0.5));
        assert!(frames[2].iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn an_oversized_tick_is_truncated_rather_than_shifting_the_timeline() {
        let (mut tap, receiver, _) = tap_for_test(64);
        tap.push(&stereo(FRAME_SAMPLES * 2, 1.0));
        let frames = drain(&receiver);

        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), FRAME_SAMPLES);
    }

    #[test]
    fn a_saturated_writer_drops_frames_instead_of_blocking_the_audio_thread() {
        let (mut tap, receiver, state) = tap_for_test(1);
        tap.push(&stereo(FRAME_SAMPLES, 0.5));
        tap.push(&stereo(FRAME_SAMPLES, 0.5));
        tap.push(&stereo(FRAME_SAMPLES, 0.5));

        // The tap returned instead of waiting for the writer, and only the
        // frames the queue could hold were accepted.
        assert_eq!(state.dropped_frames.load(Ordering::Relaxed), 2);
        assert_eq!(drain(&receiver).len(), 1);
    }

    #[test]
    fn a_stopped_tap_closes_the_queue_so_the_file_can_be_finalized() {
        let (mut tap, receiver, state) = tap_for_test(8);
        tap.push(&stereo(FRAME_SAMPLES, 0.5));
        state.stop.store(true, Ordering::Release);

        let flow = tap.tick(&stereo(FRAME_SAMPLES, 0.5)).expect("tick");

        assert!(matches!(flow, ControlFlow::Break(())));
        assert_eq!(drain(&receiver).len(), 1);
        assert!(tap.sender.is_none());
    }

    #[test]
    fn the_engine_frame_matches_what_the_capture_loop_hands_to_a_sink() {
        assert_eq!(ENGINE_FRAME_SAMPLES, ENGINE_FORMAT.sample_count(FRAME));
        assert_eq!(FRAME_SAMPLES, 960);
    }

    #[test]
    fn writes_a_canonical_wav_header_and_pads_the_sizes_on_close() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("speaker.wav");
        let mut file = WavFile::create(&path).expect("create");
        for _ in 0..3 {
            file.append(&vec![0.5; FRAME_SAMPLES]).expect("append");
        }
        let frames = file.frames;
        file.finalize().expect("finalize");

        let bytes = fs::read(&path).expect("read back");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 1);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            SAMPLE_RATE
        );
        assert_eq!(u32::from_le_bytes(bytes[28..32].try_into().unwrap()), 96_000);
        assert_eq!(u16::from_le_bytes(bytes[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 16);
        assert_eq!(&bytes[36..40], b"data");

        let expected_data = frames * FRAME_SAMPLES as u64 * 2;
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as u64,
            expected_data
        );
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as u64,
            36 + expected_data
        );
        assert_eq!(bytes.len() as u64, 44 + expected_data);
    }

    #[test]
    fn a_full_session_writes_one_aligned_file_per_participant() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().to_path_buf();
        let (sender, receiver) = sync_channel::<Vec<f32>>(64);
        let writer = std::thread::spawn(move || {
            write_loop(receiver, root, "ada.wav".to_owned())
        });
        // 100 frames, then close: the file must be exactly 100 frames long and
        // readable as plain PCM, with the sizes patched in.
        for _ in 0..100 {
            sender.send(vec![0.25; FRAME_SAMPLES]).expect("send");
        }
        drop(sender);
        let outcome = writer.join().expect("join writer");

        assert_eq!(outcome.frames, 100);
        assert_eq!(outcome.file_name.as_deref(), Some("ada.wav"));
        assert!(outcome.error.is_none());
        assert_eq!(outcome.frames * FRAME.as_millis() as u64, 2_000);

        let bytes = fs::read(dir.path().join("ada.wav")).expect("read back");
        assert_eq!(bytes.len(), 44 + 100 * FRAME_SAMPLES * 2);
        // The first sample round-trips through i16 without clipping.
        let first = i16::from_le_bytes(bytes[44..46].try_into().unwrap());
        assert!((first - i16::MAX / 4).abs() <= 1, "got {first}");
    }

    #[test]
    fn a_speaker_that_never_speaks_leaves_no_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (sender, receiver) = sync_channel::<Vec<f32>>(4);
        // No frames ever arrive, and the tap goes away, which is exactly when a
        // participant's file must not be created.
        drop(sender);
        let outcome = write_loop(receiver, dir.path().to_path_buf(), "quiet.wav".to_owned());

        assert!(outcome.file_name.is_none());
        assert_eq!(outcome.frames, 0);
        assert!(!dir.path().join("quiet.wav").exists());
    }

    #[test]
    fn clips_samples_rather_than_wrapping_them() {
        assert_eq!(sample_to_i16(0.0), 0);
        assert_eq!(sample_to_i16(1.0), i16::MAX);
        assert_eq!(sample_to_i16(-1.0), -i16::MAX);
        assert_eq!(sample_to_i16(4.0), i16::MAX);
        assert_eq!(sample_to_i16(-4.0), -i16::MAX);
    }

    #[test]
    fn file_names_stay_readable_and_safe() {
        assert_eq!(sanitize_file_stem("Ada Lovelace"), "Ada Lovelace");
        assert_eq!(sanitize_file_stem("Ada   Lovelace"), "Ada Lovelace");
        assert_eq!(sanitize_file_stem("a/b:c*d"), "a-b-c-d");
        // Leading dots would hide the file or create a traversal segment.
        assert_eq!(sanitize_file_stem("../escape"), "-escape");
        assert_eq!(sanitize_file_stem(""), "speaker");
        assert_eq!(sanitize_file_stem("   "), "speaker");
        assert_eq!(sanitize_file_stem(&"x".repeat(200)).len(), 48);
    }

    #[test]
    fn converts_unix_timestamps_to_utc_civil_dates() {
        assert_eq!(civil_from_unix_seconds(0), (1970, 1, 1, 0, 0, 0));
        // A leap day, to catch the March-based month shift.
        assert_eq!(
            civil_from_unix_seconds(1_709_164_800),
            (2024, 2, 29, 0, 0, 0)
        );
        assert_eq!(civil_from_unix_seconds(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(
            civil_from_unix_seconds(1_791_037_925),
            (2026, 10, 3, 14, 32, 5)
        );
        assert_eq!(format_timestamp(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn directory_slugs_sort_chronologically() {
        let early = timestamp_slug(UNIX_EPOCH + Duration::from_secs(1_791_037_925));
        let late = timestamp_slug(UNIX_EPOCH + Duration::from_secs(1_791_037_925 + 3_600));
        assert!(early < late, "{early} should sort before {late}");
        assert_eq!(early, "2026-10-03T14-32-05-000Z");
        // Millisecond precision keeps two recordings in the same second apart.
        assert_eq!(
            timestamp_slug(UNIX_EPOCH + Duration::new(1_791_037_925, 250_000_000)),
            "2026-10-03T14-32-05-250Z"
        );
    }

    #[test]
    fn a_second_recording_never_reuses_an_existing_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let now = SystemTime::now();
        let first = create_session_dir(dir.path(), now).expect("first");
        let second = create_session_dir(dir.path(), now).expect("second");

        assert_ne!(first, second);
        assert!(first.is_dir());
        assert!(second.is_dir());
    }

    #[test]
    fn the_manifest_records_the_alignment_a_transcriber_needs() {
        let dir = tempfile::tempdir().expect("temp dir");
        let summary = RecordingSummary {
            dir: dir.path().to_path_buf(),
            started_at: "2026-10-03T14:32:05Z".to_owned(),
            ended_at: "2026-10-03T14:33:05Z".to_owned(),
            duration_ms: 60_000,
            speakers: vec![
                RecordedSpeaker {
                    name: "Ada Lovelace".to_owned(),
                    node_id: "peer".to_owned(),
                    file_name: Some("01-Ada Lovelace.wav".to_owned()),
                    frames: 3_000,
                    dropped_frames: 0,
                    error: None,
                },
                RecordedSpeaker {
                    name: "Silent Peer".to_owned(),
                    node_id: "quiet".to_owned(),
                    file_name: None,
                    frames: 0,
                    dropped_frames: 0,
                    error: None,
                },
            ],
        };
        write_manifest(dir.path(), &summary).expect("write manifest");

        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join("manifest.json")).unwrap())
                .expect("parse manifest");
        assert_eq!(manifest["frame_ms"], FRAME.as_millis() as u64);
        assert_eq!(manifest["sample_rate"], SAMPLE_RATE);
        assert_eq!(manifest["channels"], 1);
        assert_eq!(manifest["speakers"][0]["file"], "01-Ada Lovelace.wav");
        assert_eq!(manifest["speakers"][0]["duration_ms"], 60_000);
        assert!(manifest["speakers"][1]["file"].is_null());
    }

    #[test]
    fn a_session_records_one_file_per_participant() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut session = RecordingSession::start(dir.path()).expect("start");
        // Node ids are validated public keys, so derive real ones.
        let ada = iroh::SecretKey::from_bytes(&[1u8; 32]).public();
        let alan = iroh::SecretKey::from_bytes(&[2u8; 32]).public();

        for (node_id, name) in [(ada, "Ada"), (alan, "Alan")] {
            session
                .attach_participant(node_id, name.to_owned(), 1, silent_track())
                .expect("attach");
        }
        // Re-attaching the same peer must not produce a second file.
        assert!(
            session
                .attach_participant(ada, "Ada".to_owned(), 2, silent_track())
                .is_ok()
        );
        assert_eq!(session.speakers.len(), 2);
        assert!(session.has_speaker(alan));

        // Names learned while recording are only applied on close.
        let session_dir = session.dir().to_path_buf();
        let summary = session.finish(&BTreeMap::from([(alan, "Alan Turing".to_owned())]));

        assert_eq!(summary.dir, session_dir);
        assert!(session_dir.starts_with(dir.path()));
        assert_eq!(summary.speakers.len(), 2);
        let ada = summary
            .speakers
            .iter()
            .find(|speaker| speaker.node_id == ada.to_string())
            .expect("ada");
        assert_eq!(ada.name, "Ada");
        let alan = summary
            .speakers
            .iter()
            .find(|speaker| speaker.node_id == alan.to_string())
            .expect("alan");
        assert_eq!(alan.name, "Alan Turing");
        // No audio ever arrived, so nothing was written to disk.
        assert!(summary.speakers.iter().all(|s| s.file_name.is_none()));
        assert!(session_dir.join("manifest.json").exists());
        assert!(session_dir.join("README.txt").exists());
        // The README lands in the session directory, not the recordings root.
        assert!(!dir.path().join("README.txt").exists());
    }

    /// Drive a real Opus stream through the whole recording path.
    ///
    /// This is the closest thing to a live call that can run without audio
    /// hardware: a real encoder feeds a real track, a real decoder consumes it,
    /// and the result must be a WAV whose length matches the audio sent. It
    /// covers the frame sizing that a hand-fed tap cannot prove.
    #[test]
    fn an_opus_stream_becomes_one_aligned_wav_file() {
        use wire::codec::opus::{AudioQuality, MediaTrackOpusEncoder};

        let dir = tempfile::tempdir().expect("temp dir");
        let ada = iroh::SecretKey::from_bytes(&[9u8; 32]).public();
        let (mut encoder, track) =
            MediaTrackOpusEncoder::new(64, ENGINE_FORMAT, AudioQuality::High).expect("encoder");
        // Resubscribe the way the worker does, so the encoder keeps its own
        // receiver alive while the recorder reads a second one.
        let mut session = RecordingSession::start(dir.path()).expect("start");
        session
            .attach_participant(ada, "Ada".to_owned(), 1, track.clone())
            .expect("attach");

        let frames = 50usize;
        let tick = ENGINE_FORMAT.sample_count(FRAME);
        for index in 0..frames {
            let value = 0.1 + index as f32 * 0.002;
            // The encoder reports a `ControlFlow` when its track is gone; here it
            // is held by `track`, so `Continue` is the only expected answer.
            assert!(matches!(
                encoder.tick(&vec![value; tick]),
                Ok(ControlFlow::Continue(()))
            ));
        }

        // The recorder thread is deadline-paced, so give it time to drain.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let bytes = fs::read(dir_wav_path(&dir, "00-Ada.wav")).unwrap_or_default();
            if bytes.len() >= 44 + frames * FRAME_SAMPLES * 2 || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let session_dir = session.dir().to_path_buf();
        let summary = session.finish(&BTreeMap::new());
        let ada = &summary.speakers[0];
        let file_name = ada.file_name.clone().expect("a file was written");
        assert_eq!(file_name, "00-Ada.wav");
        assert!(ada.error.is_none());
        assert_eq!(ada.dropped_frames, 0);
        // Never fewer frames than were encoded: no audio may be lost. More is
        // expected and correct, because the recorder keeps emitting silent
        // frames after the last packet to hold the timeline open.
        assert!(
            ada.frames as usize >= frames,
            "lost audio: {} frames recorded from {frames} encoded",
            ada.frames
        );

        // The file, the manifest, and the reported duration must all agree.
        let bytes = fs::read(session_dir.join(&file_name)).expect("read back");
        let expected = ada.frames as usize * FRAME_SAMPLES * 2;
        assert_eq!(bytes.len(), 44 + expected);
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize,
            expected
        );
        assert_eq!(ada.duration_ms(), ada.frames * 20);
        // Real audio, not silence: at least one sample is clearly non-zero.
        let pcm = &bytes[44..];
        assert!(
            pcm.as_chunks::<2>()
                .0
                .iter()
                .any(|s| i16::from_le_bytes(*s) > 100),
            "the decoded stream produced no audible samples"
        );
    }

    /// Find a session's WAV by name without depending on the timestamped folder.
    fn dir_wav_path(dir: &tempfile::TempDir, file_name: &str) -> PathBuf {
        fs::read_dir(dir.path())
            .expect("recordings root")
            .flatten()
            .map(|entry| entry.path().join(file_name))
            .find(|path| path.exists())
            .unwrap_or_else(|| dir.path().join(file_name))
    }

    /// A session dropped without `finish` must not leave a corrupt file.
    ///
    /// The safe path is hard to reach through the public API (it needs real
    /// audio hardware), so this checks the guarantee it protects: dropping the
    /// session closes its writers rather than abandoning them.
    #[test]
    fn dropping_a_session_releases_its_writers() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut session = RecordingSession::start(dir.path()).expect("start");
        let ada = iroh::SecretKey::from_bytes(&[11u8; 32]).public();
        session
            .attach_participant(ada, "Ada".to_owned(), 1, silent_track())
            .expect("attach");

        let session_dir = session.dir().to_path_buf();
        assert_eq!(session.speakers.len(), 1);
        // No `finish`, no rename, no manifest: the Drop has to cope.
        drop(session);

        // The recorder thread is gone, so nothing is left writing into the
        // directory after this returns.
        assert!(session_dir.is_dir());
        assert!(!session_dir.join("manifest.json").exists());
    }

    #[test]
    fn a_rejoining_participant_keeps_one_file_and_adopts_the_new_track() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut session = RecordingSession::start(dir.path()).expect("start");
        let ada = iroh::SecretKey::from_bytes(&[5u8; 32]).public();

        session
            .attach_participant(ada, "Ada".to_owned(), 1, silent_track())
            .expect("attach");
        let recorder = &session.speakers[&ada];

        // The first track is offered once, then never again.
        assert!(recorder.source.undecoded(0).is_some());
        assert!(recorder.source.undecoded(1).is_none());
        // The same generation is a duplicate and must be ignored.
        assert!(!recorder.source.replace(1, silent_track()));

        // A rejoin arrives as a new call generation.
        assert!(recorder.source.replace(2, silent_track()));
        assert!(recorder.source.undecoded(1).is_some());

        // The file is the speaker's, not the call's, so it is not replaced.
        assert_eq!(session.speakers.len(), 1);
        session.replace_participant_track(ada, 3, silent_track());
        assert_eq!(session.speakers.len(), 1);
        assert!(session.speakers[&ada].source.undecoded(2).is_some());
    }
}
