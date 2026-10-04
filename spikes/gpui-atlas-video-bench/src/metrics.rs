//! Timing + frame-accounting schema for the atlas video bench.
//!
//! Field set mirrors `66HEX/frame`'s `preview_engine/metrics.rs` shape (per-stage
//! microsecond timings, generations, drops) so the numbers are comparable to a
//! real implementation rather than a one-off. Reimplemented from the field list,
//! not copied.

use std::time::Instant;

/// Bytes for one BGRA frame: width * height * 4.
/// 1920x1080 -> 8_294_400 (7.91 MiB).
pub fn bytes_per_frame(width: u32, height: u32) -> u64 {
    width as u64 * height as u64 * 4
}

/// Generation accounting for the latest-frame store.
///
/// The publisher always overwrites the single latest slot; the painter presents
/// whatever is latest. A generation that is published but never painted before
/// being superseded counts as `overwritten_before_present`. `dropped` is
/// derived as `published - presented`.
#[derive(Default, Debug)]
pub struct FrameStore {
    next_generation: u64,
    latest_published: u64,
    latest_presented: u64,
    pub published: u64,
    pub presented: u64,
    pub overwritten_before_present: u64,
}

impl FrameStore {
    pub fn publish(&mut self) -> u64 {
        if self.latest_presented < self.latest_published {
            self.overwritten_before_present += self.latest_published - self.latest_presented;
        }
        self.next_generation += 1;
        self.latest_published = self.next_generation;
        self.published += 1;
        self.latest_published
    }

    /// Returns true when this paint advanced the presented generation (i.e. it
    /// painted new content, not a re-paint of an already-presented frame).
    pub fn mark_presented(&mut self, generation: u64) -> bool {
        if generation > self.latest_presented {
            self.latest_presented = generation;
            self.presented += 1;
            true
        } else {
            false
        }
    }
}

/// In-process metrics shared between the publish path (UI thread) and paint.
///
/// `on_published` returns the generation the painter must present; the paint
/// closure reports back via `on_presented`, so re-paints of an unchanged frame
/// (window expose, resize) do not inflate the frame counts.
#[derive(Debug, Default)]
pub struct Metrics {
    pub store: FrameStore,
    /// Alloc + fill + wrap cost per published frame.
    pub build_us: Vec<u64>,
    /// `paint_image` wall time per presented frame (dominated by the atlas
    /// `get_or_update_with` upload for a full-window 1080p image).
    pub upload_us: Vec<u64>,
    /// Timestamps of publishes that survived warmup (for publish-side pacing).
    pub published_at: Vec<Instant>,
    /// Timestamps of paints that presented a new generation.
    pub presented_at: Vec<Instant>,
    warmup_publishes_to_skip: u64,
    warmup_presents_to_skip: u64,
}

impl Metrics {
    pub fn with_warmup(skip_frames: u64) -> Self {
        Self {
            warmup_publishes_to_skip: skip_frames,
            warmup_presents_to_skip: skip_frames,
            ..Default::default()
        }
    }

    pub fn on_published(&mut self, build_us: u64) -> u64 {
        let generation = self.store.publish();
        if self.warmup_publishes_to_skip > 0 {
            self.warmup_publishes_to_skip -= 1;
            return generation;
        }
        self.build_us.push(build_us);
        self.published_at.push(Instant::now());
        generation
    }

    pub fn on_presented(&mut self, generation: u64, upload_us: u64) {
        if !self.store.mark_presented(generation) {
            return;
        }
        if self.warmup_presents_to_skip > 0 {
            self.warmup_presents_to_skip -= 1;
            return;
        }
        self.upload_us.push(upload_us);
        self.presented_at.push(Instant::now());
    }
}

fn avg(samples: &[u64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.iter().sum::<u64>() as f64 / samples.len() as f64
}

/// p95 with the same index rule Wire uses in `log_stats` /
/// `sync_rgba_texture`: `samples[(len - 1) * 0.95 round]`.
fn p95(mut samples: Vec<u64>) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_unstable();
    samples[((samples.len() - 1) as f64 * 0.95).round() as usize] as f64
}

/// Mean and p95 of consecutive-present intervals in microseconds.
fn interval_stats(presented_at: &[Instant]) -> (f64, f64) {
    if presented_at.len() < 2 {
        return (0.0, 0.0);
    }
    let mut intervals: Vec<u64> = presented_at
        .windows(2)
        .map(|w| w[1].duration_since(w[0]).as_micros() as u64)
        .collect();
    let mean = avg(&intervals);
    intervals.sort_unstable();
    let p95 = intervals[((intervals.len() - 1) as f64 * 0.95).round() as usize] as f64;
    (mean, p95)
}

pub fn print_report(
    mode: &str,
    target_fps: u32,
    seconds: u32,
    width: u32,
    height: u32,
    gpui_rev: &str,
    metrics: &Metrics,
) {
    let store = &metrics.store;
    let bpf = bytes_per_frame(width, height);
    let presented = store.presented;
    let total_bytes = presented * bpf;
    let elapsed_s = metrics
        .presented_at
        .first()
        .zip(metrics.presented_at.last())
        .map(|(a, b)| b.duration_since(*a).as_secs_f64())
        .unwrap_or(0.0);
    // fps over the post-warmup window only (presented_at excludes warmup).
    let post_warmup_presented = metrics.presented_at.len() as u64;
    let fps_avg = if elapsed_s > 0.0 {
        post_warmup_presented as f64 / elapsed_s
    } else {
        0.0
    };
    let (interval_avg_us, interval_p95_us) = interval_stats(&metrics.presented_at);
    let (publish_avg_us, publish_p95_us) = interval_stats(&metrics.published_at);
    let dropped = store.published.saturating_sub(store.presented);
    // MiB/s actually re-uploaded into the atlas (post-warmup presents).
    let total_bytes_post_warmup = post_warmup_presented * bpf;
    let mb_per_s = if elapsed_s > 0.0 {
        total_bytes_post_warmup as f64 / elapsed_s / (1024.0 * 1024.0)
    } else {
        0.0
    };
    println!(
        "{{\n  \"mode\": \"{mode}\",\n  \"gpui_rev\": \"{gpui_rev}\",\n  \"target_fps\": {target_fps},\n  \"seconds\": {seconds},\n  \"width\": {width},\n  \"height\": {height},\n  \"bytes_per_frame\": {bpf},\n  \"frames_published\": {},\n  \"frames_presented\": {presented},\n  \"frames_dropped\": {dropped},\n  \"frames_overwritten_before_present\": {},\n  \"total_bytes_uploaded\": {total_bytes},\n  \"upload_mib_per_s\": {mb_per_s:.1},\n  \"fps_avg\": {fps_avg:.2},\n  \"frame_interval_us_avg\": {interval_avg_us:.1},\n  \"frame_interval_us_p95\": {interval_p95_us:.1},\n  \"publish_interval_us_avg\": {:.1},\n  \"publish_interval_us_p95\": {:.1},\n  \"build_render_image_us_avg\": {:.1},\n  \"build_render_image_us_p95\": {:.1},\n  \"upload_us_avg\": {:.1},\n  \"upload_us_p95\": {:.1}\n}}",
        store.published,
        store.overwritten_before_present,
        publish_avg_us,
        publish_p95_us,
        avg(&metrics.build_us),
        p95(metrics.build_us.clone()),
        avg(&metrics.upload_us),
        p95(metrics.upload_us.clone()),
    );
}
