//! Playlist rebuild jobs.
//!
//! After a scanned playlist is queued on deemix, a job is persisted to
//! jobs.json and a background worker:
//!   1. polls the deemix queue until every tracked download reaches a
//!      terminal state (completed / failed / removed),
//!   2. triggers a media server library scan and waits for it,
//!   3. matches each track by title + artist (with retries while the
//!      server indexes), and
//!   4. rebuilds a playlist with the same name (delete + recreate).
//!
//! Jobs survive bot restarts: they are loaded from jobs.json on startup and
//! their workers respawned.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use teloxide::prelude::*;

use crate::{deemix, BotState};

/// Poll deemix every 30s for up to 2h before giving up on downloads.
const DOWNLOAD_POLL_INTERVAL: Duration = Duration::from_secs(30);
const DOWNLOAD_MAX_POLLS: usize = 240;
/// Wait for the library scan up to 5 minutes.
const SCAN_POLL_INTERVAL: Duration = Duration::from_secs(10);
const SCAN_MAX_POLLS: usize = 30;
/// Retry matching unfound tracks — up to 6 minutes total to absorb slow indexing.
const MATCH_ATTEMPTS: usize = 12;
const MATCH_RETRY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Serialize, Deserialize)]
pub struct JobTrack {
    pub title: String,
    pub artist: String,
    /// deemix queue uuid; empty when unknown (e.g. was already in the queue),
    /// in which case the download is assumed done.
    #[serde(default)]
    pub uuid: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PlaylistJob {
    pub id: String,
    pub chat_id: i64,
    pub name: String,
    pub tracks: Vec<JobTrack>,
}

pub type JobsDb = Arc<RwLock<Vec<PlaylistJob>>>;

pub fn load(path: &str) -> JobsDb {
    let jobs: Vec<PlaylistJob> = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Arc::new(RwLock::new(jobs))
}

fn save(db: &JobsDb, path: &str) {
    if let Ok(jobs) = db.read() {
        if let Ok(json) = serde_json::to_string_pretty(&*jobs) {
            let _ = std::fs::write(path, json);
        }
    }
}

pub fn add(state: &Arc<BotState>, job: PlaylistJob) {
    if let Ok(mut jobs) = state.jobs.write() {
        jobs.push(job);
    }
    save(&state.jobs, &state.config.jobs_file);
}

fn remove(state: &Arc<BotState>, job_id: &str) {
    if let Ok(mut jobs) = state.jobs.write() {
        jobs.retain(|j| j.id != job_id);
    }
    save(&state.jobs, &state.config.jobs_file);
}

pub fn spawn(bot: Bot, state: Arc<BotState>, job: PlaylistJob) {
    tokio::spawn(run(bot, state, job));
}

/// Respawn workers for jobs that were pending when the bot last stopped.
pub fn resume_all(bot: &Bot, state: &Arc<BotState>) {
    let pending: Vec<PlaylistJob> = state.jobs.read().map(|j| j.clone()).unwrap_or_default();
    if pending.is_empty() {
        return;
    }
    log::info!("Resuming {} pending playlist job(s)", pending.len());
    for job in pending {
        spawn(bot.clone(), Arc::clone(state), job);
    }
}

async fn run(bot: Bot, state: Arc<BotState>, job: PlaylistJob) {
    let chat = ChatId(job.chat_id);
    log::info!("[job {}] started: playlist \"{}\", {} tracks", job.id, job.name, job.tracks.len());

    // ── Phase 1: wait for deemix downloads ──
    // Tracks without a uuid were already in the deemix queue (e.g. downloaded
    // before) — nothing to wait for. If no track needs waiting, go straight
    // to the rebuild so cloning an already-downloaded playlist still works.
    let tracked = job.tracks.iter().filter(|t| !t.uuid.is_empty()).count();
    if tracked == 0 {
        log::info!("[job {}] no downloads to wait for, rebuilding immediately", job.id);
    }
    let mut polls = 0usize;
    let mut last_pending = tracked;
    let mut last_map = std::collections::HashMap::new();
    let mut progress_msg: Option<teloxide::types::MessageId> = None;
    while tracked > 0 {
        match deemix::queue_status_map(&state).await {
            Ok(map) => {
                let pending = job.tracks.iter()
                    .filter(|t| !t.uuid.is_empty())
                    .filter(|t| matches!(map.get(&t.uuid).map(|s| s.as_str()), Some("inQueue") | Some("downloading")))
                    .count();
                last_map = map;
                last_pending = pending;
                if pending == 0 {
                    break;
                }
                log::info!("[job {}] waiting on {} download(s)", job.id, pending);
            }
            Err(e) => log::warn!("[job {}] deemix unreachable while polling: {}", job.id, e),
        }
        polls += 1;
        if polls >= DOWNLOAD_MAX_POLLS {
            log::warn!("[job {}] download wait timed out, proceeding with what's done", job.id);
            break;
        }
        // Every 10 polls (~5 min) send/edit a progress message so the user
        // knows the job is still running.
        if polls % 10 == 0 {
            let text = format!(
                "⏳ Playlist \"{}\" — {} track(s) still downloading. Next check in 30 s.",
                job.name, last_pending
            );
            match progress_msg {
                Some(mid) => { let _ = bot.edit_message_text(chat, mid, &text).await; }
                None => if let Ok(m) = bot.send_message(chat, &text).await { progress_msg = Some(m.id); }
            }
        }
        tokio::time::sleep(DOWNLOAD_POLL_INTERVAL).await;
    }
    if let Some(mid) = progress_msg {
        let _ = bot.delete_message(chat, mid).await;
    }

    // Tracks whose download failed are excluded from the playlist.
    // Missing from the queue (cleared) or empty uuid counts as done.
    let (done, dl_failed): (Vec<&JobTrack>, Vec<&JobTrack>) = job.tracks.iter()
        .partition(|t| last_map.get(&t.uuid).map(|s| s.as_str()) != Some("failed"));

    let server = match &state.media {
        Some(s) => s.clone(),
        None => {
            // Media server was unconfigured between persisting and resuming
            remove(&state, &job.id);
            return;
        }
    };

    if done.is_empty() {
        let _ = bot.send_message(chat, format!(
            "😕 Playlist \"{}\": all downloads failed, nothing to rebuild in {}.",
            job.name, server.label())).await;
        remove(&state, &job.id);
        return;
    }

    // ── Phase 2: library scan ──
    let mut session = None;
    for attempt in 0..3 {
        match server.connect(&state.http).await {
            Ok(s) => { session = Some(s); break; }
            Err(e) => {
                log::warn!("[job {}] can't reach {} (attempt {}): {}", job.id, server.label(), attempt + 1, e);
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(300)).await;
                }
            }
        }
    }
    let session = match session {
        Some(s) => s,
        None => {
            let _ = bot.send_message(chat, format!(
                "⚠️ Playlist \"{}\" downloaded, but I couldn't reach {}.\nThe job is saved and will retry on the next bot restart.",
                job.name, server.label())).await;
            // Keep the job persisted so a restart retries it
            return;
        }
    };
    session.trigger_scan(&state.http).await;
    // Always wait at least 60 s before polling scan status — Jellyfin has no
    // scan-status endpoint so the poll loop exits immediately, and other
    // servers need time to discover the new files before reporting "scanning".
    tokio::time::sleep(Duration::from_secs(60)).await;
    let mut scan_polls = 0usize;
    while session.scan_in_progress(&state.http).await && scan_polls < SCAN_MAX_POLLS {
        scan_polls += 1;
        tokio::time::sleep(SCAN_POLL_INTERVAL).await;
    }

    // ── Phase 3: match tracks in the library ──
    let mut matched: Vec<Option<String>> = vec![None; done.len()];
    for attempt in 0..MATCH_ATTEMPTS {
        for (i, track) in done.iter().enumerate() {
            if matched[i].is_some() {
                continue;
            }
            matched[i] = session.find_track(&state.http, &track.title, &track.artist).await;
        }
        if matched.iter().all(|m| m.is_some()) {
            break;
        }
        if attempt + 1 < MATCH_ATTEMPTS {
            tokio::time::sleep(MATCH_RETRY_INTERVAL).await;
        }
    }

    let ids: Vec<String> = matched.iter().flatten().cloned().collect();
    let unmatched: Vec<&&JobTrack> = done.iter().zip(&matched)
        .filter(|(_, m)| m.is_none())
        .map(|(t, _)| t)
        .collect();

    // ── Phase 4: rebuild the playlist ──
    if ids.is_empty() {
        let _ = bot.send_message(chat, format!(
            "😕 Playlist \"{}\": none of the downloaded tracks showed up in the {} library, so I couldn't build the playlist.",
            job.name, server.label())).await;
        remove(&state, &job.id);
        return;
    }

    let mut text = match session.rebuild_playlist(&state.http, &job.name, &ids).await {
        Ok(_) => format!(
            "🎧 Playlist \"{}\" rebuilt in {}: {}/{} tracks.",
            job.name, server.label(), ids.len(), job.tracks.len()),
        Err(e) => format!(
            "❌ Playlist \"{}\": failed to create the playlist in {}: {}",
            job.name, server.label(), e),
    };
    if !dl_failed.is_empty() {
        text.push_str(&format!("\n\n⚠️ {} download(s) failed:\n", dl_failed.len()));
        push_track_list(&mut text, dl_failed.iter().map(|t| *t));
    }
    if !unmatched.is_empty() {
        text.push_str(&format!("\n\n😕 {} track(s) not found in the library:\n", unmatched.len()));
        push_track_list(&mut text, unmatched.iter().map(|t| **t));
    }
    let _ = bot.send_message(chat, text).await;
    remove(&state, &job.id);
    log::info!("[job {}] finished", job.id);
}

fn push_track_list<'a>(text: &mut String, tracks: impl Iterator<Item = &'a JobTrack>) {
    let tracks: Vec<&JobTrack> = tracks.collect();
    for t in tracks.iter().take(10) {
        let label = if t.artist.is_empty() { t.title.clone() } else { format!("{} — {}", t.title, t.artist) };
        let label = if label.chars().count() > 80 {
            format!("{}…", label.chars().take(79).collect::<String>())
        } else {
            label
        };
        text.push_str(&format!("• {}\n", label));
    }
    if tracks.len() > 10 {
        text.push_str(&format!("…and {} more\n", tracks.len() - 10));
    }
}
