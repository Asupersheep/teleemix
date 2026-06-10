<p align="center">
  <img src="logo.png" alt="Teleemix Logo" width="180"/>
</p>

<h1 align="center">Teleemix</h1>

<p align="center">
  <em>Your music, your server, your phone. No friction.</em>
</p>

<p align="center">
  <a href="https://github.com/asupersheep/teleemix/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License"></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/built%20with-Rust-orange.svg" alt="Built with Rust"></a>
  <a href="https://github.com/asupersheep/teleemix/releases"><img src="https://img.shields.io/github/v/release/asupersheep/teleemix" alt="Release"></a>
  <a href="https://github.com/asupersheep/teleemix/pkgs/container/teleemix"><img src="https://img.shields.io/badge/docker-ghcr.io-blue" alt="Docker"></a>
  <a href="https://github.com/asupersheep/teleemix/wiki"><img src="https://img.shields.io/badge/docs-wiki-green" alt="Docs"></a>
</p>

<p align="center">
  <a href="https://github.com/asupersheep/teleemix/wiki">→ Full documentation on the Wiki</a>
</p>

---

A fast, lightweight Telegram bot written in Rust for requesting music downloads via your self-hosted [deemix](https://github.com/bambanah/deemix) instance — from your phone, without ever touching the deemix web UI or dealing with ARL logins.

Supports Deezer URLs, Spotify links, YouTube, YouTube Music, Apple Music, voice search, song recognition, and free-text search.

---

## Features

- 🎵 Send a **Deezer URL** (track, album, playlist) → queued instantly
- 🔗 Send a **Spotify link** (track or album) → looked up and queued automatically
- 🎬 Send a **YouTube or YouTube Music link** → resolved via Odesli and queued on Deezer
- 🍎 Send an **Apple Music link** → resolved via Odesli and queued on Deezer
- 📥 **Import a playlist** — send a Spotify or YouTube playlist link and every track is scanned, found on Deezer, and queued individually (works for user-generated playlists)
- 🎧 **Clone a playlist** — same as import, plus the playlist is rebuilt with the same name in your media server (**Plex/Plexamp, Jellyfin, or Navidrome**) once all downloads finish; pending clones survive bot restarts
- 🔍 Send a **song or artist name** → search results shown as buttons to pick from
- 🎤 Send a **voice note** → transcribe what you said and search (requires OpenAI key)
- 🎵 Send a **voice recording of a song** → identify it and queue it (requires AudD key)
- 📲 `/menu` — quick action keyboard buttons
- ⚙️ `/settings` — per-user settings with toggle buttons
- 💿 `/album` — search albums
- 🔔 Restart notifications — opt-in per user via /settings
- 🔄 `/updatearl` — update your Deezer ARL interactively via Telegram
- 📊 `/status` — shows pending, downloading, and completed queue items separately
- 🧹 `/clearqueue` — clear completed downloads from the deemix queue
- 🔒 Optional user allowlist to restrict access
- ⚡ Written in Rust — tiny memory footprint, static binary, no runtime dependencies

---

## Requirements

- A running [deemix](https://github.com/bambanah/deemix) instance
- Docker + Docker Compose
- A Telegram bot token (from [@BotFather](https://t.me/BotFather))
- A Deezer account (free or paid; paid required for lossless)

---

## Setup

### 1. Create a Telegram bot

1. Open Telegram and message [@BotFather](https://t.me/BotFather)
2. Send `/newbot` and follow the prompts
3. Copy the token it gives you

### 2. Get your Deezer ARL

For step-by-step instructions on how to find your Deezer ARL token in your browser, see this guide: [How to Get Your Deezer ARL](https://www.dumpmedia.com/deezplus/deezer-arl.html). Alternatively, a clean browser-based guide is available [here](https://github.com/nathom/streamrip/wiki/Finding-Your-Deezer-ARL-Cookie).

The ARL lasts several months. When it expires, use `/updatearl` in Telegram to update it without touching the server.

### 3. Configure

```bash
cp .env.example .env
nano .env
```

Fill in all the values — see `.env.example` for descriptions of each variable.

> ⚠️ Teleemix requires deemix to run with `DEEMIX_SINGLE_USER=true`. Without this, deemix runs in multi-user mode and Teleemix will get `NotLoggedIn` errors. Add this to your deemix environment variables:
> ```
> DEEMIX_SINGLE_USER=true
> ```

### 4. Configure volume mounts

Edit `docker-compose.yml` and update the left side of these volume mounts to match your setup:

```yaml
volumes:
  - /path/to/your/docker-compose.yml:/compose/docker-compose.yml
  - /path/to/your/.env:/app/.env
  - /path/to/your/data/registered_users.txt:/app/registered_users.txt
  - /var/run/docker.sock:/var/run/docker.sock
```

Create the registered users and jobs files before starting (Docker creates directories instead of files if they don't exist):
```bash
touch /path/to/your/data/registered_users.txt
echo '[]' > /path/to/your/data/jobs.json
```

### 5. Deploy

```bash
docker compose pull
docker compose up -d
```

---

## Usage

| Action | How |
|---|---|
| Download a track | Send a Deezer, Spotify, YouTube, or Apple Music link, or just type the song name |
| Import a playlist | Send a Spotify/YouTube playlist link, or tap 📥 Import playlist in /menu |
| Clone a playlist to your media server | Send a playlist link and pick 🎧 Clone, or tap 🎧 Clone playlist in /menu |
| Search tracks | `/search` or tap 🔍 Search a track in /menu |
| Search albums | `/album` or tap 💿 Search an album in /menu |
| Download a Deezer URL | `/dl` |
| Download from a streaming service | `/sp` or tap 🔗 From streaming link in /menu |
| Voice search | Send a voice note (if configured) |
| Song recognition | Send a voice recording (if configured) |
| Check deemix status | `/status` |
| Clear completed downloads | `/clearqueue` or tap 🧹 Clear queue in /menu |
| Update ARL | `/updatearl` |
| Personal settings | `/settings` |
| Show all buttons | `/menu` |

---

## Playlist Import & Clone

Send any **Spotify or YouTube playlist link** (or use the 📥 / 🎧 buttons in `/menu`):

- **📥 Import** — the playlist is scanned (no API keys needed), each track is searched on Deezer, and the first match is queued. Works for user-generated playlists. You get a progress message and a summary of anything that couldn't be found.
- **🎧 Clone** — everything Import does, *plus* the playlist is rebuilt with the same name in your media server once all downloads finish. If every track was already downloaded before, the playlist is still rebuilt immediately — no waiting.

Notes: playlist pages expose roughly the first 100 tracks; YouTube auto-generated mixes (`list=RD...`) can't be scanned and are treated as a single video.

### Setting up Clone (optional)

Cloning needs a media server. Set `MEDIA_SERVER` in your `.env` to `plex`, `jellyfin`, or `navidrome` and fill in the matching variables (see `.env.example`):

| Server | Variables | Notes |
|---|---|---|
| Plex | `PLEX_URL`, `PLEX_TOKEN` | Playlists show up in **Plexamp** automatically. [Finding your token](https://support.plex.tv/articles/204059436-finding-an-authentication-token-x-plex-token/) |
| Jellyfin | `JELLYFIN_URL`, `JELLYFIN_API_KEY`, `JELLYFIN_USER_ID` | API key: Dashboard → API Keys. User id: the `userId=...` part of the URL on the user's profile page. Playlists are created for that user. |
| Navidrome | `NAVIDROME_URL`, `NAVIDROME_USER`, `NAVIDROME_PASSWORD` | Uses the Subsonic API, so any Subsonic-compatible server (Airsonic, gonic, LMS) works too. |

How a clone works behind the scenes: the bot tracks each queued download in the deemix queue, waits until they all finish, triggers a library scan, matches every track by its Deezer title + artist (deemix tags files with Deezer metadata), then deletes any same-named playlist and recreates it. The result message tells you how many tracks made it, plus which downloads failed or couldn't be matched.

Pending clone jobs are saved to `jobs.json` (the `JOBS_FILE` mount) and **resume automatically after a bot restart**. If the media server is unreachable when downloads finish, the bot retries for 15 minutes and otherwise picks the job up again on the next restart.

---

## Voice Features (optional)

### Voice Search — Transcribe spoken song names

Send a voice note saying a song or artist name. Teleemix transcribes it and searches Deezer.

Three backend options — choose one:

**Option 1 — OpenAI remote API** (easiest, pay-per-use ~$0.006/min)
```
OPENAI_API_KEY=sk-your-key-here
```
Sign up at [platform.openai.com](https://platform.openai.com/).

**Option 2 — Local compatible server** (any OpenAI-compatible Whisper server)
```
WHISPER_URL=http://your-whisper-server:8000/v1/audio/transcriptions
```
Works with [faster-whisper-server](https://github.com/fedirz/faster-whisper-server), [whisper.cpp](https://github.com/ggerganov/whisper.cpp), or any compatible server.

**Option 3 — Built-in local Whisper** (no API key needed, runs in Docker)

Uncomment the `whisper` service block in `docker-compose.yml`, then set:
```
WHISPER_URL=http://whisper:8000/v1/audio/transcriptions
```

The default model is `small` which supports 99 languages including Dutch, English, and Arabic. To change the model, update `WHISPER__MODEL` in the compose file:

| Model | Size | Speed | Accuracy |
|---|---|---|---|
| `tiny` | ~75MB | Fastest | Basic |
| `base` | ~145MB | Fast | Good |
| `small` | ~460MB | Balanced | **Recommended** |
| `medium` | ~1.5GB | Slow | High |
| `large-v3` | ~3GB | Slowest | Best |

After changing the model, recreate the container:
```bash
docker compose up -d --force-recreate whisper
```

---

### Song Recognition — Identify a song from a recording

Send a voice recording of a song playing. Teleemix identifies the song using [AudD](https://audd.io/) and queues it.

```
AUDD_API_KEY=your-key-here
```

Free tier gives **100 recognitions/month**. Sign up at [audd.io](https://audd.io/).

---

Both features are **optional** — leave keys/URLs empty to disable. Users can toggle them individually in `/settings`.

---

## Download Quality

The default download quality is set via `DEEMIX_BITRATE` in your `.env`:

| Value | Quality | Requirement |
|---|---|---|
| `9` | FLAC (lossless) | Deezer HiFi / Premium+ |
| `3` | MP3 320kbps | Deezer Premium |
| `1` | MP3 128kbps | Free accounts |

Users can change the quality on the fly via `/settings` → 🎚️ Quality. Each tap cycles through the options.

> ⚠️ Quality changes in /settings affect **all users** on the server immediately, since the setting is shared in memory. The change resets to the `DEEMIX_BITRATE` default when the container restarts.

To lock the quality and prevent users from changing it, set:
```
DEEMIX_BITRATE_LOCK=true
```

When locked, the quality button in /settings shows a 🔒 and tapping it shows a message that it is administrator-locked.

---

## Per-User Settings

Every user has their own settings managed via `/settings`:

| Setting | Default | Description |
|---|---|---|
| 🔔 Restart notifications | OFF | Get notified when the bot container restarts |
| 🎤 Voice search | ON | Transcribe voice notes to search (requires OpenAI key) |
| 🎵 Song recognition | ON | Identify songs from recordings (requires AudD key) |
| 🎚️ Download quality | env default | Cycle between FLAC, MP3 320, MP3 128 (affects all users) |

Settings are stored in `users.json` and persist across container restarts.

---

## Updating

### Manual update

```bash
docker compose pull
docker compose up -d
```

### Automatic updates with Watchtower

If you want Teleemix to update itself automatically whenever a new image is published, you can use [Watchtower](https://github.com/containrrr/watchtower) or an alternative like [Diun](https://github.com/crazy-max/diun).

Watchtower example — add to your compose file:

```yaml
  watchtower:
    image: containrrr/watchtower
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
    command: --interval 86400 teleemix
    restart: unless-stopped
```

This checks for a new `teleemix` image once a day and updates automatically.

> ⚠️ Only recommended if you trust the source. For production use, pin to a specific image digest instead.

---

## Access Control

Teleemix relies on Telegram's built-in access control rather than maintaining its own allowlist. Since only users who know your bot's username can message it, keeping your bot token private is the main security measure.

To restrict access further via @BotFather:
1. Message [@BotFather](https://t.me/BotFather) → `/mybots` → select your bot
2. **Bot Settings → Allow Groups** — disable to make it private message only
3. **Bot Settings → Group Privacy** — controls what the bot can see in groups

For more detail see the [Telegram Bot documentation](https://core.telegram.org/bots/features#privacy-mode) and the project wiki.

---

## Notifications on restart

Enable "🔔 Restart notifications" in `/settings`. From then on, every time the container restarts you will receive a message letting you know the bot is back online.

---

## Updating the ARL

When your Deezer ARL expires, send `/updatearl` to the bot. It will prompt you to send the new ARL, then:
1. Re-logs into deemix immediately
2. Updates the ARL in your `.env` file so it persists across restarts

---

## Docker tags

| Tag | Branch | Description |
|---|---|---|
| `latest` | `main` | Stable release |
| `dev` | `dev` | Experimental / in development |

To run the experimental version, change the image tag in `docker-compose.yml`:

```yaml
image: ghcr.io/asupersheep/teleemix:dev
```

---

## Built with

- [Rust](https://www.rust-lang.org/)
- [teloxide](https://github.com/teloxide/teloxide) — Telegram bot framework
- [tokio](https://tokio.rs/) — async runtime
- [reqwest](https://github.com/seanmonstar/reqwest) — HTTP client
