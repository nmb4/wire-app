//! Probe MF H.264 encoder init without launching the GUI.
#![cfg(windows)]

use std::time::Instant;

use wire::video::{VideoConfig, VideoResolution};
use wire_app::win_mf_codec::MfH264Encoder;
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("info,wire_app=debug"))
        .init();

    let config = VideoConfig {
        resolution: VideoResolution::P1080,
        framerate: 60,
        bitrate_bps: None,
        sharing_enabled: true,
    };

    let start = Instant::now();
    let mut enc = MfH264Encoder::try_new(&config)?;
    println!(
        "encoder ready in {:.1}ms (hardware={})",
        start.elapsed().as_secs_f64() * 1000.0,
        enc.is_hardware()
    );

    let frame = vec![0x80u8; (1920 * 1080 * 4) as usize];
    let mut times = Vec::new();
    for i in 0..120 {
        let t0 = Instant::now();
        let out = enc.encode_bgra(&frame)?;
        times.push(t0.elapsed());
        if i < 10 || i % 30 == 29 {
            println!(
                "frame {i}: {} bytes in {:.2}ms",
                out.len(),
                times[i].as_secs_f64() * 1000.0
            );
        }
    }
    let avg_ms = times.iter().map(|d| d.as_secs_f64()).sum::<f64>() / times.len() as f64 * 1000.0;
    println!("avg encode: {avg_ms:.2}ms");
    Ok(())
}