//! Per-peer video send/receive tasks and stream replacement controls.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use iroh::{endpoint::VarInt, NodeId};
use tracing::{info, warn};
use wire::{rtc::RtcConnection, video::transport};

#[cfg(target_os = "windows")]
use super::trim_process_working_set;
use super::{Event, EventPublisher, VideoStreamEndReason};
use crate::video_decode::VideoDecodeWorker;

const VIDEO_STREAM_RESET_CODE: VarInt = VarInt::from_u32(0x51);
const VIDEO_CONTROL_PAUSE: u8 = 0;
const VIDEO_CONTROL_RESUME: u8 = 1;
pub(super) struct VideoPeerTasks {
    pub(super) send: Option<tokio::task::JoinHandle<()>>,
    pub(super) send_stop: Option<tokio::sync::watch::Sender<VideoSendCommand>>,
    pub(super) recv: Option<tokio::task::JoinHandle<()>>,
    pub(super) recv_control: Option<tokio::sync::mpsc::UnboundedSender<VideoReceiveControl>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VideoSendCommand {
    Running,
    Finish,
    Replace,
}

impl VideoSendCommand {
    fn resets_stream(self) -> bool {
        self == Self::Replace
    }
}

impl VideoPeerTasks {
    pub(super) fn new() -> Self {
        Self {
            send: None,
            send_stop: None,
            recv: None,
            recv_control: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct VideoReceiveControl {
    pub(super) generation: u64,
    pub(super) watching: bool,
}

const VIDEO_SEND_STOP_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) async fn stop_video_send(
    node_id: NodeId,
    stop: Option<tokio::sync::watch::Sender<VideoSendCommand>>,
    handle: Option<tokio::task::JoinHandle<()>>,
    command: VideoSendCommand,
) {
    request_video_send_stop(node_id, stop.as_ref(), command);
    await_video_send_stop(node_id, handle).await;
}

pub(super) fn request_video_send_stop(
    node_id: NodeId,
    stop: Option<&tokio::sync::watch::Sender<VideoSendCommand>>,
    command: VideoSendCommand,
) {
    if let Some(stop) = stop {
        let _ = stop.send(command);
        info!(
            node = %node_id.fmt_short(),
            ?command,
            "requested cooperative video sender stop"
        );
    }
}

pub(super) async fn await_video_send_stop(
    node_id: NodeId,
    mut handle: Option<tokio::task::JoinHandle<()>>,
) {
    let Some(mut handle) = handle.take() else {
        return;
    };
    // `timeout` polls `&mut handle` to completion on success. A second
    // `handle.await` after that panics ("JoinHandle polled after completion"),
    // so only await again on the abort fallback path.
    match tokio::time::timeout(VIDEO_SEND_STOP_TIMEOUT, &mut handle).await {
        Ok(_) => {}
        Err(_) => {
            warn!(
                node = %node_id.fmt_short(),
                timeout = ?VIDEO_SEND_STOP_TIMEOUT,
                "video sender did not stop cooperatively; falling back to abort"
            );
            handle.abort();
            let _ = handle.await;
        }
    }
}

pub(super) async fn run_video_send(
    conn: RtcConnection,
    frame_tx: tokio::sync::broadcast::Sender<Arc<wire::video::transport::EncodedVideoFrame>>,
    keyframe_tx: tokio::sync::broadcast::Sender<()>,
    node_id: NodeId,
    mut stop_rx: tokio::sync::watch::Receiver<VideoSendCommand>,
) {
    let result: Result<()> = async {
        info!("opening video stream to {}", node_id.fmt_short());
        let (mut send, control_recv) = conn.transport().open_bi().await?;
        let _ = send.set_priority(10);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel();
        let control_task = tokio::spawn(read_video_controls(control_recv, control_tx));
        let mut rx = frame_tx.subscribe();
        let _ = keyframe_tx.send(());
        let mut subscribed = true;
        let mut control_open = true;
        let mut sent = 0u64;
        let mut skipped = 0u64;
        let mut resyncs = 0u64;
        let mut keyframe_gate = transport::KeyframeGate::waiting();
        let mut window_sent = 0u64;
        let mut window_bytes = 0u64;
        let mut window_send_ms = Vec::with_capacity(300);
        let mut last_stats_log = std::time::Instant::now();
        let mut stop_command = VideoSendCommand::Finish;
        loop {
            if !subscribed {
                tokio::select! {
                    changed = stop_rx.changed() => {
                        if changed.is_ok() {
                            stop_command = *stop_rx.borrow_and_update();
                        }
                        break;
                    }
                    control = control_rx.recv(), if control_open => match control {
                        Some(true) => {
                            subscribed = true;
                            keyframe_gate.require_keyframe();
                            let _ = keyframe_tx.send(());
                            last_stats_log = std::time::Instant::now();
                            info!(
                                "video subscriber {} resumed; waiting for a fresh keyframe",
                                node_id.fmt_short()
                            );
                        }
                        Some(false) => {}
                        None => break,
                    }
                }
                continue;
            }

            let frame = tokio::select! {
                changed = stop_rx.changed() => {
                    if changed.is_ok() {
                        stop_command = *stop_rx.borrow_and_update();
                    }
                    break;
                }
                control = control_rx.recv(), if control_open => match control {
                    Some(false) => {
                        subscribed = false;
                        info!(
                            "video subscriber {} paused; suspending frame transmission",
                            node_id.fmt_short()
                        );
                        continue;
                    }
                    Some(true) => continue,
                    None => {
                        // Older Wire clients drop the reverse half of the
                        // video stream because they do not implement
                        // subscription controls. Keep automatic viewing
                        // working for those peers.
                        control_open = false;
                        continue;
                    }
                },
                frame = rx.recv() => match frame {
                    Ok(frame) => frame,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        skipped += count;
                        // A broadcast lag means this sender missed encoded pictures, not
                        // that the QUIC stream itself is broken. Keep the existing stream
                        // and resume at an IDR: an IDR resets the receiver's H.264 reference
                        // chain without forcing it to recreate its decoder and GPU surfaces.
                        keyframe_gate.require_keyframe();
                        let _ = keyframe_tx.send(());
                        warn!(
                            "video send to {} lagged by {} frame(s); recovering on the existing stream at the next IDR",
                            node_id.fmt_short(),
                            count
                        );
                        continue;
                    }
                    Err(_) => break,
                }
            };
            let was_waiting = keyframe_gate.is_waiting();
            if !keyframe_gate.accept(&frame) {
                skipped += 1;
                continue;
            }
            if was_waiting {
                resyncs += 1;
            }
            let send_start = std::time::Instant::now();
            let send_result = tokio::select! {
                changed = stop_rx.changed() => {
                    if changed.is_ok() {
                        stop_command = *stop_rx.borrow_and_update();
                    }
                    break;
                }
                result = transport::send_frame(&mut send, frame.as_ref()) => result,
            };
            if let Err(e) = send_result {
                info!("video send to {} failed: {e:?}", node_id.fmt_short());
                break;
            }
            let send_elapsed = send_start.elapsed();
            window_sent += 1;
            window_bytes += frame.data.len() as u64;
            window_send_ms.push(send_elapsed.as_secs_f64() * 1000.0);
            let latency_budget = std::cmp::max(
                Duration::from_millis(250),
                conn.transport().rtt().saturating_mul(3),
            );
            if send_elapsed > latency_budget {
                // Congestion is recoverable in-band. Replacing a healthy QUIC stream
                // here used to create a decoder/presenter allocation storm on the peer
                // during sustained screen sharing.
                keyframe_gate.require_keyframe();
                let _ = keyframe_tx.send(());
                warn!(
                    "video send to {} took {:.0}ms for {} bytes (budget {:.0}ms); recovering at the next IDR without replacing the stream",
                    node_id.fmt_short(),
                    send_elapsed.as_secs_f64() * 1000.0,
                    frame.data.len(),
                    latency_budget.as_secs_f64() * 1000.0,
                );
            }
            sent += 1;
            if sent == 1 {
                info!(
                    "sent first video frame ({} bytes) to {}",
                    frame.data.len(),
                    node_id.fmt_short()
                );
            } else if send_elapsed > Duration::from_millis(75) {
                info!(
                    "video send to {} took {:.0}ms for {} bytes",
                    node_id.fmt_short(),
                    send_elapsed.as_secs_f64() * 1000.0,
                    frame.data.len()
                );
            }
            if last_stats_log.elapsed() >= Duration::from_secs(5) {
                let elapsed = last_stats_log.elapsed().as_secs_f64();
                let avg = window_send_ms.iter().sum::<f64>() / window_send_ms.len() as f64;
                window_send_ms.sort_by(f64::total_cmp);
                let p95 = window_send_ms
                    [((window_send_ms.len() - 1) as f64 * 0.95).round() as usize];
                info!(
                    "video send pipeline to {}: {:.1} fps, {:.1} Mbps, {:.1} ms avg / {:.1} ms p95, {} skipped, {} resyncs",
                    node_id.fmt_short(),
                    window_sent as f64 / elapsed,
                    window_bytes as f64 * 8.0 / elapsed / 1_000_000.0,
                    avg,
                    p95,
                    skipped,
                    resyncs
                );
                window_sent = 0;
                window_bytes = 0;
                window_send_ms.clear();
                last_stats_log = std::time::Instant::now();
            }
        }
        if stop_command.resets_stream() {
            let reset_result = send.reset(VIDEO_STREAM_RESET_CODE);
            info!(
                node = %node_id.fmt_short(),
                reset_code = "0x51",
                "resetting video send stream for replacement"
            );
            if reset_result.is_err() {
                warn!(
                    "video send reset for {} failed: {reset_result:?}",
                    node_id.fmt_short()
                );
            }
        } else {
            let finish_result = send.finish();
            info!(node = %node_id.fmt_short(), "finishing video send stream");
            if finish_result.is_err() {
                warn!(
                    "video send finish for {} failed: {finish_result:?}",
                    node_id.fmt_short()
                );
            }
        }
        control_task.abort();
        let _ = control_task.await;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        info!("video send to {} stopped: {e:?}", node_id.fmt_short());
    } else {
        info!("video send to {} ended", node_id.fmt_short());
    }
}

async fn read_video_controls(
    mut recv: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    control_tx: tokio::sync::mpsc::UnboundedSender<bool>,
) {
    loop {
        let mut value = [0u8; 1];
        match tokio::io::AsyncReadExt::read_exact(&mut recv, &mut value).await {
            Ok(_) if value[0] == VIDEO_CONTROL_PAUSE => {
                let _ = control_tx.send(false);
            }
            Ok(_) if value[0] == VIDEO_CONTROL_RESUME => {
                let _ = control_tx.send(true);
            }
            Ok(_) => {
                warn!("ignored unknown video subscriber control byte {}", value[0]);
            }
            Err(_) => break,
        }
    }
}

pub(super) async fn run_video_recv(
    conn: RtcConnection,
    node_id: NodeId,
    event_tx: EventPublisher,
    mut control_rx: tokio::sync::mpsc::UnboundedReceiver<VideoReceiveControl>,
) {
    const DECODER_IDLE_GRACE: Duration = Duration::from_secs(5);

    // Keep one decoder across brief share restarts. Media Foundation and the GPU
    // driver retain sizeable allocator caches when a decoder is destroyed and
    // immediately recreated, which makes repeated button presses look like a
    // leak even though every COM object is eventually released. A replacement
    // retains the last frame during this grace period, while an explicit stop
    // clears it immediately and only retains the decoder allocation.
    let mut worker = None;
    let mut decoder_idle_deadline: Option<tokio::time::Instant> = None;
    let mut next_generation = 0u64;
    let mut active_generation = None;
    let mut pending_end: Option<(u64, VideoStreamEndReason)> = None;
    loop {
        let accept = conn.transport().accept_bi();
        tokio::select! {
            closed = conn.transport().closed() => {
                match closed {
                    iroh::endpoint::ConnectionError::LocallyClosed => {}
                    err => info!(
                        "connection closed while waiting for video from {}: {err:?}",
                        node_id.fmt_short()
                    ),
                }
                break;
            }
            _ = wait_until_optional(decoder_idle_deadline) => {
                if let Some((generation, reason)) = pending_end.take() {
                    notify_video_stream_ended(&event_tx, node_id, generation, reason);
                    // Avoid a duplicate ConnectionClosed end for the same gen
                    // if the recv task later exits without a newer stream.
                    if active_generation == Some(generation) {
                        active_generation = None;
                    }
                }
                worker = None;
                decoder_idle_deadline = None;
                info!(
                    "released idle video decoder for {} after {:?}",
                    node_id.fmt_short(),
                    DECODER_IDLE_GRACE
                );
                #[cfg(target_os = "windows")]
                match trim_process_working_set() {
                    Ok(()) => info!(
                        "released inactive video receive pages for {}",
                        node_id.fmt_short()
                    ),
                    Err(error) => warn!(
                        "could not release inactive video receive pages for {}: {error:#}",
                        node_id.fmt_short()
                    ),
                }
            }
            stream = accept => {
                match stream {
                    Ok((mut control_send, mut recv)) => {
                        next_generation = next_generation
                            .checked_add(1)
                            .expect("video stream generation exhausted");
                        let generation = next_generation;
                        active_generation = Some(generation);
                        pending_end = None;
                        info!(
                            node = %node_id.fmt_short(),
                            generation,
                            "accepted video receive stream"
                        );
                        notify_video_stream_accepted(&event_tx, node_id, generation).await;
                        if worker.is_none() {
                            match spawn_video_decode_worker(node_id, event_tx.clone()) {
                                Ok(new_worker) => worker = Some(new_worker),
                                Err(error) => {
                                    warn!(
                                        "could not start video decoder for {}: {error:?}",
                                        node_id.fmt_short()
                                    );
                                    break;
                                }
                            }
                        }
                        let stream_result = recv_video_on_stream(
                            &mut recv,
                            node_id,
                            generation,
                            worker.as_ref().expect("video decoder was initialized"),
                            &mut control_send,
                            &mut control_rx,
                        )
                        .await;
                        match stream_result {
                            Ok(()) => {
                                info!(
                                    "video stream from {} generation {} ended cleanly; clearing the frame and retaining the decoder for {:?}",
                                    node_id.fmt_short(),
                                    generation,
                                    DECODER_IDLE_GRACE
                                );
                                notify_video_stream_ended(
                                    &event_tx,
                                    node_id,
                                    generation,
                                    VideoStreamEndReason::CleanEof,
                                );
                                active_generation = None;
                                pending_end = None;
                                decoder_idle_deadline = Some(tokio::time::Instant::now() + DECODER_IDLE_GRACE);
                            }
                            Err(error) if is_video_stream_replacement_error(&error) => {
                                info!(
                                    "video stream from {} generation {} was replaced at a frame boundary; waiting for the resync stream",
                                    node_id.fmt_short(),
                                    generation
                                );
                                pending_end =
                                    Some((generation, VideoStreamEndReason::ReplacementIdle));
                                decoder_idle_deadline = Some(tokio::time::Instant::now() + DECODER_IDLE_GRACE);
                            }
                            Err(error) => {
                                warn!(
                                    "video stream from {} generation {} failed: {error:?}; clearing the frame while waiting for a replacement stream",
                                    node_id.fmt_short(),
                                    generation
                                );
                                notify_video_stream_ended(
                                    &event_tx,
                                    node_id,
                                    generation,
                                    VideoStreamEndReason::ReceiveError,
                                );
                                active_generation = None;
                                pending_end = None;
                                decoder_idle_deadline = Some(tokio::time::Instant::now() + DECODER_IDLE_GRACE);
                            }
                        }
                    }
                    Err(e) => {
                        info!("video accept_bi from {} failed: {e:?}", node_id.fmt_short());
                        break;
                    }
                }
            }
        }
    }
    if let Some((generation, reason)) = pending_end {
        notify_video_stream_ended(&event_tx, node_id, generation, reason);
    } else if let Some(generation) = active_generation {
        notify_video_stream_ended(
            &event_tx,
            node_id,
            generation,
            VideoStreamEndReason::ConnectionClosed,
        );
    }
}

async fn wait_until_optional(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

fn spawn_video_decode_worker(
    node_id: NodeId,
    event_tx: EventPublisher,
) -> Result<VideoDecodeWorker> {
    VideoDecodeWorker::spawn(move |frame, generation| {
        event_tx.try_send(Event::VideoFrame {
            node_id,
            generation,
            frame,
        });
    })
}

async fn notify_video_stream_accepted(event_tx: &EventPublisher, node_id: NodeId, generation: u64) {
    event_tx
        .send(Event::VideoStreamAccepted {
            node_id,
            generation,
        })
        .await;
}

fn notify_video_stream_ended(
    event_tx: &EventPublisher,
    node_id: NodeId,
    generation: u64,
    reason: VideoStreamEndReason,
) {
    info!(
        node = %node_id.fmt_short(),
        generation,
        reason = ?reason,
        "video receive stream ended"
    );
    event_tx.try_send(Event::VideoStreamEnded {
        node_id,
        generation,
        reason,
    });
}

fn is_video_stream_replacement_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("stream reset by peer")
        && (message.contains("error 81") || message.contains("error 0x51"))
}

async fn recv_video_on_stream(
    recv: &mut (impl tokio::io::AsyncRead + Unpin),
    node_id: NodeId,
    generation: u64,
    worker: &VideoDecodeWorker,
    control_send: &mut (impl tokio::io::AsyncWrite + Unpin),
    control_rx: &mut tokio::sync::mpsc::UnboundedReceiver<VideoReceiveControl>,
) -> Result<()> {
    let mut received = 0u64;
    let mut received_bytes = 0u64;
    let mut received_age_ms = 0.0;
    let mut max_age_ms = 0.0;
    let mut age_samples = Vec::with_capacity(300);
    let mut last_sequence: Option<u64> = None;
    let mut last_stats_log = std::time::Instant::now();
    let mut watching = true;
    let mut waiting_for_keyframe = false;

    loop {
        let frame = loop {
            tokio::select! {
                frame = transport::recv_frame(&mut *recv) => break frame,
                control = control_rx.recv() => {
                    let Some(control) = control else {
                        continue;
                    };
                    if control.generation != generation || control.watching == watching {
                        continue;
                    }
                    let value = if control.watching {
                        VIDEO_CONTROL_RESUME
                    } else {
                        VIDEO_CONTROL_PAUSE
                    };
                    tokio::io::AsyncWriteExt::write_all(control_send, &[value]).await?;
                    watching = control.watching;
                    waiting_for_keyframe = watching;
                    info!(
                        node = %node_id.fmt_short(),
                        generation,
                        watching,
                        "updated video subscription"
                    );
                }
            }
        };
        match frame {
            Ok(Some(frame)) => {
                if !watching {
                    continue;
                }
                if waiting_for_keyframe {
                    if !frame.keyframe {
                        continue;
                    }
                    waiting_for_keyframe = false;
                }
                if let Some(previous) = last_sequence {
                    let expected = previous.wrapping_add(1);
                    if frame.sequence != expected {
                        warn!(
                            "video sequence gap from {}: expected {}, received {} (keyframe={})",
                            node_id.fmt_short(),
                            expected,
                            frame.sequence,
                            frame.keyframe
                        );
                    }
                }
                last_sequence = Some(frame.sequence);
                received += 1;
                if received == 1 {
                    info!(
                        node = %node_id.fmt_short(),
                        generation,
                        "received first encoded video frame"
                    );
                }
                received_bytes += frame.data.len() as u64;
                if let Some(age_ms) = transport::frame_age_ms(frame.sent_at_micros) {
                    received_age_ms += age_ms;
                    max_age_ms = f64::max(max_age_ms, age_ms);
                    age_samples.push(age_ms);
                }
                if last_stats_log.elapsed() >= Duration::from_secs(5) {
                    let elapsed = last_stats_log.elapsed().as_secs_f64();
                    let recv_fps = if elapsed > 0.0 {
                        received as f64 / elapsed
                    } else {
                        0.0
                    };
                    let avg_packet_kb = if received > 0 {
                        received_bytes as f64 / received as f64 / 1024.0
                    } else {
                        0.0
                    };
                    let avg_age_ms = if received > 0 {
                        received_age_ms / received as f64
                    } else {
                        0.0
                    };
                    age_samples.sort_by(f64::total_cmp);
                    let p95_age_ms = if age_samples.is_empty() {
                        0.0
                    } else {
                        age_samples[((age_samples.len() - 1) as f64 * 0.95).round() as usize]
                    };
                    info!(
                        "video receive pipeline from {}: {:.1} fps, {:.1} KiB/frame, {:.0}ms avg / {:.0}ms p95 / {:.0}ms max age",
                        node_id.fmt_short(),
                        recv_fps,
                        avg_packet_kb,
                        avg_age_ms,
                        p95_age_ms,
                        max_age_ms
                    );
                    last_stats_log = std::time::Instant::now();
                    received = 0;
                    received_bytes = 0;
                    received_age_ms = 0.0;
                    max_age_ms = 0.0;
                    age_samples.clear();
                }
                worker.submit(
                    frame.data,
                    frame.keyframe,
                    generation,
                    u32::from(frame.height),
                    u32::from(frame.fps),
                );
            }
            Ok(None) => break,
            Err(e) => {
                return Err(e);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_the_reserved_video_replacement_reset() {
        assert!(is_video_stream_replacement_error(&anyhow::anyhow!(
            "stream reset by peer: error 81"
        )));
        assert!(!is_video_stream_replacement_error(&anyhow::anyhow!(
            "stream reset by peer: error 12"
        )));
        assert!(!is_video_stream_replacement_error(&anyhow::anyhow!(
            "invalid video frame length: 81 bytes"
        )));
    }

    #[test]
    fn only_replacement_stops_reset_the_video_stream() {
        assert!(VideoSendCommand::Replace.resets_stream());
        assert!(!VideoSendCommand::Finish.resets_stream());
        assert!(!VideoSendCommand::Running.resets_stream());
    }

    #[tokio::test]
    async fn video_subscription_controls_pause_and_resume_independently() {
        let (mut writer, reader) = tokio::io::duplex(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(read_video_controls(reader, control_tx));

        tokio::io::AsyncWriteExt::write_all(
            &mut writer,
            &[VIDEO_CONTROL_PAUSE, VIDEO_CONTROL_RESUME],
        )
        .await
        .unwrap();

        assert_eq!(control_rx.recv().await, Some(false));
        assert_eq!(control_rx.recv().await, Some(true));
        drop(writer);
        task.await.unwrap();
    }
}
