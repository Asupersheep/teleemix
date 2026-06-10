use std::sync::Arc;

use serde_json::Value;

use crate::BotState;

pub async fn login(state: &Arc<BotState>) {
    if state.config.deemix_arl.is_empty() {
        log::warn!("DEEMIX_ARL not set — bot may get NotLoggedIn errors.");
        return;
    }
    match login_arl(state, &state.config.deemix_arl.clone()).await {
        Ok(_) => log::info!("Successfully logged into deemix"),
        Err(e) => log::warn!("Could not login to deemix: {}", e),
    }
}

pub async fn login_arl(state: &Arc<BotState>, arl: &str) -> Result<String, String> {
    let url = format!("{}/api/loginArl", state.config.deemix_url);
    let resp = state
        .http
        .post(&url)
        .json(&serde_json::json!({ "arl": arl }))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let data: Value = resp.json().await.map_err(|e| e.to_string())?;

    // status 1 = logged in, status 2 = already logged in
    let status = data["status"].as_i64().unwrap_or(0);
    if status == 1 || status == 2 {
        let username = data["user"]["name"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();
        Ok(username)
    } else {
        Err(format!("Login failed (status {})", status))
    }
}

/// deemix sessions live in an in-memory store with a ~24h TTL, so the bot's
/// cookie session silently expires and POSTs start failing with NotLoggedIn.
/// Re-login with the current ARL so the caller can retry.
async fn relogin(state: &Arc<BotState>) -> Result<(), String> {
    let arl = state.current_arl.lock().await.clone();
    if arl.is_empty() {
        return Err("Session expired and no ARL is configured".to_string());
    }
    log::warn!("deemix session expired — re-logging in");
    login_arl(state, &arl).await.map(|_| ())
}

/// Queue a URL on deemix. Returns the queue uuids of the created download
/// objects (empty when the item was already in the queue).
pub async fn add_to_queue(state: &Arc<BotState>, url: &str) -> Result<Vec<String>, String> {
    match add_to_queue_once(state, url).await {
        Err(e) if e.eq_ignore_ascii_case("notloggedin") => {
            relogin(state).await?;
            add_to_queue_once(state, url).await
        }
        other => other,
    }
}

async fn add_to_queue_once(state: &Arc<BotState>, url: &str) -> Result<Vec<String>, String> {
    let bitrate = *state.current_bitrate.lock().await;
    let endpoint = format!("{}/api/addToQueue", state.config.deemix_url);
    let resp = state
        .http
        .post(&endpoint)
        .json(&serde_json::json!({ "url": url, "bitrate": bitrate }))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let data: Value = resp.json().await.map_err(|e| e.to_string())?;

    if data["result"].as_bool().unwrap_or(false) {
        let uuids = data["data"]["obj"]
            .as_array()
            .map(|objs| {
                objs.iter()
                    .filter_map(|o| o["uuid"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(uuids)
    } else {
        Err(data["errid"]
            .as_str()
            .unwrap_or("Unknown error")
            .to_string())
    }
}

/// Map of queue item uuid → status ("inQueue", "downloading", "completed",
/// "failed", "withErrors"). Used by playlist jobs to track downloads.
pub async fn queue_status_map(state: &Arc<BotState>) -> Result<std::collections::HashMap<String, String>, String> {
    let url = format!("{}/api/getQueue", state.config.deemix_url);
    let resp = state.http.get(&url).send().await.map_err(|e| e.to_string())?;
    let data: Value = resp.json().await.map_err(|e| e.to_string())?;

    let mut map = std::collections::HashMap::new();
    if let Some(queue) = data["queue"].as_object() {
        for (uuid, item) in queue {
            let status = match item["status"].as_str() {
                Some(s) => s.to_string(),
                None => {
                    let downloaded = item["downloaded"].as_u64().unwrap_or(0);
                    let size = item["size"].as_u64().unwrap_or(1);
                    if downloaded >= size && size > 0 { "completed".to_string() } else { "inQueue".to_string() }
                }
            };
            map.insert(uuid.clone(), status);
        }
    }
    Ok(map)
}

pub struct QueueStatus {
    pub pending: usize,
    pub downloading: usize,
    pub done: usize,
    pub failed: usize,
}

pub async fn get_queue(state: &Arc<BotState>) -> Result<QueueStatus, String> {
    let url = format!("{}/api/getQueue", state.config.deemix_url);
    let resp = state
        .http
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let data: Value = resp.json().await.map_err(|e| e.to_string())?;
    let mut pending = 0usize;
    let mut downloading = 0usize;
    let mut done = 0usize;
    let mut failed = 0usize;

    if let Some(queue) = data["queue"].as_object() {
        for item in queue.values() {
            match item["status"].as_str() {
                Some("completed") | Some("withErrors") => done += 1,
                Some("failed") => failed += 1,
                Some("downloading") => downloading += 1,
                Some("inQueue") => pending += 1,
                // Older deemix builds don't expose status — fall back to counters
                _ => {
                    let progress = item["progress"].as_u64().unwrap_or(0);
                    let downloaded = item["downloaded"].as_u64().unwrap_or(0);
                    let size = item["size"].as_u64().unwrap_or(1);
                    if downloaded >= size && size > 0 {
                        done += 1;
                    } else if progress > 0 {
                        downloading += 1;
                    } else {
                        pending += 1;
                    }
                }
            }
        }
    }

    Ok(QueueStatus { pending, downloading, done, failed })
}

pub async fn clear_completed(state: &Arc<BotState>) -> Result<usize, String> {
    let url = format!("{}/api/getQueue", state.config.deemix_url);
    let resp = state.http.get(&url).send().await.map_err(|e| e.to_string())?;
    let data: Value = resp.json().await.map_err(|e| e.to_string())?;

    // Items deemix's own removeFinishedDownloads handles (status == "completed")
    // vs. ones we have to remove one by one (failed / withErrors / legacy builds).
    let mut completed = 0usize;
    let mut individual: Vec<String> = Vec::new();
    if let Some(queue) = data["queue"].as_object() {
        for (uuid, item) in queue {
            match item["status"].as_str() {
                Some("completed") => completed += 1,
                Some("failed") | Some("withErrors") => individual.push(uuid.clone()),
                Some(_) => {}
                None => {
                    let downloaded = item["downloaded"].as_u64().unwrap_or(0);
                    let size = item["size"].as_u64().unwrap_or(1);
                    if downloaded >= size && size > 0 {
                        individual.push(uuid.clone());
                    }
                }
            }
        }
    }

    let mut cleared = 0usize;

    if completed > 0 {
        let url = format!("{}/api/removeFinishedDownloads", state.config.deemix_url);
        let resp = state.http.post(&url).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("removeFinishedDownloads failed: {}", resp.status()));
        }
        cleared += completed;
    }

    // removeFromQueue takes the uuid as a query parameter, not a JSON body
    for uuid in &individual {
        let url = format!("{}/api/removeFromQueue", state.config.deemix_url);
        let ok = state.http
            .post(&url)
            .query(&[("uuid", uuid.as_str())])
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if ok {
            cleared += 1;
        }
    }

    Ok(cleared)
}

pub async fn search(
    state: &Arc<BotState>,
    query: &str,
    search_type: &str,
) -> Result<Vec<Value>, String> {
    let url = format!("{}/api/search", state.config.deemix_url);
    let resp = state
        .http
        .get(&url)
        .query(&[("term", query), ("type", search_type)])
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let data: Value = resp.json().await.map_err(|e| e.to_string())?;

    let results = data["data"]
        .as_array()
        .or_else(|| {
            data["results"]["data"].as_array()
        })
        .cloned()
        .unwrap_or_default();

    Ok(results.into_iter().take(8).collect())
}
