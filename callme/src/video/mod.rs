pub mod bitstream;
pub mod codec;
pub mod transport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoResolution {
    P720,
    P1080,
    P1440,
}

impl VideoResolution {
    pub fn width(&self) -> u32 {
        match self {
            VideoResolution::P720 => 1280,
            VideoResolution::P1080 => 1920,
            VideoResolution::P1440 => 2560,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            VideoResolution::P720 => 720,
            VideoResolution::P1080 => 1080,
            VideoResolution::P1440 => 1440,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            VideoResolution::P720 => "720p",
            VideoResolution::P1080 => "1080p",
            VideoResolution::P1440 => "1440p",
        }
    }

    pub fn pixels(&self) -> u32 {
        self.width() * self.height()
    }

    pub fn all() -> &'static [VideoResolution] {
        &[VideoResolution::P720, VideoResolution::P1080, VideoResolution::P1440]
    }
}

/// Suggested bitrate in bits per second for screen content at the given resolution.
pub fn default_bitrate(resolution: VideoResolution, framerate: u32) -> u32 {
    let base = match resolution {
        VideoResolution::P720 => 5_000_000,
        VideoResolution::P1080 => 12_000_000,
        VideoResolution::P1440 => 20_000_000,
    };
    (base as f64 * (framerate as f64 / 30.0)) as u32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitratePreset {
    Auto,
    Mbps4,
    Mbps6,
    Mbps8,
    Mbps12,
    Mbps16,
    Mbps24,
    Mbps32,
}

impl BitratePreset {
    pub fn all() -> &'static [BitratePreset] {
        &[
            BitratePreset::Auto,
            BitratePreset::Mbps4,
            BitratePreset::Mbps6,
            BitratePreset::Mbps8,
            BitratePreset::Mbps12,
            BitratePreset::Mbps16,
            BitratePreset::Mbps24,
            BitratePreset::Mbps32,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            BitratePreset::Auto => "Auto (match resolution)",
            BitratePreset::Mbps4 => "4 Mbps (fastest encode)",
            BitratePreset::Mbps6 => "6 Mbps",
            BitratePreset::Mbps8 => "8 Mbps",
            BitratePreset::Mbps12 => "12 Mbps",
            BitratePreset::Mbps16 => "16 Mbps",
            BitratePreset::Mbps24 => "24 Mbps",
            BitratePreset::Mbps32 => "32 Mbps (max quality)",
        }
    }

    pub fn bps(self) -> Option<u32> {
        match self {
            BitratePreset::Auto => None,
            BitratePreset::Mbps4 => Some(4_000_000),
            BitratePreset::Mbps6 => Some(6_000_000),
            BitratePreset::Mbps8 => Some(8_000_000),
            BitratePreset::Mbps12 => Some(12_000_000),
            BitratePreset::Mbps16 => Some(16_000_000),
            BitratePreset::Mbps24 => Some(24_000_000),
            BitratePreset::Mbps32 => Some(32_000_000),
        }
    }

    pub fn from_config(config: &VideoConfig) -> Self {
        let Some(bps) = config.bitrate_bps else {
            return BitratePreset::Auto;
        };
        Self::all()
            .iter()
            .copied()
            .find(|preset| preset.bps() == Some(bps))
            .unwrap_or(BitratePreset::Auto)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoConfig {
    pub resolution: VideoResolution,
    pub framerate: u32,
    /// When `None`, `default_bitrate()` is used for the current resolution and fps.
    pub bitrate_bps: Option<u32>,
    pub sharing_enabled: bool,
}

impl VideoConfig {
    pub fn effective_bitrate(&self) -> u32 {
        self.bitrate_bps
            .unwrap_or_else(|| default_bitrate(self.resolution, self.framerate))
    }
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            resolution: VideoResolution::P1080,
            framerate: 30,
            bitrate_bps: None,
            sharing_enabled: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPreset {
    pub resolution: VideoResolution,
    pub framerate: u32,
    pub label: &'static str,
}

impl StreamPreset {
    pub fn all() -> &'static [StreamPreset] {
        &[
            StreamPreset {
                resolution: VideoResolution::P720,
                framerate: 30,
                label: "720p · 30 fps",
            },
            StreamPreset {
                resolution: VideoResolution::P1080,
                framerate: 30,
                label: "1080p · 30 fps",
            },
            StreamPreset {
                resolution: VideoResolution::P1080,
                framerate: 60,
                label: "1080p · 60 fps",
            },
            StreamPreset {
                resolution: VideoResolution::P1440,
                framerate: 30,
                label: "1440p · 30 fps",
            },
        ]
    }

    pub fn matches(config: &VideoConfig) -> Option<&'static StreamPreset> {
        Self::all()
            .iter()
            .find(|p| p.resolution == config.resolution && p.framerate == config.framerate)
    }
}
