# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
# Local development (requires .env file)
cargo run

# Build release binary
cargo build --release

# Build static musl binary (matches Docker/production)
cargo build --release --target x86_64-unknown-linux-musl

# Run tests
cargo test

# Docker build
docker build -t teleemix .

# Deploy (on production server at /opt/yams/)
docker compose pull teleemix && docker compose up -d --force-recreate teleemix
docker logs teleemix --tail 20
```

## Architecture

Teleemix is a Telegram bot that lets users request music downloads from a self-hosted [deemix](https://deemix.app/) instance. Written in Rust using **teloxide** (Telegram framework) + **tokio** (async runtime) + **reqwest** (HTTP).

### Module responsibilities

| File | Purpose |
|---|---|
| `src/main.rs` | Bot entry point, dialogue state machine, command handlers, keyboard UI, `Config`/`BotState` structs |
| `src/deemix.rs` | deemix API client: login with ARL, queue downloads, search, poll queue status |
| `src/spotify.rs` | Spotify track/album/playlist resolver via embed page scraping — no API key required |
| `src/youtube.rs` | YouTube playlist scanner via `ytInitialData` scraping — no API key required |
| `src/users.rs` | Per-user settings persistence backed by `users.json` |
| `src/voice.rs` | Whisper transcription (OpenAI or local) + AudD song recognition |
| `src/mediaserver.rs` | Plex / Jellyfin / Navidrome (Subsonic) clients: scan, track search, playlist rebuild |
| `src/jobs.rs` | Persistent playlist-rebuild jobs (`jobs.json`), background worker, resumed on startup |

### Shared state (`BotState`)

All handlers receive a clone of `BotState` which holds:
- `config: Config` — static env-loaded settings
- `http: reqwest::Client` — shared HTTP client with `cookie_store(true)` for deemix session
- `users: UsersDb` — `Arc<RwLock<HashMap<String, UserSettings>>>` backed by users.json
- `bitrate: Arc<Mutex<u8>>` — app-wide quality setting, mutable at runtime via `/settings`
- `pending_voices: Arc<Mutex<HashMap<String, String>>>` — short_id → Telegram file_id (see below)
- `current_arl: Arc<Mutex<String>>` — active ARL (updated via /updatearl), used for automatic session re-login
- `media: Option<MediaServer>` — playlist rebuild target, built from `MEDIA_SERVER` env (None = feature off)
- `jobs: JobsDb` — `Arc<RwLock<Vec<PlaylistJob>>>` backed by jobs.json

### Dialogue state machine

teloxide's `Dialogue<State, InMemStorage<State>>` drives all multi-step interactions:

```
Idle → AwaitingSearch / AwaitingAlbum / AwaitingDl / AwaitingSpotify
     → AwaitingPlaylistImport / AwaitingPlaylistClone (menu buttons; clone only when MEDIA_SERVER set)
     → AwaitingArl (via /updatearl)
     → AwaitingVoiceTranscribe / AwaitingVoiceRecognize (voice notes, 60s timeout)
```

All states return to `Idle` after handling. Voice notes received in `Idle` show inline choice buttons (transcribe vs. recognize); voice notes received in `AwaitingVoice*` states are processed directly.

## Key Design Decisions

**Static musl binary** — compiled with `--target x86_64-unknown-linux-musl`, runs from a `scratch` Docker image with only CA certs and docker CLI. All TLS via rustls (no OpenSSL dependency).

**deemix session auth** — deemix uses cookie-based auth (`connect.sid`). The bot calls `/api/loginArl` on startup to establish the session. **deemix must run with `DEEMIX_SINGLE_USER=true`** or all POST requests return `NotLoggedIn`. deemix's session store is an in-memory store with a ~24h TTL, so the bot's session eventually expires; `deemix::add_to_queue` detects `NotLoggedIn`, re-logs-in with `current_arl`, and retries once.

**No Spotify API** — Spotify links are resolved by scraping `open.spotify.com/embed/track/ID?utm_source=oembed` for `__NEXT_DATA__` JSON. Falls back to oEmbed for title-only. Single YouTube, YouTube Music, and Apple Music links are resolved via the Odesli API (song.link) to get a Deezer URL and queued directly.

**Playlist scanning** — Spotify playlists are scanned from the embed page `trackList` (works for user-generated playlists; ~first 100 tracks). YouTube playlists are scanned from the playlist page's `ytInitialData` JSON (~first 100 videos; auto-generated mixes `list=RD...` can't be scanned). Each track is searched on Deezer and the first match queued individually, with a progress/summary message. Queue-removal endpoints: `removeFromQueue` takes `uuid` as a **query param** (not JSON body); `removeFinishedDownloads` clears `status == "completed"` items.

**Voice callback IDs** — Telegram callback data has a 64-byte limit. Voice note file IDs are stored in `pending_voices` HashMap keyed by short IDs; callbacks use `vt:{id}` (transcribe) and `vr:{id}` (recognize).

**AudD Deezer link** — AudD's response includes a `deezer.link` field used directly instead of searching by title. This avoids transliteration failures for Arabic/non-Latin songs.

**Runtime bitrate** — stored in `Arc<Mutex<u8>>`, app-wide (not per-user), resets to `DEEMIX_BITRATE` env var on restart. `DEEMIX_BITRATE_LOCK=true` disables user changes.

**Import vs. clone** — pasting a playlist link with `MEDIA_SERVER` configured shows an inline choice: import (download only) or clone (download + rebuild job). The URL is stashed in `pending_playlists` keyed by a short id (`pi:{id}`/`pc:{id}` callbacks, 64-byte limit). Cloning a fully-already-downloaded playlist skips the download wait and rebuilds immediately.

**Playlist rebuild jobs** — when cloning, `queue_playlist` records each queued track's deemix uuid (from the `addToQueue` response `data.obj`) plus its **Deezer** title/artist (deemix tags files with Deezer metadata, so that's what the media server indexes). A background tokio task polls `getQueue` until all uuids are terminal (uuid missing from queue = done, e.g. cleared), triggers a library scan, matches tracks by title+artist with retries, then deletes and recreates the same-named playlist. Jobs are persisted to `jobs.json` and respawned by `jobs::resume_all` on startup. Plex needs `machineIdentifier` + music section key (resolved in `MediaServer::connect`); Jellyfin playlist creation requires a `UserId`; Navidrome uses the Subsonic API with salted-md5 token auth.

## Environment Variables

Copy `.env.example` to `.env`. Required variables:

| Variable | Notes |
|---|---|
| `TELEGRAM_TOKEN` | From @BotFather |
| `DEEMIX_URL` | Default: `http://localhost:6595` |
| `DEEMIX_ARL` | ~190-char Deezer ARL token |
| `DEEMIX_BITRATE` | `9`=FLAC, `3`=MP3 320, `1`=MP3 128 (default: 9) |
| `AUDD_API_KEY` | Optional: enables song recognition |
| `OPENAI_API_KEY` | Optional: enables Whisper via OpenAI |
| `WHISPER_URL` | Optional: enables local Whisper-compatible server |
| `MEDIA_SERVER` | Optional: `plex`, `jellyfin` or `navidrome` — enables playlist rebuild |
| `PLEX_URL` / `PLEX_TOKEN` | Required when `MEDIA_SERVER=plex` |
| `JELLYFIN_URL` / `JELLYFIN_API_KEY` / `JELLYFIN_USER_ID` | Required when `MEDIA_SERVER=jellyfin` |
| `NAVIDROME_URL` / `NAVIDROME_USER` / `NAVIDROME_PASSWORD` | Required when `MEDIA_SERVER=navidrome` |
| `JOBS_FILE` | Playlist job persistence (default: `/app/jobs.json`) |

## CI/CD

GitHub Actions (`.github/workflows/build.yml`) triggers on push to `main` or `dev`, builds the musl binary inside Docker, and pushes to `ghcr.io/asupersheep/teleemix:latest` (main) and `:dev` (dev branch).

## Branch Strategy

- `main` — stable tagged releases (currently v1.1.0)
- `dev` — active development; PRs target this branch

## Known Pending Work

- Test v1.2 on dev with a real Arabic/Egyptian song before merging to main and tagging v1.2.0.
