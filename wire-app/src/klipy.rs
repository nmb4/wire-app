//! Direct client integration with KLIPY's GIF API and media delivery URLs.

use anyhow::{bail, Context, Result};
use image::{codecs::gif::GifDecoder, AnimationDecoder};
use reqwest::{blocking::Client, redirect::Policy, Url};
use serde::Deserialize;
use std::{
    io::{Cursor, Read},
    sync::OnceLock,
    time::Duration,
};

const API_BASE: &str = "https://api.klipy.com/api/v1/";
const MAX_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;
const MAX_GIF_BYTES: u64 = 16 * 1024 * 1024;
const MAX_GIF_FRAMES: usize = 120;
const MAX_DECODED_PIXELS: u64 = 5_000_000;
static HTTP_CLIENT: OnceLock<Client> = OnceLock::new();

#[derive(Debug, Clone)]
pub(crate) struct GifItem {
    pub kind: String,
    pub slug: String,
    pub title: String,
    pub preview_url: Option<String>,
    pub media_url: Option<String>,
    pub width: u32,
    pub height: u32,
    pub byte_len: u64,
    pub ad_content: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct GifFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub delay_ms: u32,
}

#[derive(Debug)]
pub(crate) enum UiEvent {
    SearchFinished {
        request_id: u64,
        query: String,
        result: Result<Vec<GifItem>, String>,
    },
    AnimationFinished {
        request_id: u64,
        preview_only: bool,
        url: String,
        result: Result<Vec<GifFrame>, String>,
    },
}

#[derive(Deserialize)]
struct ApiEnvelope {
    #[serde(default)]
    result: bool,
    #[serde(default)]
    data: Option<ApiResults>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize, Default)]
struct ApiResults {
    #[serde(default)]
    data: Vec<ApiItem>,
}

#[derive(Deserialize, Default)]
struct ApiItem {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    file: Option<ApiFile>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
}

#[derive(Deserialize, Default)]
struct ApiFile {
    #[serde(default)]
    xs: Option<ApiFormats>,
    #[serde(default)]
    sm: Option<ApiFormats>,
    #[serde(default)]
    md: Option<ApiFormats>,
    #[serde(default)]
    hd: Option<ApiFormats>,
}

#[derive(Deserialize, Default)]
struct ApiFormats {
    #[serde(default)]
    gif: Option<ApiFormat>,
    #[serde(default)]
    webp: Option<ApiFormat>,
}

#[derive(Deserialize, Default)]
struct ApiFormat {
    #[serde(default)]
    url: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    size: u64,
}

fn client() -> Result<&'static Client> {
    if let Some(client) = HTTP_CLIENT.get() {
        return Ok(client);
    }
    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() < 5 && is_klipy_request_url(attempt.url().as_str()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .user_agent(concat!("Wire/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("could not create KLIPY HTTP client")?;
    Ok(HTTP_CLIENT.get_or_init(|| client))
}

fn api_url(api_key: &str, endpoint: &str) -> Result<Url> {
    let mut url = Url::parse(API_BASE).context("invalid KLIPY API base URL")?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid KLIPY API URL"))?
        .push(api_key.trim())
        .push("gifs")
        .push(endpoint);
    Ok(url)
}

pub(crate) fn search_gifs(api_key: &str, customer_id: &str, query: &str) -> Result<Vec<GifItem>> {
    if api_key.trim().is_empty() {
        bail!("Add a KLIPY API key in Settings to browse GIFs.");
    }
    let endpoint = if query.trim().is_empty() {
        "trending"
    } else {
        "search"
    };
    let mut url = api_url(api_key, endpoint)?;
    {
        let mut params = url.query_pairs_mut();
        params
            .append_pair("page", "1")
            .append_pair("per_page", "24")
            .append_pair("customer_id", customer_id)
            .append_pair("format_filter", "gif,webp")
            .append_pair("content_filter", "high");
        if !query.trim().is_empty() {
            params.append_pair("q", query);
        }
    }

    let response: ApiEnvelope = client()?
        .get(url)
        .send()
        .context("could not reach KLIPY")?
        .error_for_status()
        .context("KLIPY rejected the GIF request")?
        .json()
        .context("KLIPY returned an invalid GIF response")?;
    if !response.result {
        bail!(
            "{}",
            response
                .message
                .unwrap_or_else(|| "KLIPY could not load GIFs".to_owned())
        );
    }

    let items = response.data.unwrap_or_default().data;
    Ok(items.into_iter().map(map_api_item).collect())
}

fn map_api_item(item: ApiItem) -> GifItem {
    let file = item.file.unwrap_or_default();
    let preview = file
        .xs
        .as_ref()
        .and_then(|formats| nonempty_format(formats.webp.as_ref()))
        .or_else(|| {
            file.sm
                .as_ref()
                .and_then(|formats| nonempty_format(formats.webp.as_ref()))
        })
        .or_else(|| {
            file.xs
                .as_ref()
                .and_then(|formats| nonempty_format(formats.gif.as_ref()))
        })
        .or_else(|| {
            file.sm
                .as_ref()
                .and_then(|formats| nonempty_format(formats.gif.as_ref()))
        });
    let media = file
        .sm
        .as_ref()
        .and_then(|formats| nonempty_format(formats.gif.as_ref()))
        .or_else(|| {
            file.md
                .as_ref()
                .and_then(|formats| nonempty_format(formats.gif.as_ref()))
        })
        .or_else(|| {
            file.hd
                .as_ref()
                .and_then(|formats| nonempty_format(formats.gif.as_ref()))
        })
        .or_else(|| {
            file.xs
                .as_ref()
                .and_then(|formats| nonempty_format(formats.gif.as_ref()))
        });
    GifItem {
        kind: if item.kind.is_empty() {
            "gif".to_owned()
        } else {
            item.kind
        },
        slug: item.slug,
        title: item.title,
        preview_url: preview.map(|format| format.url.clone()),
        media_url: media.map(|format| format.url.clone()),
        width: media.or(preview).map_or(item.width, |format| format.width),
        height: media
            .or(preview)
            .map_or(item.height, |format| format.height),
        byte_len: media.map_or(0, |format| format.size),
        ad_content: item.content,
    }
}

fn nonempty_format(format: Option<&ApiFormat>) -> Option<&ApiFormat> {
    format.filter(|format| !format.url.trim().is_empty())
}

pub(crate) fn track_share(api_key: &str, customer_id: &str, slug: &str, query: &str) -> Result<()> {
    if api_key.trim().is_empty() || slug.trim().is_empty() {
        bail!("KLIPY share tracking is missing required details");
    }
    let mut url = api_url(api_key, "share")?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid KLIPY API URL"))?;
        segments.pop();
        segments.push("share").push(slug);
    }
    client()?
        .post(url)
        .json(&serde_json::json!({
            "customer_id": customer_id,
            "q": query,
        }))
        .send()
        .context("could not send KLIPY share event")?
        .error_for_status()
        .context("KLIPY rejected the share event")?;
    Ok(())
}

pub(crate) fn is_klipy_media_url(raw: &str) -> bool {
    Url::parse(raw).ok().is_some_and(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443)
            && matches!(
                url.host_str(),
                Some("static.klipy.com" | "static1.klipy.com" | "static2.klipy.com")
            )
    })
}

pub(crate) fn gif_url_from_message_body(body: &str) -> Option<&str> {
    let raw = body.trim();
    let url = Url::parse(raw).ok()?;
    let has_gif_extension = url
        .path()
        .rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("gif"));
    (is_klipy_media_url(raw) && has_gif_extension).then_some(raw)
}

fn is_klipy_request_url(raw: &str) -> bool {
    is_klipy_media_url(raw)
        || Url::parse(raw).ok().is_some_and(|url| {
            url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && url.port_or_known_default() == Some(443)
                && url.host_str() == Some("api.klipy.com")
        })
}

pub(crate) fn load_animation(raw_url: &str) -> Result<Vec<GifFrame>> {
    let bytes = download_media(raw_url, MAX_GIF_BYTES)?;
    let decoder = GifDecoder::new(Cursor::new(bytes)).context("KLIPY media is not a GIF")?;
    let mut frames = Vec::new();
    let mut decoded_pixels = 0_u64;
    for frame in decoder.into_frames().take(MAX_GIF_FRAMES) {
        let frame = frame.context("could not decode KLIPY GIF frame")?;
        let (numerator_ms, denominator) = frame.delay().numer_denom_ms();
        let buffer = frame.into_buffer();
        let width = buffer.width();
        let height = buffer.height();
        validate_dimensions(width, height)?;
        decoded_pixels = decoded_pixels.saturating_add(u64::from(width) * u64::from(height));
        if decoded_pixels > MAX_DECODED_PIXELS {
            bail!("GIF animation is too large to display");
        }
        frames.push(GifFrame {
            width,
            height,
            rgba: buffer.into_raw(),
            delay_ms: (numerator_ms / denominator.max(1)).clamp(20, 10_000),
        });
    }
    if frames.is_empty() {
        bail!("GIF has no frames");
    }
    Ok(frames)
}

pub(crate) fn load_preview(raw_url: &str) -> Result<Vec<GifFrame>> {
    let bytes = download_media(raw_url, MAX_PREVIEW_BYTES)?;
    let image = image::load_from_memory(&bytes).context("KLIPY preview is not an image")?;
    let buffer = image.into_rgba8();
    let width = buffer.width();
    let height = buffer.height();
    validate_dimensions(width, height)?;
    Ok(vec![GifFrame {
        width,
        height,
        rgba: buffer.into_raw(),
        delay_ms: 10_000,
    }])
}

fn download_media(raw_url: &str, byte_limit: u64) -> Result<Vec<u8>> {
    if !is_klipy_media_url(raw_url) {
        bail!("media URL is not a trusted KLIPY URL");
    }
    let mut response = client()?
        .get(raw_url)
        .send()
        .context("could not load GIF from KLIPY")?
        .error_for_status()
        .context("KLIPY could not load this GIF")?;
    if response
        .content_length()
        .is_some_and(|length| length > byte_limit)
    {
        bail!("KLIPY media is too large to display");
    }
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(byte_limit + 1)
        .read_to_end(&mut bytes)
        .context("could not read media from KLIPY")?;
    if bytes.len() as u64 > byte_limit {
        bail!("KLIPY media is too large to display");
    }
    Ok(bytes)
}

fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        bail!("KLIPY media has unsupported dimensions");
    }
    Ok(())
}
