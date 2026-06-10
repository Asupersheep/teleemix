use regex::Regex;
use reqwest::Client;
use serde_json::Value;

pub struct SpotifyMeta {
    pub query: String,
    pub label: String,
}

/// A single track scanned from a playlist (Spotify or YouTube).
pub struct PlaylistTrack {
    pub title: String,
    pub artist: String,
}

pub struct Playlist {
    pub name: String,
    pub tracks: Vec<PlaylistTrack>,
}

/// Resolve a Spotify URL to track title + artist.
/// Uses the Spotify embed page __NEXT_DATA__ JSON — no API key required.
/// Falls back to oEmbed for title if embed page fails.
pub async fn resolve(url: &str) -> Option<SpotifyMeta> {
    let client = Client::new();

    let (title, artist) = get_metadata_from_embed(&client, url).await;

    // If embed page didn't give us a title, try oEmbed
    let title = if title.is_empty() {
        get_title_from_oembed(&client, url).await.unwrap_or_default()
    } else {
        title
    };

    if title.is_empty() {
        return None;
    }

    let query = format!("{} {}", title, artist).trim().to_string();
    let label = if artist.is_empty() {
        title.clone()
    } else {
        format!("{} — {}", title, artist)
    };

    Some(SpotifyMeta { query, label })
}

/// Resolve a Spotify playlist URL to its name and track list by scraping the
/// embed page __NEXT_DATA__ JSON — no API key required. Works for any public
/// playlist, including user-generated ones. The embed page exposes roughly the
/// first 100 tracks.
pub async fn resolve_playlist(url: &str) -> Option<Playlist> {
    let client = Client::new();
    let entity = get_embed_entity(&client, url).await?;

    let name = entity["name"]
        .as_str()
        .or_else(|| entity["title"].as_str())
        .unwrap_or("Spotify playlist")
        .to_string();

    let tracks: Vec<PlaylistTrack> = entity["trackList"]
        .as_array()?
        .iter()
        .filter_map(|t| {
            let title = t["title"].as_str()?.trim().to_string();
            if title.is_empty() {
                return None;
            }
            let artist = t["subtitle"].as_str().unwrap_or("").trim().to_string();
            Some(PlaylistTrack { title, artist })
        })
        .collect();

    if tracks.is_empty() {
        return None;
    }
    Some(Playlist { name, tracks })
}

/// Fetch the Spotify embed page for a URL and return the entity JSON
/// from __NEXT_DATA__.
async fn get_embed_entity(client: &Client, url: &str) -> Option<Value> {
    let embed_url = url
        .replace("open.spotify.com/", "open.spotify.com/embed/")
        .split('?')
        .next()
        .unwrap_or("")
        .to_string()
        + "?utm_source=oembed";

    let resp = client.get(&embed_url).send().await.ok()?;
    let body = resp.text().await.ok()?;

    let re = Regex::new(r#"<script id="__NEXT_DATA__" type="application/json">(.*?)</script>"#)
        .unwrap();
    let json_str = re.captures(&body)?.get(1)?.as_str();

    let data: Value = serde_json::from_str(json_str).ok()?;
    let entity = data["props"]["pageProps"]["state"]["data"]["entity"].clone();
    if entity.is_null() {
        None
    } else {
        Some(entity)
    }
}

async fn get_metadata_from_embed(client: &Client, url: &str) -> (String, String) {
    let entity = match get_embed_entity(client, url).await {
        Some(e) => e,
        None => return (String::new(), String::new()),
    };

    let title = entity["name"].as_str().unwrap_or("").to_string();
    let artist = entity["artists"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a["name"].as_str().map(|s| s.to_string()))
                .collect::<Vec<String>>()
                .join(", ")
        })
        .unwrap_or_default();

    (title, artist)
}

async fn get_title_from_oembed(client: &Client, url: &str) -> Option<String> {
    let resp = client
        .get("https://open.spotify.com/oembed")
        .query(&[("url", url)])
        .send()
        .await
        .ok()?;

    let data: Value = resp.json().await.ok()?;
    data["title"].as_str().map(|s| s.to_string())
}
