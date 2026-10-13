use std::path::PathBuf;

use clap::Parser;
use dialoguer::Confirm;
use iroh::protocol::Router;
use tokio::task::JoinSet;
use tracing::{error, info, warn};
use wire::{
    audio::{AudioConfig, AudioContext, AudioQuality},
    net, remote_logs,
    rtc::{handle_connection_with_audio_context, RtcConnection, RtcProtocol},
    NodeId,
};

#[derive(Parser, Debug)]
#[command(about = "Wire CLI for iroh voice calls", long_about = None)]
struct Args {
    /// The audio input device to use.
    #[arg(short, long)]
    input_device: Option<String>,
    /// The audio output device to use.
    #[arg(short, long)]
    output_device: Option<String>,
    /// If set, audio processing and echo cancellation will be disabled.
    #[arg(long)]
    disable_processing: bool,
    /// If set, RNNoise microphone noise suppression will be disabled.
    #[arg(long)]
    disable_noise_suppression: bool,
    /// Audio quality preset (low, medium, high, ultra).
    #[arg(long, default_value = "high", value_parser = |s: &str| -> Result<AudioQuality, String> { s.parse() })]
    quality: AudioQuality,
    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
enum Command {
    /// Accept calls from remote nodes.
    Accept {
        /// Accept more than one call.
        #[clap(long)]
        many: bool,
        /// Auto-accept calls without confirmation.
        #[clap(long)]
        auto: bool,
    },
    /// Make calls to remote nodes.
    Connect { node_id: Vec<NodeId> },
    /// Create a debug feedback loop through an in-memory channel.
    Feedback { mode: Option<FeedbackMode> },
    /// List the available audio devices
    ListDevices,
    /// Fetch a GUI log file from a running Wire client (no UI prompt).
    FetchLogs {
        /// Target Wire node id.
        node_id: NodeId,
        /// Where to write the log (default: the remote file name, or
        /// ./wire-remote-<short-id>.log when fetching the latest).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Max bytes to pull from the tail of the remote log (default 8 MiB).
        #[arg(long, default_value_t = 8 * 1024 * 1024)]
        max_bytes: u64,
        /// Fetch one specific remote log by bare file name (e.g.
        /// wire-app-1234.log). Use this to retrieve the log of a session that
        /// has already ended, which `--list` can show you.
        #[arg(long, value_name = "FILE")]
        name: Option<String>,
        /// List the log files the remote client can serve, then exit.
        #[arg(long)]
        list: bool,
    },
}

#[derive(Debug, Clone, clap::ValueEnum, Default)]
enum FeedbackMode {
    #[default]
    Raw,
    Encoded,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let audio_config = AudioConfig {
        input_device: args.input_device,
        output_device: args.output_device,
        processing_enabled: !args.disable_processing,
        noise_suppression_enabled: !args.disable_noise_suppression,
        quality: args.quality,
    };
    let mut endpoint_shutdown = None;
    let fut = async {
        match args.command {
            Command::Accept { many, auto } => {
                let endpoint = net::bind_endpoint().await?;
                let proto = RtcProtocol::new(endpoint.clone());
                let _router = Router::builder(endpoint.clone())
                    .accept(RtcProtocol::ALPN, proto.clone())
                    .spawn()
                    .await?;

                endpoint_shutdown = Some(endpoint.clone());
                println!("our node id:\n{}", endpoint.node_id());

                let audio_ctx = AudioContext::new(audio_config).await?;

                while let Some(conn) = proto.accept().await? {
                    if !many {
                        handle_connection(audio_ctx, conn).await;
                        break;
                    } else {
                        let peer = conn.transport().remote_node_id()?.fmt_short();
                        let accept =
                            auto || confirm(format!("Incoming call from {peer}. Accept?")).await;
                        if accept {
                            n0_future::task::spawn(handle_connection(audio_ctx.clone(), conn));
                        } else {
                            info!("reject connection from {peer}");
                            conn.transport().close(0u32.into(), b"bye");
                        }
                    }
                }
            }
            Command::Connect { node_id } => {
                let endpoint = net::bind_endpoint().await?;
                endpoint_shutdown = Some(endpoint.clone());

                let proto = RtcProtocol::new(endpoint);
                let audio_ctx = AudioContext::new(audio_config).await?;

                let mut join_set = JoinSet::new();

                for node_id in node_id {
                    info!("connecting to {}", node_id.fmt_short());
                    let audio_ctx = audio_ctx.clone();
                    let proto = proto.clone();
                    join_set.spawn(async move {
                        let fut = async {
                            let conn = proto.connect(node_id).await?;
                            info!("established connection to {}", node_id.fmt_short());
                            handle_connection(audio_ctx, conn).await;
                            anyhow::Ok(())
                        };
                        (node_id, fut.await)
                    });
                }

                while let Some(res) = join_set.join_next().await {
                    let (node_id, res) = match res {
                        Ok(joined) => joined,
                        Err(err) => {
                            warn!("connection task failed to join: {err:?}");
                            continue;
                        }
                    };
                    if let Err(err) = res {
                        warn!("failed to connect to {}: {err:?}", node_id.fmt_short())
                    }
                }
            }
            Command::Feedback { mode } => {
                let ctx = AudioContext::new(audio_config).await?;
                let mode = mode.unwrap_or_default();
                println!("start feedback loop for 5 seconds (mode {mode:?}");
                match mode {
                    FeedbackMode::Raw => ctx.feedback_raw().await?,
                    FeedbackMode::Encoded => ctx.feedback_encoded().await?,
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                println!("closing");
            }
            Command::ListDevices => {
                let devices = AudioContext::list_devices().await?;
                println!("{devices:?}");
            }
            Command::FetchLogs {
                node_id,
                output,
                max_bytes,
                name,
                list,
            } => {
                if list && name.is_some() {
                    anyhow::bail!("--list and --name are mutually exclusive");
                }
                let endpoint =
                    net::bind_endpoint_with_alpns([remote_logs::LOGS_ALPN.to_vec()]).await?;
                endpoint_shutdown = Some(endpoint.clone());

                if list {
                    info!(target = %node_id.fmt_short(), "listing remote Wire logs");
                    let meta = remote_logs::list_logs(&endpoint, node_id).await?;
                    println!(
                        "remote node={} pid={} — {} log file(s) available",
                        meta.node_id,
                        meta.pid,
                        meta.files.len()
                    );
                    let now_ms = unix_millis();
                    for file in &meta.files {
                        let marker = if file.is_current_process {
                            " (current)"
                        } else {
                            ""
                        };
                        let pid = file
                            .pid
                            .map(|pid| format!("pid={pid:<8}"))
                            .unwrap_or_else(|| "pid=?       ".to_owned());
                        println!(
                            "{pid} {:>10}  {:<24}  {}  {}{marker}",
                            format_bytes(file.byte_len),
                            file.file_name,
                            format_utc(file.modified_ms),
                            format_age(now_ms.saturating_sub(file.modified_ms)),
                        );
                    }
                    return anyhow::Ok(());
                }

                let requested = name.clone();
                info!(
                    target = %node_id.fmt_short(),
                    max_bytes,
                    file = requested.as_deref().unwrap_or("<latest>"),
                    "fetching remote Wire logs"
                );
                let fetched = match &requested {
                    Some(file_name) => {
                        remote_logs::fetch_named_logs(&endpoint, node_id, file_name, max_bytes)
                            .await?
                    }
                    None => remote_logs::fetch_latest_logs(&endpoint, node_id, max_bytes).await?,
                };
                let output = output.unwrap_or_else(|| match &requested {
                    // A named fetch writes to the remote's own file name, so
                    // repeated pulls of different sessions do not overwrite
                    // each other in the current directory.
                    Some(file_name) => PathBuf::from(file_name),
                    None => PathBuf::from(format!("wire-remote-{}.log", node_id.fmt_short())),
                });
                std::fs::write(&output, &fetched.bytes)?;
                println!(
                    "wrote {} bytes to {}",
                    fetched.bytes.len(),
                    output.display()
                );
                if let Some(name) = &fetched.meta.file_name {
                    println!("remote file: {name}");
                }
                if let Some(path) = &fetched.meta.path {
                    println!("remote path: {path}");
                }
                println!(
                    "remote node={} pid={} total_file_bytes={} truncated={}",
                    fetched.meta.node_id,
                    fetched.meta.pid,
                    fetched.meta.total_file_bytes,
                    fetched.meta.truncated
                );
            }
        }
        anyhow::Ok(())
    };

    tokio::select! {
        res = fut => res?,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            if let Some(endpoint) = endpoint_shutdown {
                endpoint.close().await;
            }
        }
    }
    Ok(())
}

async fn handle_connection(audio_ctx: AudioContext, conn: RtcConnection) {
    let peer = match conn.transport().remote_node_id() {
        Ok(id) => id.fmt_short().to_string(),
        Err(err) => {
            warn!("could not determine the peer for an incoming connection: {err:?}");
            "unknown peer".to_owned()
        }
    };
    if let Err(err) = handle_connection_with_audio_context(audio_ctx, conn).await {
        error!("connection from {peer} closed with error: {err:?}",)
    } else {
        info!("connection from {peer} closed")
    }
}

async fn confirm(msg: String) -> bool {
    match tokio::task::spawn_blocking(move || Confirm::new().with_prompt(msg).interact()).await {
        Ok(Ok(confirmed)) => confirmed,
        Ok(Err(err)) => {
            warn!("confirmation prompt failed: {err}");
            false
        }
        Err(err) => {
            warn!("confirmation prompt task failed: {err}");
            false
        }
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |delta| delta.as_millis() as u64)
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KIB * KIB {
        format!("{:.1} MiB", value / (KIB * KIB))
    } else if value >= KIB {
        format!("{:.0} KiB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Coarse age string. The exact timestamp is printed alongside it, so this only
/// has to be good enough to pick the right file out of a long list.
fn format_age(delta_ms: u64) -> String {
    const MINUTE: u64 = 60_000;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    if delta_ms < MINUTE {
        format!("{}s ago", delta_ms / 1000)
    } else if delta_ms < HOUR {
        format!("{}m ago", delta_ms / MINUTE)
    } else if delta_ms < DAY {
        format!("{}h ago", delta_ms / HOUR)
    } else {
        format!("{}d ago", delta_ms / DAY)
    }
}

/// ISO-8601 UTC timestamp, without pulling in a date-time dependency.
fn format_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian calendar date.
///
/// Howard Hinnant's `civil_from_days`, shifted so the epoch lands on
/// 1970-01-01 rather than 0000-03-01.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}
