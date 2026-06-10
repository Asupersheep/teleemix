//! Media server integration: rebuild a downloaded playlist in Plex, Jellyfin
//! or Navidrome (any Subsonic-compatible server).
//!
//! Configured via env vars (all optional — feature is dormant when unset):
//!   MEDIA_SERVER=plex      + PLEX_URL, PLEX_TOKEN
//!   MEDIA_SERVER=jellyfin  + JELLYFIN_URL, JELLYFIN_API_KEY, JELLYFIN_USER_ID
//!   MEDIA_SERVER=navidrome + NAVIDROME_URL, NAVIDROME_USER, NAVIDROME_PASSWORD

use reqwest::Client;
use serde_json::Value;

#[derive(Clone, Debug)]
pub enum MediaServer {
    Plex { url: String, token: String },
    Jellyfin { url: String, api_key: String, user_id: String },
    Navidrome { url: String, user: String, password: String },
}

/// A connected server with any per-connection data resolved (Plex needs the
/// music section key and machine identifier before it can search or create).
pub struct ServerSession {
    server: MediaServer,
    plex_section: String,
    plex_machine: String,
}

impl MediaServer {
    pub fn from_env() -> Option<Self> {
        let kind = std::env::var("MEDIA_SERVER").unwrap_or_default().trim().to_lowercase();
        let var = |k: &str| std::env::var(k).unwrap_or_default().trim().trim_end_matches('/').to_string();
        match kind.as_str() {
            "" | "none" => None,
            "plex" => {
                let (url, token) = (var("PLEX_URL"), var("PLEX_TOKEN"));
                if url.is_empty() || token.is_empty() {
                    log::warn!("MEDIA_SERVER=plex but PLEX_URL/PLEX_TOKEN not set — playlist rebuild disabled");
                    return None;
                }
                Some(Self::Plex { url, token })
            }
            "jellyfin" => {
                let (url, api_key, user_id) = (var("JELLYFIN_URL"), var("JELLYFIN_API_KEY"), var("JELLYFIN_USER_ID"));
                if url.is_empty() || api_key.is_empty() || user_id.is_empty() {
                    log::warn!("MEDIA_SERVER=jellyfin but JELLYFIN_URL/JELLYFIN_API_KEY/JELLYFIN_USER_ID not set — playlist rebuild disabled");
                    return None;
                }
                Some(Self::Jellyfin { url, api_key, user_id })
            }
            "navidrome" | "subsonic" => {
                let (url, user, password) = (var("NAVIDROME_URL"), var("NAVIDROME_USER"), var("NAVIDROME_PASSWORD"));
                if url.is_empty() || user.is_empty() || password.is_empty() {
                    log::warn!("MEDIA_SERVER=navidrome but NAVIDROME_URL/NAVIDROME_USER/NAVIDROME_PASSWORD not set — playlist rebuild disabled");
                    return None;
                }
                Some(Self::Navidrome { url, user, password })
            }
            other => {
                log::warn!("Unknown MEDIA_SERVER {:?} (expected plex, jellyfin or navidrome) — playlist rebuild disabled", other);
                None
            }
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Plex { .. } => "Plex",
            Self::Jellyfin { .. } => "Jellyfin",
            Self::Navidrome { .. } => "Navidrome",
        }
    }

    pub async fn connect(&self, http: &Client) -> Result<ServerSession, String> {
        let mut session = ServerSession {
            server: self.clone(),
            plex_section: String::new(),
            plex_machine: String::new(),
        };
        if let Self::Plex { url, token } = self {
            let identity = plex_get(http, url, token, "/identity", &[]).await
                .ok_or("Plex unreachable (GET /identity failed)")?;
            session.plex_machine = identity["MediaContainer"]["machineIdentifier"]
                .as_str()
                .ok_or("Plex /identity returned no machineIdentifier")?
                .to_string();
            let sections = plex_get(http, url, token, "/library/sections", &[]).await
                .ok_or("Plex GET /library/sections failed")?;
            session.plex_section = sections["MediaContainer"]["Directory"]
                .as_array()
                .and_then(|dirs| dirs.iter().find(|d| d["type"] == "artist"))
                .and_then(|d| d["key"].as_str())
                .ok_or("No music library found on the Plex server")?
                .to_string();
        }
        Ok(session)
    }
}

impl ServerSession {
    pub async fn trigger_scan(&self, http: &Client) {
        match &self.server {
            MediaServer::Plex { url, token } => {
                let path = format!("/library/sections/{}/refresh", self.plex_section);
                plex_raw(http, url, token, "GET", &path, &[]).await;
            }
            MediaServer::Jellyfin { url, api_key, .. } => {
                let r = http.post(format!("{}/Library/Refresh", url))
                    .header("X-Emby-Token", api_key)
                    .send().await;
                if let Err(e) = r { log::warn!("[jellyfin] Library/Refresh failed: {}", e); }
            }
            MediaServer::Navidrome { url, user, password } => {
                sub_get(http, url, user, password, "startScan", &[]).await;
            }
        }
    }

    pub async fn scan_in_progress(&self, http: &Client) -> bool {
        match &self.server {
            MediaServer::Plex { url, token } => {
                plex_get(http, url, token, "/library/sections", &[]).await
                    .and_then(|v| {
                        v["MediaContainer"]["Directory"].as_array().map(|dirs| {
                            dirs.iter()
                                .filter(|d| d["key"] == self.plex_section.as_str())
                                .any(|d| d["refreshing"].as_bool().unwrap_or(false))
                        })
                    })
                    .unwrap_or(false)
            }
            // Jellyfin has no cheap scan-status endpoint; the matching retry loop absorbs it
            MediaServer::Jellyfin { .. } => false,
            MediaServer::Navidrome { url, user, password } => {
                sub_get(http, url, user, password, "getScanStatus", &[]).await
                    .and_then(|v| v["scanStatus"]["scanning"].as_bool())
                    .unwrap_or(false)
            }
        }
    }

    /// Find a track by title + artist; returns the server's item id.
    pub async fn find_track(&self, http: &Client, title: &str, artist: &str) -> Option<String> {
        match &self.server {
            MediaServer::Plex { url, token } => {
                let path = format!("/library/sections/{}/all", self.plex_section);
                let v = plex_get(http, url, token, &path, &[("type", "10"), ("title", title)]).await?;
                let items = v["MediaContainer"]["Metadata"].as_array()?.clone();
                pick_match(&items, artist,
                    |i| i["grandparentTitle"].as_str().unwrap_or("").to_string()
                        + " " + i["originalTitle"].as_str().unwrap_or(""))
                    .and_then(plex_rating_key)
            }
            MediaServer::Jellyfin { url, api_key, user_id } => {
                let resp = http.get(format!("{}/Items", url))
                    .header("X-Emby-Token", api_key)
                    .query(&[
                        ("IncludeItemTypes", "Audio"),
                        ("Recursive", "true"),
                        ("SearchTerm", title),
                        ("Limit", "10"),
                        ("UserId", user_id),
                    ])
                    .send().await.ok()?;
                let v: Value = resp.json().await.ok()?;
                let items = v["Items"].as_array()?.clone();
                pick_match(&items, artist,
                    |i| {
                        let artists = i["Artists"].as_array()
                            .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "))
                            .unwrap_or_default();
                        artists + " " + i["AlbumArtist"].as_str().unwrap_or("")
                    })
                    .and_then(|i| i["Id"].as_str().map(|s| s.to_string()))
            }
            MediaServer::Navidrome { url, user, password } => {
                let query = if artist.is_empty() { title.to_string() } else { format!("{} {}", title, artist) };
                let mut songs = sub_search(http, url, user, password, &query).await;
                if songs.is_empty() && !artist.is_empty() {
                    songs = sub_search(http, url, user, password, title).await;
                }
                pick_match(&songs, artist, |s| s["artist"].as_str().unwrap_or("").to_string())
                    .and_then(|s| s["id"].as_str().map(|x| x.to_string()))
            }
        }
    }

    /// Delete any existing playlist with the same name, then create it fresh
    /// with the given item ids.
    pub async fn rebuild_playlist(&self, http: &Client, name: &str, ids: &[String]) -> Result<(), String> {
        match &self.server {
            MediaServer::Plex { url, token } => {
                if let Some(v) = plex_get(http, url, token, "/playlists", &[("playlistType", "audio")]).await {
                    if let Some(lists) = v["MediaContainer"]["Metadata"].as_array() {
                        for pl in lists {
                            if pl["title"].as_str().map(|t| t.eq_ignore_ascii_case(name)).unwrap_or(false) {
                                if let Some(key) = plex_rating_key(pl) {
                                    plex_raw(http, url, token, "DELETE", &format!("/playlists/{}", key), &[]).await;
                                }
                            }
                        }
                    }
                }
                let uri = format!(
                    "server://{}/com.plexapp.plugins.library/library/metadata/{}",
                    self.plex_machine, ids.join(",")
                );
                let ok = plex_raw(http, url, token, "POST", "/playlists", &[
                    ("type", "audio"), ("title", name), ("smart", "0"), ("uri", &uri),
                ]).await;
                if ok { Ok(()) } else { Err("Plex rejected the playlist creation".to_string()) }
            }
            MediaServer::Jellyfin { url, api_key, user_id } => {
                let resp = http.get(format!("{}/Items", url))
                    .header("X-Emby-Token", api_key)
                    .query(&[
                        ("IncludeItemTypes", "Playlist"),
                        ("Recursive", "true"),
                        ("SearchTerm", name),
                        ("UserId", user_id),
                    ])
                    .send().await;
                if let Ok(r) = resp {
                    if let Ok(v) = r.json::<Value>().await {
                        if let Some(items) = v["Items"].as_array() {
                            for pl in items {
                                let matches = pl["Name"].as_str().map(|n| n.eq_ignore_ascii_case(name)).unwrap_or(false);
                                if matches {
                                    if let Some(id) = pl["Id"].as_str() {
                                        let _ = http.delete(format!("{}/Items/{}", url, id))
                                            .header("X-Emby-Token", api_key)
                                            .send().await;
                                    }
                                }
                            }
                        }
                    }
                }
                let resp = http.post(format!("{}/Playlists", url))
                    .header("X-Emby-Token", api_key)
                    .json(&serde_json::json!({
                        "Name": name,
                        "Ids": ids,
                        "UserId": user_id,
                        "MediaType": "Audio",
                    }))
                    .send().await
                    .map_err(|e| e.to_string())?;
                if resp.status().is_success() {
                    Ok(())
                } else {
                    Err(format!("Jellyfin rejected the playlist creation: {}", resp.status()))
                }
            }
            MediaServer::Navidrome { url, user, password } => {
                if let Some(v) = sub_get(http, url, user, password, "getPlaylists", &[]).await {
                    if let Some(lists) = v["playlists"]["playlist"].as_array() {
                        for pl in lists {
                            if pl["name"].as_str().map(|n| n.eq_ignore_ascii_case(name)).unwrap_or(false) {
                                if let Some(id) = pl["id"].as_str() {
                                    sub_get(http, url, user, password, "deletePlaylist", &[("id", id)]).await;
                                }
                            }
                        }
                    }
                }
                let mut params: Vec<(&str, &str)> = vec![("name", name)];
                for id in ids {
                    params.push(("songId", id.as_str()));
                }
                match sub_get(http, url, user, password, "createPlaylist", &params).await {
                    Some(_) => Ok(()),
                    None => Err("Navidrome rejected the playlist creation".to_string()),
                }
            }
        }
    }
}

/// Prefer a result whose artist field matches, otherwise take the first.
fn pick_match<'a, F>(items: &'a [Value], artist: &str, artist_of: F) -> Option<&'a Value>
where
    F: Fn(&Value) -> String,
{
    if items.is_empty() {
        return None;
    }
    if !artist.is_empty() {
        let want = artist.to_lowercase();
        for item in items {
            let have = artist_of(item).to_lowercase();
            if have.contains(&want) || want.contains(have.trim()) && !have.trim().is_empty() {
                return Some(item);
            }
        }
    }
    items.first()
}

// ── Plex helpers ──────────────────────────────────────────────────────────────

fn plex_rating_key(item: &Value) -> Option<String> {
    item["ratingKey"].as_str().map(|s| s.to_string())
        .or_else(|| item["ratingKey"].as_u64().map(|n| n.to_string()))
}

async fn plex_get(http: &Client, base: &str, token: &str, path: &str, extra: &[(&str, &str)]) -> Option<Value> {
    let mut query: Vec<(&str, &str)> = vec![("X-Plex-Token", token)];
    query.extend_from_slice(extra);
    let resp = http.get(format!("{}{}", base, path))
        .header("Accept", "application/json")
        .query(&query)
        .send().await.ok()?;
    if !resp.status().is_success() {
        log::warn!("[plex] GET {} -> {}", path, resp.status());
        return None;
    }
    resp.json().await.ok()
}

/// Plex request where we only care about the status code (refresh/create/delete
/// return empty or XML bodies).
async fn plex_raw(http: &Client, base: &str, token: &str, method: &str, path: &str, extra: &[(&str, &str)]) -> bool {
    let mut query: Vec<(&str, &str)> = vec![("X-Plex-Token", token)];
    query.extend_from_slice(extra);
    let url = format!("{}{}", base, path);
    let req = match method {
        "POST" => http.post(&url),
        "DELETE" => http.delete(&url),
        _ => http.get(&url),
    };
    match req.query(&query).send().await {
        Ok(r) if r.status().is_success() => true,
        Ok(r) => { log::warn!("[plex] {} {} -> {}", method, path, r.status()); false }
        Err(e) => { log::warn!("[plex] {} {} failed: {}", method, path, e); false }
    }
}

// ── Subsonic (Navidrome) helpers ──────────────────────────────────────────────

async fn sub_get(http: &Client, base: &str, user: &str, password: &str, endpoint: &str, extra: &[(&str, &str)]) -> Option<Value> {
    let salt = format!("{:x}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0));
    let token = format!("{:x}", md5::compute(format!("{}{}", password, salt)));
    let mut query: Vec<(&str, &str)> = vec![
        ("u", user), ("t", &token), ("s", &salt),
        ("v", "1.16.1"), ("c", "teleemix"), ("f", "json"),
    ];
    query.extend_from_slice(extra);

    let resp = match http.get(format!("{}/rest/{}", base, endpoint)).query(&query).send().await {
        Ok(r) => r,
        Err(e) => { log::warn!("[navidrome] {} failed: {}", endpoint, e); return None; }
    };
    let v: Value = resp.json().await.ok()?;
    if v["subsonic-response"]["status"] == "ok" {
        Some(v["subsonic-response"].clone())
    } else {
        log::warn!("[navidrome] {} error: {}", endpoint, v["subsonic-response"]["error"]);
        None
    }
}

async fn sub_search(http: &Client, base: &str, user: &str, password: &str, query: &str) -> Vec<Value> {
    sub_get(http, base, user, password, "search3", &[
        ("query", query), ("songCount", "10"), ("artistCount", "0"), ("albumCount", "0"),
    ]).await
        .and_then(|v| v["searchResult3"]["song"].as_array().cloned())
        .unwrap_or_default()
}
