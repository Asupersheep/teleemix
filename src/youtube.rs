//! YouTube playlist scanning.
//!
//! Scrapes the playlist page's `ytInitialData` JSON — no API key required.
//! Works for public user-generated playlists; the page exposes the first
//! ~100 videos. Auto-generated mixes (list=RD...) have no playlist page
//! and return None.

use lazy_static::lazy_static;
use regex::Regex;
use reqwest::Client;
use serde_json::Value;

use crate::spotify::{Playlist, PlaylistTrack};

lazy_static! {
    static ref LIST_ID_RE: Regex = Regex::new(r"[?&]list=([A-Za-z0-9_-]+)").unwrap();
    // Bracketed noise like (Official Video), [Lyrics], (HD), (Audio) etc.
    static ref TITLE_NOISE_RE: Regex = Regex::new(
        r"(?i)[(\[][^)\]]*(official|video|lyric|lyrics|audio|visuali[sz]er|hd|4k|remaster|m/v|mv|clip)[^)\]]*[)\]]"
    ).unwrap();
    static ref YT_INITIAL_DATA_RE: Regex = Regex::new(
        r"(?s)var ytInitialData\s*=\s*(\{.*?\});\s*</script>"
    ).unwrap();
}

pub fn playlist_id(url: &str) -> Option<String> {
    LIST_ID_RE.captures(url).and_then(|c| c.get(1)).map(|m| m.as_str().to_string())
}

/// Strip list/index params so a watch URL can be handled as a single video.
pub fn strip_playlist_params(url: &str) -> String {
    let re = Regex::new(r"[?&](list|index|start_radio)=[^&]*").unwrap();
    let stripped = re.replace_all(url, "").to_string();
    // If we removed the first query param, the remaining "&" must become "?"
    if !stripped.contains('?') {
        stripped.replacen('&', "?", 1)
    } else {
        stripped
    }
}

pub async fn resolve_playlist(http: &Client, url: &str) -> Option<Playlist> {
    let id = playlist_id(url)?;
    // Mixes/radios are generated per-user and have no playlist page
    if id.starts_with("RD") {
        return None;
    }

    let page_url = format!("https://www.youtube.com/playlist?list={}", id);
    let resp = http
        .get(&page_url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Cookie", "CONSENT=YES+1; SOCS=CAI")
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        log::info!("[youtube] playlist page fetch failed: {}", resp.status());
        return None;
    }
    let html = resp.text().await.ok()?;

    let json_str = YT_INITIAL_DATA_RE.captures(&html)?.get(1)?.as_str();
    let data: Value = serde_json::from_str(json_str).ok()?;

    let name = data["metadata"]["playlistMetadataRenderer"]["title"]
        .as_str()
        .unwrap_or("YouTube playlist")
        .to_string();

    let contents = &data["contents"]["twoColumnBrowseResultsRenderer"]["tabs"][0]
        ["tabRenderer"]["content"]["sectionListRenderer"]["contents"][0]
        ["itemSectionRenderer"]["contents"][0]["playlistVideoListRenderer"]["contents"];

    let items = contents.as_array()?;
    let mut tracks = Vec::new();
    for item in items {
        let r = &item["playlistVideoRenderer"];
        let raw_title = match r["title"]["runs"][0]["text"].as_str() {
            Some(t) => t,
            None => continue, // continuation items, unavailable videos, etc.
        };
        let channel = r["shortBylineText"]["runs"][0]["text"].as_str().unwrap_or("");

        let title = clean_video_title(raw_title);
        if title.is_empty() {
            continue;
        }
        // "Artist - Song" titles already carry the artist; otherwise (e.g.
        // auto-generated "Topic" channels with bare song titles) use the channel.
        let artist = if title.contains(" - ") {
            String::new()
        } else {
            clean_channel(channel)
        };
        tracks.push(PlaylistTrack { title, artist });
    }

    log::info!("[youtube] playlist \"{}\": scanned {} videos", name, tracks.len());
    if tracks.is_empty() {
        None
    } else {
        Some(Playlist { name, tracks })
    }
}

fn clean_video_title(t: &str) -> String {
    let cleaned = TITLE_NOISE_RE.replace_all(t, " ");
    let cleaned = cleaned.replace('|', " ");
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean_channel(c: &str) -> String {
    let c = c.trim();
    let c = c.strip_suffix(" - Topic").unwrap_or(c);
    let c = c.strip_suffix("VEVO").unwrap_or(c);
    c.trim().to_string()
}
