use std::time::Duration;

use wire::audio::{AudioConfig, AudioContext};

/// Prints the normalized capture level ten times per second for five seconds,
/// so microphone capture health can be verified numerically.
///
/// Usage: cargo run -p wire --example mic-level-probe [INPUT_DEVICE]
/// Omit the argument to use the system default input device.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let input_device = std::env::args().nth(1);
    let config = AudioConfig {
        input_device,
        ..AudioConfig::default()
    };
    let ctx = AudioContext::new(config).await?;
    let level = ctx.capture_level();
    for i in 0..50 {
        let v = f32::from_bits(level.load(std::sync::atomic::Ordering::Relaxed));
        println!("t={:.1}s level={v:.4}", i as f32 / 10.0);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}
