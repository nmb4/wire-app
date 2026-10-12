pub mod audio;
pub mod codec;
pub mod net;
pub mod remote_logs;
pub mod rtc;
pub mod video;

pub use cpal;
pub use iroh::NodeId;

#[cfg(test)]
mod tests {
    use std::{
        ops::ControlFlow,
        time::{Duration, Instant},
    };

    use futures_concurrency::future::TryJoin;
    use iroh::protocol::Router;
    use testresult::TestResult;
    use tokio::sync::mpsc;

    use crate::{
        audio::{AudioSink, AudioSource, ENGINE_FORMAT},
        codec::opus::{AudioQuality, MediaTrackOpusDecoder, MediaTrackOpusEncoder},
        net::generate_ephemeral_secret_key,
        rtc::RtcProtocol,
    };

    async fn build() -> TestResult<(Router, RtcProtocol)> {
        let endpoint = iroh::Endpoint::builder()
            .secret_key(generate_ephemeral_secret_key())
            .discovery_n0()
            .alpns(vec![RtcProtocol::ALPN.to_vec()])
            .bind()
            .await?;
        let proto = RtcProtocol::new(endpoint.clone());
        let router = Router::builder(endpoint)
            .accept(RtcProtocol::ALPN, proto.clone())
            .spawn()
            .await?;
        Ok((router, proto))
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn smoke() -> TestResult {
        let (router1, rtc1) = build().await?;
        let (router2, rtc2) = build().await?;
        let addr1 = router1.endpoint().node_addr().await?;

        let (conn1, conn2) = (rtc2.connect(addr1), rtc1.accept()).try_join().await?;

        let conn2 = conn2.unwrap();

        let (mut node1, track1) =
            MediaTrackOpusEncoder::new(4, ENGINE_FORMAT, AudioQuality::Ultra)?;
        conn1.send_track(track1.clone()).await?;

        let sample_count = ENGINE_FORMAT.sample_count(Duration::from_millis(20));
        // start sending audio at node1
        let (abort_tx, mut abort_rx) = mpsc::channel(1);
        let send_task = tokio::task::spawn(async move {
            println!("loop start");
            let fut = async move {
                loop {
                    #[allow(clippy::question_mark)]
                    if let Err(err) = node1.tick(&vec![0.5; sample_count]) {
                        return Err(err);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            };
            tokio::select! {
                x = abort_rx.recv() => x.unwrap(),
                x = fut => x.unwrap(),
            }
            println!("loop end");
            conn1.transport().close(1u32.into(), b"bye");
            tokio::time::sleep(Duration::from_millis(20)).await;
            anyhow::Ok(())
        });
        let track2 = conn2.recv_track().await?.unwrap();

        assert_eq!(track1.codec(), track2.codec());

        let mut decoder = MediaTrackOpusDecoder::new(track2)?;
        let mut out = vec![0.; sample_count];
        // we need to wait a bit likely.
        let start = Instant::now();
        // wait for some audio to arrive.
        let expected = sample_count * 3;
        let mut total = 0;
        'outer: loop {
            let n = loop {
                if start.elapsed() > Duration::from_secs(2) {
                    panic!("timeout");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                match decoder.tick(&mut out)? {
                    ControlFlow::Continue(0) => continue,
                    ControlFlow::Continue(n) => break n,
                    // this signals end of track, triggered when the connection closes.
                    ControlFlow::Break(()) => break 'outer,
                }
            };
            assert!(out[..n].iter().any(|s| *s != 0.));
            out.fill(0.);
            total += n;
            if total >= expected {
                abort_tx.try_send(()).ok();
            }
            println!("received {n} audio frames, total {total}");
        }
        assert_eq!(total, expected);
        send_task.await??;
        router1.shutdown().await?;
        router2.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn duplex_call_delivers_audible_20ms_packets_at_every_quality() -> TestResult {
        let (router_a, rtc_a) = build().await?;
        let (router_b, rtc_b) = build().await?;
        let (conn_a, conn_b) = (
            rtc_a.connect(router_b.endpoint().node_addr().await?),
            rtc_b.accept(),
        )
            .try_join()
            .await?;
        let conn_b = conn_b.unwrap();

        for quality in [
            AudioQuality::Low,
            AudioQuality::Medium,
            AudioQuality::High,
            AudioQuality::Ultra,
        ] {
            let (mut encoder_a, track_a) = MediaTrackOpusEncoder::new(16, ENGINE_FORMAT, quality)?;
            let (mut encoder_b, track_b) = MediaTrackOpusEncoder::new(16, ENGINE_FORMAT, quality)?;
            conn_a.send_track(track_a).await?;
            conn_b.send_track(track_b).await?;
            let sender = tokio::spawn(async move {
                for tick in 0..60 {
                    let samples: Vec<_> = (0..960)
                        .flat_map(|sample| {
                            let value =
                                ((tick * 960 + sample) as f32 * std::f32::consts::TAU * 440.0
                                    / 48_000.0)
                                    .sin()
                                    * 0.25;
                            [value, value]
                        })
                        .collect();
                    assert!(matches!(
                        encoder_a.tick(&samples)?,
                        ControlFlow::Continue(())
                    ));
                    assert!(matches!(
                        encoder_b.tick(&samples)?,
                        ControlFlow::Continue(())
                    ));
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                anyhow::Ok(())
            });
            let received = async {
                let (track_a, track_b) = (conn_a.recv_track(), conn_b.recv_track())
                    .try_join()
                    .await?;
                async fn check(
                    mut track: crate::rtc::MediaTrack,
                    quality: AudioQuality,
                ) -> anyhow::Result<()> {
                    let mut decoder = opus::Decoder::new(48_000, quality.channels().into())?;
                    let mut output = vec![0.0; 960 * quality.channels() as usize];
                    let mut audible = 0;
                    for _ in 0..30 {
                        let frame = track.recv().await?;
                        assert_eq!(
                            frame.sample_count,
                            Some(960),
                            "{quality}: received incorrect duration"
                        );
                        assert_eq!(
                            decoder.decode_float(&frame.payload, &mut output, false)?,
                            960
                        );
                        if output.iter().any(|sample| sample.abs() > 0.05) {
                            audible += 1;
                        }
                    }
                    assert!(audible >= 28, "{quality}: silent remote audio");
                    Ok(())
                }
                (
                    check(track_a.unwrap(), quality),
                    check(track_b.unwrap(), quality),
                )
                    .try_join()
                    .await?;
                anyhow::Ok(())
            };
            tokio::time::timeout(Duration::from_secs(5), received).await??;
            sender.await??;
        }
        conn_a.transport().close(0u32.into(), b"test complete");
        router_a.shutdown().await?;
        router_b.shutdown().await?;
        Ok(())
    }
}
