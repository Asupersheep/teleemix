use std::env;
use std::sync::Arc;
use std::collections::HashMap;
use tokio::sync::Mutex;

use regex::Regex;
use reqwest::Client;
use teloxide::{
    dispatching::dialogue::InMemStorage,
    prelude::*,
    types::{
        InlineKeyboardButton, InlineKeyboardMarkup,
        KeyboardButton, KeyboardMarkup,
    },
    utils::command::BotCommands,
};

mod spotify;
mod deemix;
mod users;
mod voice;
mod youtube;
mod mediaserver;
mod jobs;

use users::{UsersDb, UserSettings};

// ── Dialogue State ────────────────────────────────────────────────────────────

#[derive(Clone, Default, Debug)]
pub enum State {
    #[default]
    Idle,
    AwaitingArl,
    AwaitingSearch,
    AwaitingAlbum,
    AwaitingDl,
    AwaitingSpotify,
    AwaitingPlaylistImport,
    AwaitingPlaylistClone,
    AwaitingVoiceTranscribe,
    AwaitingVoiceRecognize,
}

type MyDialogue = Dialogue<State, InMemStorage<State>>;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Config {
    pub deemix_url: String,
    pub deemix_arl: String,
    pub compose_file: String,
    pub service_name: String,
    pub env_file: String,
    pub users_file: String,
    pub jobs_file: String,
    pub audd_api_key: String,
    pub openai_api_key: String,
    pub whisper_url: String,
    pub deemix_bitrate: u8,
    pub deemix_bitrate_lock: bool,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            deemix_url: env::var("DEEMIX_URL")
                .unwrap_or_else(|_| "http://localhost:6595".to_string()),
            deemix_arl: env::var("DEEMIX_ARL").unwrap_or_default(),
            compose_file: env::var("COMPOSE_FILE")
                .unwrap_or_else(|_| "/compose/docker-compose.yml".to_string()),
            service_name: env::var("SERVICE_NAME")
                .unwrap_or_else(|_| "teleemix".to_string()),
            env_file: env::var("ENV_FILE")
                .unwrap_or_else(|_| "/app/.env".to_string()),
            users_file: env::var("USERS_FILE")
                .unwrap_or_else(|_| "/app/users.json".to_string()),
            jobs_file: env::var("JOBS_FILE")
                .unwrap_or_else(|_| "/app/jobs.json".to_string()),
            audd_api_key: env::var("AUDD_API_KEY").unwrap_or_default(),
            openai_api_key: env::var("OPENAI_API_KEY").unwrap_or_default(),
            whisper_url: env::var("WHISPER_URL").unwrap_or_default(),
            deemix_bitrate: env::var("DEEMIX_BITRATE").unwrap_or_else(|_| "9".to_string()).parse().unwrap_or(9),
            deemix_bitrate_lock: env::var("DEEMIX_BITRATE_LOCK").unwrap_or_else(|_| "false".to_string()).to_lowercase() == "true",
        }
    }


    pub fn audd_enabled(&self) -> bool { !self.audd_api_key.is_empty() }
    pub fn whisper_enabled(&self) -> bool { !self.openai_api_key.is_empty() || !self.whisper_url.is_empty() }
}

// ── Bot State ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct BotState {
    pub config: Arc<Config>,
    pub http: Client,
    pub users: UsersDb,
    pub pending_voices: Arc<Mutex<HashMap<String, String>>>, // short_id -> file_id
    pub pending_playlists: Arc<Mutex<HashMap<String, String>>>, // short_id -> playlist url (import/clone choice)
    pub current_bitrate: Arc<Mutex<u8>>, // runtime-changeable bitrate
    pub current_arl: Arc<Mutex<String>>, // updated via /updatearl, used for auto re-login
    pub media: Option<mediaserver::MediaServer>, // optional playlist rebuild target
    pub jobs: jobs::JobsDb, // pending playlist rebuild jobs, persisted to jobs.json
}

impl BotState {
    pub fn new(config: Config, users: UsersDb) -> Self {
        let http = Client::builder()
            .cookie_store(true)
            .build()
            .expect("Failed to build HTTP client");
        let default_bitrate = config.deemix_bitrate;
        let default_arl = config.deemix_arl.clone();
        let media = mediaserver::MediaServer::from_env();
        if let Some(ref m) = media {
            log::info!("Playlist rebuild enabled for {}", m.label());
        }
        let jobs = jobs::load(&config.jobs_file);
        Self {
            config: Arc::new(config),
            http,
            users,
            pending_voices: Arc::new(Mutex::new(HashMap::new())),
            pending_playlists: Arc::new(Mutex::new(HashMap::new())),
            current_bitrate: Arc::new(Mutex::new(default_bitrate)),
            current_arl: Arc::new(Mutex::new(default_arl)),
            media,
            jobs,
        }
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Teleemix commands:")]
enum Command {
    #[command(description = "Welcome message")]
    Start,
    #[command(description = "Show all commands and info")]
    Help,
    #[command(description = "Check deemix status")]
    Status,
    #[command(description = "Queue a Deezer URL")]
    Dl,
    #[command(description = "Search for a track")]
    Search,
    #[command(description = "Search for an album")]
    Album,
    #[command(description = "Download from a streaming link (Spotify, YouTube, Apple Music)")]
    Sp,
    #[command(description = "Clear completed downloads from queue")]
    Clearqueue,
    #[command(description = "Show quick action buttons")]
    Menu,
    #[command(description = "Show settings")]
    Settings,
    #[command(description = "Update Deezer ARL")]
    Updatearl,
}

// ── URL Patterns ──────────────────────────────────────────────────────────────

lazy_static::lazy_static! {
    static ref DEEZER_URL_RE: Regex = Regex::new(
        r"https?://(?:www\.)?deezer\.com/(?:[a-z]+/)?(track|album|playlist|artist)/(\d+)"
    ).unwrap();
    static ref DEEZER_SHORT_RE: Regex = Regex::new(
        r"https?://link\.deezer\.com/s/\S+"
    ).unwrap();
    static ref SPOTIFY_TRACK_RE: Regex = Regex::new(
        r"https?://open\.spotify\.com/track/([A-Za-z0-9]+)"
    ).unwrap();
    static ref SPOTIFY_ALBUM_RE: Regex = Regex::new(
        r"https?://open\.spotify\.com/album/([A-Za-z0-9]+)"
    ).unwrap();
    static ref SPOTIFY_PLAYLIST_RE: Regex = Regex::new(
        r"https?://open\.spotify\.com/playlist/[A-Za-z0-9]+"
    ).unwrap();
    static ref YOUTUBE_RE: Regex = Regex::new(
        r"https?://(?:(?:www\.)?youtube\.com/|youtu\.be/|music\.youtube\.com/)"
    ).unwrap();
    static ref APPLE_MUSIC_RE: Regex = Regex::new(
        r"https?://music\.apple\.com/"
    ).unwrap();
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    pretty_env_logger::init();

    let config = Config::from_env();
    let users = users::load(&config.users_file);
    let state = Arc::new(BotState::new(config, users));

    let token = env::var("TELEGRAM_TOKEN").expect("TELEGRAM_TOKEN must be set");
    let bot = Bot::new(token);

    deemix::login(&state).await;

    log::info!("Teleemix bot starting...");

    // Resume playlist rebuild jobs that were pending before the last restart
    jobs::resume_all(&bot, &state);

    // Send startup notification to users with restart_notifications enabled
    {
        let startup_msg = "🎵 Teleemix is back online!\n\nTap /menu for quick actions.";
        for chat_id in users::all_with_notifications(&state.users) {
            let _ = bot.send_message(teloxide::types::ChatId(chat_id), startup_msg).await;
        }
    }

    let storage = InMemStorage::<State>::new();

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .enter_dialogue::<Message, InMemStorage<State>, State>()
                .branch(dptree::case![State::AwaitingArl].endpoint(receive_arl))
                .branch(dptree::case![State::AwaitingSearch].endpoint(receive_search))
                .branch(dptree::case![State::AwaitingAlbum].endpoint(receive_album))
                .branch(dptree::case![State::AwaitingDl].endpoint(receive_dl))
                .branch(dptree::case![State::AwaitingSpotify].endpoint(receive_spotify))
                .branch(dptree::case![State::AwaitingPlaylistImport].endpoint(receive_playlist_import))
                .branch(dptree::case![State::AwaitingPlaylistClone].endpoint(receive_playlist_clone))
                .branch(dptree::case![State::AwaitingVoiceTranscribe].endpoint(receive_voice_transcribe))
                .branch(dptree::case![State::AwaitingVoiceRecognize].endpoint(receive_voice_recognize))
                .branch(
                    Update::filter_message()
                        .filter_command::<Command>()
                        .endpoint(handle_command),
                )
                .branch(
                    Update::filter_message()
                        .endpoint(handle_message),
                ),
        )
        .branch(
            Update::filter_callback_query()
                .endpoint(handle_callback),
        );

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![Arc::clone(&state), storage])
        .build()
        .dispatch()
        .await;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn user_id_from_msg(msg: &Message) -> i64 {
    msg.from().map(|u| u.id.0 as i64).unwrap_or(0)
}

fn settings_keyboard(s: &UserSettings, config: &Config, bitrate: u8) -> KeyboardMarkup {
    let notif = if s.restart_notifications { "🔔 Restart notifications: ON" } else { "🔕 Restart notifications: OFF" };
    let voice = if s.voice_search && config.whisper_enabled() { "🎤 Voice search: ON" } else { "🎤 Voice search: OFF" };
    let recog = if s.song_recognition && config.audd_enabled() { "🎵 Song recognition: ON" } else { "🎵 Song recognition: OFF" };
    let bitrate_btn = if config.deemix_bitrate_lock {
        format!("🔒 Quality: {} (locked)", bitrate_label(bitrate))
    } else {
        format!("🎚️ Quality: {} (tap to change)", bitrate_label(bitrate))
    };

    KeyboardMarkup::new(vec![
        vec![KeyboardButton::new(notif)],
        vec![KeyboardButton::new(voice)],
        vec![KeyboardButton::new(recog)],
        vec![KeyboardButton::new(bitrate_btn)],
        vec![KeyboardButton::new("🔑 Update ARL")],
        vec![KeyboardButton::new("🔙 Back to menu")],
    ])
    .resize_keyboard(true)
}

fn arl_cancel_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback("❌ Cancel", "cancel_arl")]])
}

fn main_keyboard(s: &UserSettings, state: &BotState) -> KeyboardMarkup {
    let config = &state.config;
    let mut rows = vec![
        vec![
            KeyboardButton::new("🔍 Search a track"),
            KeyboardButton::new("💿 Search an album"),
        ],
        vec![
            KeyboardButton::new("🔗 From streaming link"),
            KeyboardButton::new("🎵 From Deezer URL"),
        ],
    ];

    let mut playlist_row = vec![KeyboardButton::new("📥 Import playlist")];
    if state.media.is_some() {
        playlist_row.push(KeyboardButton::new("🎧 Clone playlist"));
    }
    rows.push(playlist_row);

    // Only show voice buttons if features are configured AND user has them enabled
    let show_voice = config.whisper_enabled() && s.voice_search;
    let show_recog = config.audd_enabled() && s.song_recognition;

    if show_voice || show_recog {
        let mut voice_row = vec![];
        if show_voice { voice_row.push(KeyboardButton::new("🎤 Voice search")); }
        if show_recog { voice_row.push(KeyboardButton::new("🎵 Recognize song")); }
        rows.push(voice_row);
    }

    rows.push(vec![
        KeyboardButton::new("📊 Check status"),
        KeyboardButton::new("🧹 Clear queue"),
    ]);
    rows.push(vec![
        KeyboardButton::new("⚙️ Settings"),
        KeyboardButton::new("ℹ️ Help"),
    ]);

    KeyboardMarkup::new(rows).resize_keyboard(true)
}

// ── Command Handler ───────────────────────────────────────────────────────────

async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    state: Arc<BotState>,
    dialogue: MyDialogue,
) -> ResponseResult<()> {
    // Auto-create user on any interaction
    let user_settings = users::get_or_create(&state.users, user_id_from_msg(&msg));
    users::save(&state.users, &state.config.users_file);

    match cmd {
        Command::Start => {
            let kb = main_keyboard(&user_settings, &state);
            bot.send_message(
                msg.chat.id,
                "👋 Hey! I'm Teleemix — your personal music download assistant.\n\nJust send me a song name, a Deezer link, or a Spotify link and I'll find it and queue it for download on your server. No technical stuff needed!\n\n📲 Use /menu to see quick action buttons.\n\nFor a full list of what I can do, type /help.",
            )
            .reply_markup(kb)
            .await?;
        }

        Command::Help => {
            bot.send_message(
                msg.chat.id,
                "ℹ️ Teleemix — Full Guide\n\n\
🎵 What I do:\n\
I connect to your personal deemix server and queue music downloads for you. Just tell me what you want!\n\n\
📥 Ways to request music:\n\
• Type any song or artist name → search and pick from results\n\
• Send a Deezer link (track, album, playlist) → queued instantly\n\
• Send a Spotify, YouTube, YouTube Music, or Apple Music link → found on Deezer and queued\n\
• Send a Spotify or YouTube playlist link → every track is scanned and queued individually\n\
• Send a voice note → transcribe what you said or recognize the song\n\n\
📜 Playlists:\n\
• 📥 Import playlist — downloads every song from a Spotify/YouTube playlist\n\
• 🎧 Clone playlist — same, plus rebuilds the playlist (same name) in your media server (Plex, Jellyfin or Navidrome) once all downloads finish — works even if everything was already downloaded\n\
Both are in /menu; pasting a playlist link directly also offers the choice.\n\n\
🔧 All commands:\n\
/menu — quick action buttons\n\
/search — search for a track\n\
/album — search for an album\n\
/dl — queue from a Deezer URL\n\
/sp — queue from a streaming link (Spotify, YouTube, Apple Music)\n\
/status — check deemix connection and queue\n\
/clearqueue — clear completed downloads from queue\n\
/settings — manage your personal preferences\n\
/updatearl — update your Deezer ARL\n\n\
⚙️ Settings (via /settings):\n\
• Restart notifications — get notified when the bot restarts\n\
• Voice search — transcribe voice notes to search\n\
• Song recognition — identify songs from voice recordings\n\n\
💡 Tip: You don't need commands — just send a song name or link directly!",
            )
            .await?;
        }

        Command::Status => {
            let mut text = String::new();
            match deemix::get_queue(&state).await {
                Ok(q) => {
                    text.push_str("✅ Deemix is reachable\n");
                    if q.downloading > 0 { text.push_str(&format!("⬇️ Downloading: {}\n", q.downloading)); }
                    if q.pending > 0 { text.push_str(&format!("⏳ Pending: {}\n", q.pending)); }
                    if q.failed > 0 { text.push_str(&format!("❌ Failed: {}\n", q.failed)); }
                    if q.done > 0 { text.push_str(&format!("✅ Completed (in queue): {}\n", q.done)); }
                    if q.downloading == 0 && q.pending == 0 && q.done == 0 && q.failed == 0 { text.push_str("📭 Queue is empty\n"); }
                }
                Err(e) => { text.push_str(&format!("❌ Can't reach deemix: {}\n", e)); }
            }
            if let Some(server) = &state.media {
                text.push('\n');
                match server.connect(&state.http).await {
                    Ok(_) => text.push_str(&format!("✅ {} is reachable", server.label())),
                    Err(e) => text.push_str(&format!("❌ Can't reach {}: {}", server.label(), e)),
                }
            }
            bot.send_message(msg.chat.id, text.trim_end().to_string()).await?;
        }

        Command::Dl => {
            dialogue.update(State::AwaitingDl).await
                .map_err(|e| teloxide::RequestError::Api(teloxide::ApiError::Unknown(e.to_string())))?;
            bot.send_message(msg.chat.id, "🎵 Send me a Deezer URL:").await?;
        }

        Command::Search => {
            dialogue.update(State::AwaitingSearch).await
                .map_err(|e| teloxide::RequestError::Api(teloxide::ApiError::Unknown(e.to_string())))?;
            bot.send_message(msg.chat.id, "🔍 What song or artist are you looking for?").await?;
        }

        Command::Album => {
            dialogue.update(State::AwaitingAlbum).await
                .map_err(|e| teloxide::RequestError::Api(teloxide::ApiError::Unknown(e.to_string())))?;
            bot.send_message(msg.chat.id, "💿 What album are you looking for?").await?;
        }

        Command::Sp => {
            dialogue.update(State::AwaitingSpotify).await
                .map_err(|e| teloxide::RequestError::Api(teloxide::ApiError::Unknown(e.to_string())))?;
            bot.send_message(msg.chat.id, "🔗 Send me a link from Spotify, YouTube, YouTube Music, or Apple Music:").await?;
        }

        Command::Clearqueue => {
            match deemix::clear_completed(&state).await {
                Ok(0) => { bot.send_message(msg.chat.id, "📭 No completed downloads to clear.").await?; }
                Ok(n) => { bot.send_message(msg.chat.id, format!("🧹 Cleared {} completed download(s) from queue.", n)).await?; }
                Err(e) => { bot.send_message(msg.chat.id, format!("❌ Failed to clear queue: {}", e)).await?; }
            }
        }

        Command::Menu => {
            let kb = main_keyboard(&user_settings, &state);
            bot.send_message(msg.chat.id, "Choose an action:").reply_markup(kb).await?;
        }

        Command::Settings => {
            let current_br = *state.current_bitrate.lock().await;
    let kb = settings_keyboard(&user_settings, &state.config, current_br);
            bot.send_message(msg.chat.id, "⚙️ Your settings — tap to toggle:").reply_markup(kb).await?;
        }

        Command::Updatearl => {
            dialogue.update(State::AwaitingArl).await
                .map_err(|e| teloxide::RequestError::Api(teloxide::ApiError::Unknown(e.to_string())))?;
            bot.send_message(msg.chat.id, "Please send your new Deezer ARL:")
                .reply_markup(arl_cancel_keyboard())
                .await?;
        }
    }

    Ok(())
}

// ── Dialogue Receivers ────────────────────────────────────────────────────────

async fn receive_search(bot: Bot, msg: Message, state: Arc<BotState>, dialogue: MyDialogue) -> ResponseResult<()> {
    dialogue.exit().await.ok();
    if let Some(query) = msg.text() { do_search(&bot, &msg, &state, query.trim(), "track").await?; }
    Ok(())
}

async fn receive_album(bot: Bot, msg: Message, state: Arc<BotState>, dialogue: MyDialogue) -> ResponseResult<()> {
    dialogue.exit().await.ok();
    if let Some(query) = msg.text() { do_search(&bot, &msg, &state, query.trim(), "album").await?; }
    Ok(())
}

async fn receive_dl(bot: Bot, msg: Message, state: Arc<BotState>, dialogue: MyDialogue) -> ResponseResult<()> {
    dialogue.exit().await.ok();
    if let Some(url) = msg.text() { queue_url(&bot, &msg, &state, url.trim()).await?; }
    Ok(())
}

async fn receive_spotify(bot: Bot, msg: Message, state: Arc<BotState>, dialogue: MyDialogue) -> ResponseResult<()> {
    dialogue.exit().await.ok();
    if let Some(url) = msg.text() { handle_streaming_link(&bot, &msg, &state, url.trim()).await?; }
    Ok(())
}

async fn receive_playlist_import(bot: Bot, msg: Message, state: Arc<BotState>, dialogue: MyDialogue) -> ResponseResult<()> {
    dialogue.exit().await.ok();
    if let Some(url) = msg.text() {
        receive_playlist_link(&bot, &msg, &state, url.trim(), false).await?;
    }
    Ok(())
}

async fn receive_playlist_clone(bot: Bot, msg: Message, state: Arc<BotState>, dialogue: MyDialogue) -> ResponseResult<()> {
    dialogue.exit().await.ok();
    if let Some(url) = msg.text() {
        // Defensive: the clone button is hidden when no media server is configured
        let rebuild = state.media.is_some();
        receive_playlist_link(&bot, &msg, &state, url.trim(), rebuild).await?;
    }
    Ok(())
}

async fn receive_playlist_link(bot: &Bot, msg: &Message, state: &Arc<BotState>, url: &str, rebuild: bool) -> ResponseResult<()> {
    if !is_playlist_link(url) {
        bot.send_message(msg.chat.id, "❌ That doesn't look like a Spotify or YouTube playlist link. Use /menu to try again.").await?;
        return Ok(());
    }
    let sent = bot.send_message(msg.chat.id, "🎵 Looking up playlist...").await?;
    start_playlist_flow(bot, msg.chat.id, state, sent.id, url, rebuild).await
}

async fn receive_arl(bot: Bot, msg: Message, state: Arc<BotState>, dialogue: MyDialogue) -> ResponseResult<()> {
    let arl = match msg.text() {
        Some(t) => t.trim().to_string(),
        None => { bot.send_message(msg.chat.id, "Please send the ARL as text.").await?; return Ok(()); }
    };

    if arl.len() < 100 {
        bot.send_message(msg.chat.id, "❌ That ARL looks too short, double check it. Try again:")
            .reply_markup(arl_cancel_keyboard())
            .await?;
        return Ok(());
    }

    dialogue.exit().await.ok();
    handle_updatearl(&bot, &msg, &state, &arl).await?;
    Ok(())
}

// ── Message Handler ───────────────────────────────────────────────────────────


async fn receive_voice_transcribe(bot: Bot, msg: Message, state: Arc<BotState>, dialogue: MyDialogue) -> ResponseResult<()> {
    dialogue.exit().await.ok();

    let voice = match msg.voice() {
        Some(v) => v,
        None => {
            bot.send_message(msg.chat.id, "⚠️ I expected a voice note. Use /menu to try again.").await?;
            return Ok(());
        }
    };

    let sent = bot.send_message(msg.chat.id, "🎤 Transcribing...").await?;

    // Download audio
    let file = bot.get_file(&voice.file.id).await
        .map_err(|e| teloxide::RequestError::Api(teloxide::ApiError::Unknown(e.to_string())))?;
    let url = format!("https://api.telegram.org/file/bot{}/{}", bot.token(), file.path);
    let audio_bytes = match state.http.get(&url).send().await {
        Ok(r) => r.bytes().await.map_err(|e| teloxide::RequestError::Api(teloxide::ApiError::Unknown(e.to_string())))?.to_vec(),
        Err(e) => { bot.edit_message_text(msg.chat.id, sent.id, format!("❌ Failed to download audio: {}", e)).await?; return Ok(()); }
    };

    match voice::transcribe(&state.http, audio_bytes, &state.config.openai_api_key, &state.config.whisper_url).await {
        Ok(text) if text.is_empty() => { bot.edit_message_text(msg.chat.id, sent.id, "😕 Could not transcribe anything. Try speaking more clearly.").await?; }
        Ok(text) => {
            bot.edit_message_text(msg.chat.id, sent.id, format!("🔍 I heard: {}\nSearching...", text)).await?;
            let sent2 = bot.send_message(msg.chat.id, format!("Results for: {}", text)).await?;
            match deemix::search(&state, &text, "track").await {
                Ok(results) if results.is_empty() => { bot.edit_message_text(msg.chat.id, sent2.id, format!("😕 No results for: {}", text)).await?; }
                Ok(results) => {
                    let (listing, mut buttons) = build_search_results(&results, "🎵");
                    buttons.push(vec![InlineKeyboardButton::callback("❌ Cancel", "cancel")]);
                    bot.edit_message_text(msg.chat.id, sent2.id, format!("Results for {}:\n\n{}\nTap a button to download.", text, listing))
                        .reply_markup(InlineKeyboardMarkup::new(buttons)).await?;
                }
                Err(e) => { bot.edit_message_text(msg.chat.id, sent2.id, format!("❌ Search failed: {}", e)).await?; }
            }
        }
        Err(e) => { bot.edit_message_text(msg.chat.id, sent.id, format!("❌ Transcription failed: {}", e)).await?; }
    }
    Ok(())
}

async fn receive_voice_recognize(bot: Bot, msg: Message, state: Arc<BotState>, dialogue: MyDialogue) -> ResponseResult<()> {
    dialogue.exit().await.ok();

    let voice = match msg.voice() {
        Some(v) => v,
        None => {
            bot.send_message(msg.chat.id, "⚠️ I expected a voice note. Use /menu to try again.").await?;
            return Ok(());
        }
    };

    let sent = bot.send_message(msg.chat.id, "🎵 Recognizing song...").await?;

    // Download audio
    let file = bot.get_file(&voice.file.id).await
        .map_err(|e| teloxide::RequestError::Api(teloxide::ApiError::Unknown(e.to_string())))?;
    let url = format!("https://api.telegram.org/file/bot{}/{}", bot.token(), file.path);
    let audio_bytes = match state.http.get(&url).send().await {
        Ok(r) => r.bytes().await.map_err(|e| teloxide::RequestError::Api(teloxide::ApiError::Unknown(e.to_string())))?.to_vec(),
        Err(e) => { bot.edit_message_text(msg.chat.id, sent.id, format!("❌ Failed to download audio: {}", e)).await?; return Ok(()); }
    };

    match voice::recognize(&state.http, audio_bytes, &state.config.audd_api_key).await {
        Ok(rec) => {
            let query = format!("{} {}", rec.title, rec.artist).replace('&', " ").split_whitespace().collect::<Vec<_>>().join(" ");
            bot.edit_message_text(msg.chat.id, sent.id, format!("🎵 Found: {} — {}\nQueuing...", rec.title, rec.artist)).await?;
            // Use Deezer URL directly from AudD if available (avoids transliteration issues)
            if let Some(ref deezer_url) = rec.deezer_url {
                log::info!("[recognize] Step 1: using AudD Deezer URL: {}", deezer_url);
                match deemix::add_to_queue(&state, deezer_url).await {
                    Ok(_) => { bot.edit_message_text(msg.chat.id, sent.id, format!("✅ {} — {} added to queue!", rec.title, rec.artist)).await?; }
                    Err(e) => {
                        log::info!("[recognize] Step 1 FAILED: add_to_queue error: {}", e);
                        bot.edit_message_text(msg.chat.id, sent.id, format!("❌ Failed to queue: {}", e)).await?;
                    }
                }
            } else {
                log::info!("[recognize] Step 1: no AudD Deezer URL, trying Odesli");
                // Try Odesli (song.link) for a direct Deezer URL
                let odesli_deezer = if let Some(ref sl) = rec.song_link {
                    log::info!("[recognize] Step 2: song_link present: {}", sl);
                    voice::lookup_deezer_via_odesli(&state.http, sl).await
                } else {
                    log::info!("[recognize] Step 2: no song_link, skipping Odesli");
                    None
                };

                if let Some(ref deezer_url) = odesli_deezer {
                    log::info!("[recognize] Step 2: using Odesli Deezer URL: {}", deezer_url);
                    match deemix::add_to_queue(&state, deezer_url).await {
                        Ok(_) => { bot.edit_message_text(msg.chat.id, sent.id, format!("✅ {} — {} added to queue!", rec.title, rec.artist)).await?; }
                        Err(e) => {
                            log::info!("[recognize] Step 2 FAILED: add_to_queue error: {}", e);
                            bot.edit_message_text(msg.chat.id, sent.id, format!("❌ Failed to queue: {}", e)).await?;
                        }
                    }
                } else {
                    log::info!("[recognize] Step 2: song.link gave no Deezer URL, trying iTunes→Odesli");
                    let itunes_deezer = voice::lookup_deezer_via_itunes(&state.http, &rec.title, &rec.artist).await;
                    if let Some(ref deezer_url) = itunes_deezer {
                        log::info!("[recognize] Step 2b: using iTunes→Odesli Deezer URL: {}", deezer_url);
                        match deemix::add_to_queue(&state, deezer_url).await {
                            Ok(_) => { bot.edit_message_text(msg.chat.id, sent.id, format!("✅ {} — {} added to queue!", rec.title, rec.artist)).await?; }
                            Err(e) => { log::info!("[recognize] Step 2b FAILED: {}", e); bot.edit_message_text(msg.chat.id, sent.id, format!("❌ Failed to queue: {}", e)).await?; }
                        }
                    } else {
                    // Step 2c: try Odesli API directly with Spotify URL
                    let spotify_deezer = if let Some(ref sp_url) = rec.spotify_url {
                        log::info!("[recognize] Step 2c: trying Odesli via Spotify URL: {}", sp_url);
                        voice::lookup_deezer_via_spotify(&state.http, sp_url).await
                    } else {
                        log::info!("[recognize] Step 2c: no Spotify URL, skipping");
                        None
                    };
                    if let Some(ref deezer_url) = spotify_deezer {
                        log::info!("[recognize] Step 2c: using Spotify→Odesli Deezer URL: {}", deezer_url);
                        match deemix::add_to_queue(&state, deezer_url).await {
                            Ok(_) => { bot.edit_message_text(msg.chat.id, sent.id, format!("✅ {} — {} added to queue!", rec.title, rec.artist)).await?; }
                            Err(e) => { log::info!("[recognize] Step 2c FAILED: {}", e); bot.edit_message_text(msg.chat.id, sent.id, format!("❌ Failed to queue: {}", e)).await?; }
                        }
                    } else {
                    // Try Spotify metadata for proper Unicode title; fall back to arabizi
                    let search_query = if let Some(ref sp_url) = rec.spotify_url {
                        log::info!("[recognize] Step 3: resolving Spotify URL: {}", sp_url);
                        match spotify::resolve(sp_url).await {
                            Some(meta) => { log::info!("[recognize] Step 3: Spotify query: {:?}", meta.query); meta.query }
                            None => { log::info!("[recognize] Step 3: Spotify resolve failed, falling back to AudD text: {:?}", query); query.clone() }
                        }
                    } else {
                        log::info!("[recognize] Step 3: no Spotify URL, using AudD text: {:?}", query);
                        query.clone()
                    };
                    log::info!("[recognize] Step 3: Deezer text search query: {:?}", search_query);
                    let deezer_search_url: String = {
                        let encoded: String = search_query.bytes().map(|b| match b {
                            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => char::from(b).to_string(),
                            b' ' => "%20".to_string(),
                            _ => format!("%{:02X}", b),
                        }).collect();
                        format!("https://www.deezer.com/search/{}", encoded)
                    };
                    let sent2 = bot.send_message(msg.chat.id, "Searching on Deezer...").await?;
                    let results = match deemix::search(&state, &search_query, "track").await {
                        Err(e) => { bot.edit_message_text(msg.chat.id, sent2.id, format!("❌ Search failed: {}", e)).await?; return Ok(()); }
                        Ok(r) if r.is_empty() => {
                            log::info!("[recognize] Step 3: full query 0 results, retrying title-only: {:?}", rec.title);
                            deemix::search(&state, &rec.title, "track").await.unwrap_or_default()
                        }
                        Ok(r) => r,
                    };
                    if results.is_empty() {
                        log::info!("[recognize] Step 3: title-only search also 0 results");
                        if let Ok(url) = reqwest::Url::parse(&deezer_search_url) {
                            bot.edit_message_text(msg.chat.id, sent2.id, format!("😕 No results for: {} — {}\n\nSearch on Deezer and paste the link back here.", rec.title, rec.artist))
                                .reply_markup(InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::url("🔍 Search on Deezer", url)]]))
                                .await?;
                        } else {
                            bot.edit_message_text(msg.chat.id, sent2.id, format!("😕 No results for: {} — {}", rec.title, rec.artist)).await?;
                        }
                    } else {
                        log::info!("[recognize] Step 3: got {} Deezer results", results.len());
                        let (listing, mut buttons) = build_search_results(&results, "🎵");
                        if let Ok(url) = reqwest::Url::parse(&deezer_search_url) {
                            buttons.push(vec![InlineKeyboardButton::url("🔍 None of these — search on Deezer", url)]);
                        }
                        buttons.push(vec![InlineKeyboardButton::callback("❌ Cancel", "cancel")]);
                        bot.edit_message_text(msg.chat.id, sent2.id, format!("Results for {} — {}:\n\n{}\nIf none match, search on Deezer and paste the link back here.", rec.title, rec.artist, listing))
                            .reply_markup(InlineKeyboardMarkup::new(buttons)).await?;
                    }
                    } // closes 2c else
                    } // closes iTunes else
                }
            }
        }
        Err(e) => { bot.edit_message_text(msg.chat.id, sent.id, format!("❌ Recognition failed: {}", e)).await?; }
    }
    Ok(())
}

async fn handle_message(bot: Bot, msg: Message, state: Arc<BotState>, dialogue: MyDialogue) -> ResponseResult<()> {
    // Auto-create user
    let user_settings = users::get_or_create(&state.users, user_id_from_msg(&msg));
    users::save(&state.users, &state.config.users_file);

    // ── Voice note handling ──
    if let Some(voice) = msg.voice() {
        dialogue.exit().await.ok(); // exit any active dialogue state
        let user_settings = users::get_or_create(&state.users, user_id_from_msg(&msg));
        let audd_on = state.config.audd_enabled() && user_settings.song_recognition;
        let whisper_on = state.config.whisper_enabled() && user_settings.voice_search;

        if !audd_on && !whisper_on {
            bot.send_message(msg.chat.id, "⚠️ Voice features are not configured or disabled. Use /settings to manage them.").await?;
            return Ok(());
        }

        // Store file_id with a short key to stay under Telegram's 64 byte callback limit
        let short_id = format!("{}", msg.id.0);
        {
            let mut map = state.pending_voices.lock().await;
            map.insert(short_id.clone(), voice.file.id.to_string());
        }

        // Build choice buttons based on what's enabled
        let mut buttons: Vec<Vec<teloxide::types::InlineKeyboardButton>> = vec![];
        if whisper_on {
            buttons.push(vec![teloxide::types::InlineKeyboardButton::callback(
                "🎤 Transcribe what I said",
                format!("vt:{}", short_id),
            )]);
        }
        if audd_on {
            buttons.push(vec![teloxide::types::InlineKeyboardButton::callback(
                "🎵 Recognize the song",
                format!("vr:{}", short_id),
            )]);
        }
        buttons.push(vec![teloxide::types::InlineKeyboardButton::callback("❌ Cancel", "cancel")]);

        bot.send_message(msg.chat.id, "🎙️ What should I do with this voice note?")
            .reply_markup(teloxide::types::InlineKeyboardMarkup::new(buttons))
            .await?;
        return Ok(());
    }

    let text = match msg.text() {
        Some(t) => t.trim().to_string(),
        None => return Ok(()),
    };

    // ── Keyboard button presses ──
    match text.as_str() {
        "🔍 Search a track" => {
            dialogue.update(State::AwaitingSearch).await.ok();
            bot.send_message(msg.chat.id, "🔍 What song or artist are you looking for?").await?;
            return Ok(());
        }
        "💿 Search an album" => {
            dialogue.update(State::AwaitingAlbum).await.ok();
            bot.send_message(msg.chat.id, "💿 What album are you looking for?").await?;
            return Ok(());
        }
        "🔗 From streaming link" => {
            dialogue.update(State::AwaitingSpotify).await.ok();
            bot.send_message(msg.chat.id, "🔗 Send me a link from Spotify, YouTube, YouTube Music, or Apple Music:").await?;
            return Ok(());
        }
        "🎵 From Deezer URL" => {
            dialogue.update(State::AwaitingDl).await.ok();
            bot.send_message(msg.chat.id, "🎵 Send me a Deezer URL:").await?;
            return Ok(());
        }
        "📥 Import playlist" => {
            dialogue.update(State::AwaitingPlaylistImport).await.ok();
            bot.send_message(msg.chat.id, "📥 Send me a Spotify or YouTube playlist link and I'll download every song:").await?;
            return Ok(());
        }
        "🎧 Clone playlist" => {
            match &state.media {
                Some(server) => {
                    dialogue.update(State::AwaitingPlaylistClone).await.ok();
                    bot.send_message(msg.chat.id, format!(
                        "🎧 Send me a Spotify or YouTube playlist link — I'll download every song and rebuild the playlist in {}:",
                        server.label())).await?;
                }
                None => {
                    bot.send_message(msg.chat.id, "⚠️ No media server is configured. Set MEDIA_SERVER in your .env to enable playlist cloning.").await?;
                }
            }
            return Ok(());
        }
        "🎤 Voice search" => {
            if !state.config.whisper_enabled() || !user_settings.voice_search {
                bot.send_message(msg.chat.id, "⚠️ Voice search is not configured. Add OPENAI_API_KEY or WHISPER_URL to your .env, or enable it in /settings.").await?;
            } else {
                dialogue.update(State::AwaitingVoiceTranscribe).await.ok();
                bot.send_message(msg.chat.id, "🎤 Send me a voice note and I'll transcribe what you said and search for it.

⏱ You have 60 seconds.").await?;
                // Spawn timeout to reset dialogue after 60s
                let dialogue_clone = dialogue.clone();
                let chat_id = msg.chat.id;
                let bot_clone = bot.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                    if let Ok(Some(State::AwaitingVoiceTranscribe)) = dialogue_clone.get().await {
                        dialogue_clone.exit().await.ok();
                        let _ = bot_clone.send_message(chat_id, "⏱ Voice search timed out. Send a voice note or use /menu to start again.").await;
                    }
                });
            }
            return Ok(());
        }
        "🎵 Recognize song" => {
            if !state.config.audd_enabled() || !user_settings.song_recognition {
                bot.send_message(msg.chat.id, "⚠️ Song recognition is not configured. Add AUDD_API_KEY to your .env, or enable it in /settings.").await?;
            } else {
                dialogue.update(State::AwaitingVoiceRecognize).await.ok();
                bot.send_message(msg.chat.id, "🎵 Send me a voice recording of a song and I'll identify it.

⏱ You have 60 seconds.").await?;
                // Spawn timeout to reset dialogue after 60s
                let dialogue_clone = dialogue.clone();
                let chat_id = msg.chat.id;
                let bot_clone = bot.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                    if let Ok(Some(State::AwaitingVoiceRecognize)) = dialogue_clone.get().await {
                        dialogue_clone.exit().await.ok();
                        let _ = bot_clone.send_message(chat_id, "⏱ Song recognition timed out. Send a voice note or use /menu to start again.").await;
                    }
                });
            }
            return Ok(());
        }
        "📊 Check status" => {
            let mut t = String::new();
            match deemix::get_queue(&state).await {
                Ok(q) => {
                    t.push_str("✅ Deemix is reachable\n");
                    if q.downloading > 0 { t.push_str(&format!("⬇️ Downloading: {}\n", q.downloading)); }
                    if q.pending > 0 { t.push_str(&format!("⏳ Pending: {}\n", q.pending)); }
                    if q.failed > 0 { t.push_str(&format!("❌ Failed: {}\n", q.failed)); }
                    if q.done > 0 { t.push_str(&format!("✅ Completed: {}\n", q.done)); }
                    if q.downloading == 0 && q.pending == 0 && q.done == 0 && q.failed == 0 { t.push_str("📭 Queue is empty\n"); }
                }
                Err(e) => { t.push_str(&format!("❌ Can't reach deemix: {}\n", e)); }
            }
            if let Some(server) = &state.media {
                t.push('\n');
                match server.connect(&state.http).await {
                    Ok(_) => t.push_str(&format!("✅ {} is reachable", server.label())),
                    Err(e) => t.push_str(&format!("❌ Can't reach {}: {}", server.label(), e)),
                }
            }
            bot.send_message(msg.chat.id, t.trim_end().to_string()).await?;
            return Ok(());
        }
        "🧹 Clear queue" => {
            match deemix::clear_completed(&state).await {
                Ok(0) => { bot.send_message(msg.chat.id, "📭 No completed downloads to clear.").await?; }
                Ok(n) => { bot.send_message(msg.chat.id, format!("🧹 Cleared {} completed download(s) from queue.", n)).await?; }
                Err(e) => { bot.send_message(msg.chat.id, format!("❌ Failed to clear queue: {}", e)).await?; }
            }
            return Ok(());
        }
        "⚙️ Settings" => {
            let current_br = *state.current_bitrate.lock().await;
    let kb = settings_keyboard(&user_settings, &state.config, current_br);
            bot.send_message(msg.chat.id, "⚙️ Your settings — tap to toggle:").reply_markup(kb).await?;
            return Ok(());
        }
        "🔙 Back to menu" => {
            let kb = main_keyboard(&user_settings, &state);
            bot.send_message(msg.chat.id, "Choose an action:").reply_markup(kb).await?;
            return Ok(());
        }
        "🔑 Update ARL" => {
            dialogue.update(State::AwaitingArl).await.ok();
            bot.send_message(msg.chat.id, "Please send your new Deezer ARL:")
                .reply_markup(arl_cancel_keyboard())
                .await?;
            return Ok(());
        }
        "ℹ️ Help" => {
            bot.send_message(msg.chat.id,
                "ℹ️ Teleemix — Full Guide\n\n\
🎵 What I do:\n\
I connect to your personal deemix server and queue music downloads. Just tell me what you want!\n\n\
📥 Ways to request music:\n\
• Type any song or artist name → search and pick\n\
• Send a Deezer link → queued instantly\n\
• Send a Spotify, YouTube, YouTube Music, or Apple Music link → found on Deezer and queued\n\
• Send a Spotify or YouTube playlist link → every track is scanned and queued individually\n\
• Send a voice note → transcribe or recognize\n\n\
📜 Playlists (via /menu):\n\
• 📥 Import playlist — download every song from a Spotify/YouTube playlist\n\
• 🎧 Clone playlist — import + rebuild the playlist in your media server\n\n\
🔧 Commands:\n\
/menu — quick action buttons\n\
/search — search for a track\n\
/album — search for an album\n\
/dl — queue from a Deezer URL\n\
/sp — queue from a streaming link (Spotify, YouTube, Apple Music)\n\
/status — check deemix\n\
/clearqueue — clear completed downloads from queue\n\
/settings — your personal settings\n\
/updatearl — update your Deezer ARL\n\n\
💡 Tip: Just send a song name or link — no commands needed!"
            ).await?;
            return Ok(());
        }
        // Settings toggles
        t if t.starts_with("🔔 Restart notifications:") || t.starts_with("🔕 Restart notifications:") => {
            users::update(&state.users, &state.config.users_file, user_id_from_msg(&msg), |s| {
                s.restart_notifications = !s.restart_notifications;
            });
            let updated = users::get_or_create(&state.users, user_id_from_msg(&msg));
            let current_br = *state.current_bitrate.lock().await;
    let kb = settings_keyboard(&updated, &state.config, current_br);
            let status = if updated.restart_notifications { "ON" } else { "OFF" };
            bot.send_message(msg.chat.id, format!("🔔 Restart notifications: {}", status)).reply_markup(kb).await?;
            return Ok(());
        }
        t if t.starts_with("🎤 Voice search:") => {
            if !state.config.whisper_enabled() {
                bot.send_message(msg.chat.id, "⚠️ Voice search is not configured. Add OPENAI_API_KEY to your .env to enable it.").await?;
                return Ok(());
            }
            users::update(&state.users, &state.config.users_file, user_id_from_msg(&msg), |s| {
                s.voice_search = !s.voice_search;
            });
            let updated = users::get_or_create(&state.users, user_id_from_msg(&msg));
            let current_br = *state.current_bitrate.lock().await;
    let kb = settings_keyboard(&updated, &state.config, current_br);
            let status = if updated.voice_search { "ON" } else { "OFF" };
            bot.send_message(msg.chat.id, format!("🎤 Voice search: {}", status)).reply_markup(kb).await?;
            return Ok(());
        }
        t if t.starts_with("🎚️ Quality:") => {
            if state.config.deemix_bitrate_lock {
                bot.send_message(msg.chat.id, "🔒 Download quality is locked by the administrator.").await?;
                return Ok(());
            }
            let new_bitrate = {
                let mut br = state.current_bitrate.lock().await;
                *br = next_bitrate(*br);
                *br
            };
            let updated = users::get_or_create(&state.users, user_id_from_msg(&msg));
            let current_br = new_bitrate;
            let kb = settings_keyboard(&updated, &state.config, current_br);
            bot.send_message(
                msg.chat.id,
                format!(
                    "🎚️ Download quality changed to: {}

⚠️ This affects ALL users on this server.",
                    bitrate_label(new_bitrate)
                ),
            )
            .reply_markup(kb)
            .await?;
            return Ok(());
        }
        t if t.starts_with("🔒 Quality:") => {
            bot.send_message(msg.chat.id, "🔒 Download quality is locked by the administrator.").await?;
            return Ok(());
        }
        t if t.starts_with("🎵 Song recognition:") => {
            if !state.config.audd_enabled() {
                bot.send_message(msg.chat.id, "⚠️ Song recognition is not configured. Add AUDD_API_KEY to your .env to enable it.").await?;
                return Ok(());
            }
            users::update(&state.users, &state.config.users_file, user_id_from_msg(&msg), |s| {
                s.song_recognition = !s.song_recognition;
            });
            let updated = users::get_or_create(&state.users, user_id_from_msg(&msg));
            let current_br = *state.current_bitrate.lock().await;
    let kb = settings_keyboard(&updated, &state.config, current_br);
            let status = if updated.song_recognition { "ON" } else { "OFF" };
            bot.send_message(msg.chat.id, format!("🎵 Song recognition: {}", status)).reply_markup(kb).await?;
            return Ok(());
        }
        _ => {}
    }

    // ── Voice notes ──
    if let Some(voice) = msg.voice() {
        handle_voice_note(&bot, &msg, &state, voice.file.id.clone()).await?;
        return Ok(());
    }

    // ── Streaming service URLs ──
    if SPOTIFY_TRACK_RE.is_match(&text) || SPOTIFY_ALBUM_RE.is_match(&text)
        || SPOTIFY_PLAYLIST_RE.is_match(&text) || YOUTUBE_RE.is_match(&text)
        || APPLE_MUSIC_RE.is_match(&text)
    {
        handle_streaming_link(&bot, &msg, &state, &text).await?;
        return Ok(());
    }

    // ── Deezer short URL ──
    if DEEZER_SHORT_RE.is_match(&text) {
        let resolved = resolve_short_link(&state.http, &text).await;
        if let Some(url) = resolved {
            queue_url(&bot, &msg, &state, &url).await?;
        } else {
            bot.send_message(msg.chat.id, "❌ Could not resolve that link.").await?;
        }
        return Ok(());
    }

    // ── Full Deezer URL ──
    if DEEZER_URL_RE.is_match(&text) {
        queue_url(&bot, &msg, &state, &text).await?;
        return Ok(());
    }

    // ── Plain text search ──
    do_search(&bot, &msg, &state, &text, "track").await?;
    Ok(())
}

// ── Callback Handler ──────────────────────────────────────────────────────────

async fn handle_callback(
    bot: Bot,
    q: CallbackQuery,
    state: Arc<BotState>,
    storage: Arc<InMemStorage<State>>,
) -> ResponseResult<()> {
    bot.answer_callback_query(&q.id).await?;

    let data = match &q.data {
        Some(d) => d.clone(),
        None => return Ok(()),
    };

    if data == "cancel" {
        if let Some(msg) = &q.message {
            bot.edit_message_text(msg.chat.id, msg.id, "Cancelled.").await?;
        }
        return Ok(());
    }

    if data == "cancel_arl" {
        if let Some(msg) = &q.message {
            let dialogue: MyDialogue = Dialogue::new(storage, msg.chat.id);
            dialogue.exit().await.ok();
            bot.edit_message_text(msg.chat.id, msg.id, "❌ ARL update cancelled.").await?;
        }
        return Ok(());
    }

    // ── Playlist import/clone choice ──
    let is_pl_import = data.starts_with("pi:");
    let is_pl_clone = data.starts_with("pc:");
    if is_pl_import || is_pl_clone {
        if let Some(msg) = &q.message {
            let short_id = &data[3..];
            let url = state.pending_playlists.lock().await.remove(short_id);
            match url {
                Some(url) => {
                    start_playlist_flow(&bot, msg.chat.id, &state, msg.id, &url, is_pl_clone).await?;
                }
                None => {
                    bot.edit_message_text(msg.chat.id, msg.id, "❌ That playlist link expired. Please send it again.").await?;
                }
            }
        }
        return Ok(());
    }

    if let Some(url) = data.strip_prefix("dl:") {
        if let Some(msg) = &q.message {
            let kind = DEEZER_URL_RE.captures(url)
                .and_then(|c| c.get(1))
                .map(|m| capitalize(m.as_str()))
                .unwrap_or_else(|| "Item".to_string());
            bot.edit_message_text(msg.chat.id, msg.id, format!("⏳ Queuing {}...", kind.to_lowercase())).await?;
            match deemix::add_to_queue(&state, url).await {
                Ok(_) => { bot.edit_message_text(msg.chat.id, msg.id, format!("✅ {} added to queue!", kind)).await?; }
                Err(e) => { bot.edit_message_text(msg.chat.id, msg.id, format!("❌ Failed: {}", e)).await?; }
            }
        }
    }

    // ── Voice callbacks ──
    let is_transcribe = data.starts_with("vt:");
    let is_recognize = data.starts_with("vr:");
    if is_transcribe || is_recognize {
        if let Some(msg) = &q.message {
            let short_id = &data[3..];
            let action = if is_transcribe { "transcribe" } else { "recognize" };

            // Retrieve file_id from pending_voices map
            let file_id = {
                let map = state.pending_voices.lock().await;
                map.get(short_id).cloned()
            };
            let file_id = match file_id {
                Some(f) => f,
                None => {
                    bot.edit_message_text(msg.chat.id, msg.id, "❌ Voice note expired. Please send it again.").await?;
                    return Ok(());
                }
            };

            bot.edit_message_text(msg.chat.id, msg.id, "⏳ Processing voice note...").await?;

            // Download audio from Telegram
            let file = bot.get_file(&file_id).await
                .map_err(|e| teloxide::RequestError::Api(teloxide::ApiError::Unknown(e.to_string())))?;
            let url = format!("https://api.telegram.org/file/bot{}/{}", bot.token(), file.path);
            let audio_bytes = match state.http.get(&url).send().await {
                Ok(r) => match r.bytes().await {
                    Ok(b) => b.to_vec(),
                    Err(e) => { bot.edit_message_text(msg.chat.id, msg.id, format!("❌ Failed to read audio: {}", e)).await?; return Ok(()); }
                },
                Err(e) => { bot.edit_message_text(msg.chat.id, msg.id, format!("❌ Failed to download audio: {}", e)).await?; return Ok(()); }
            };

            match action {
                "transcribe" => {
                    bot.edit_message_text(msg.chat.id, msg.id, "🎤 Transcribing...").await?;
                    match voice::transcribe(&state.http, audio_bytes, &state.config.openai_api_key, &state.config.whisper_url).await {
                        Ok(text) if text.is_empty() => {
                            bot.edit_message_text(msg.chat.id, msg.id, "😕 Could not transcribe anything. Try speaking more clearly.").await?;
                        }
                        Ok(text) => {
                            bot.edit_message_text(msg.chat.id, msg.id, format!("🔍 I heard: {}\nSearching...", text)).await?;
                            let sent = bot.send_message(msg.chat.id, format!("Results for: {}", text)).await?;
                            match deemix::search(&state, &text, "track").await {
                                Ok(results) if results.is_empty() => {
                                    bot.edit_message_text(msg.chat.id, sent.id, format!("😕 No results for: {}", text)).await?;
                                }
                                Ok(results) => {
                                    let (listing, mut buttons) = build_search_results(&results, "🎵");
                                    buttons.push(vec![InlineKeyboardButton::callback("❌ Cancel", "cancel")]);
                                    bot.edit_message_text(msg.chat.id, sent.id, format!("Results for '{}':\n\n{}\nTap a button to download.", text, listing))
                                        .reply_markup(InlineKeyboardMarkup::new(buttons)).await?;
                                }
                                Err(e) => { bot.edit_message_text(msg.chat.id, sent.id, format!("❌ Search failed: {}", e)).await?; }
                            }
                        }
                        Err(e) => { bot.edit_message_text(msg.chat.id, msg.id, format!("❌ Transcription failed: {}", e)).await?; }
                    }
                }
                "recognize" => {
                    bot.edit_message_text(msg.chat.id, msg.id, "🎵 Recognizing song...").await?;
                    match voice::recognize(&state.http, audio_bytes, &state.config.audd_api_key).await {
                        Ok(rec) => {
                            let query = format!("{} {}", rec.title, rec.artist).replace('&', " ").split_whitespace().collect::<Vec<_>>().join(" ");
                            bot.edit_message_text(msg.chat.id, msg.id, format!("🎵 Found: {} — {}\nQueuing...", rec.title, rec.artist)).await?;
                            // Use Deezer URL directly from AudD if available (avoids transliteration issues)
                            if let Some(ref deezer_url) = rec.deezer_url {
                                log::info!("[recognize/cb] Step 1: using AudD Deezer URL: {}", deezer_url);
                                match deemix::add_to_queue(&state, deezer_url).await {
                                    Ok(_) => { bot.edit_message_text(msg.chat.id, msg.id, format!("✅ {} — {} added to queue!", rec.title, rec.artist)).await?; }
                                    Err(e) => {
                                        log::info!("[recognize/cb] Step 1 FAILED: add_to_queue error: {}", e);
                                        bot.edit_message_text(msg.chat.id, msg.id, format!("❌ Failed to queue: {}", e)).await?;
                                    }
                                }
                            } else {
                                log::info!("[recognize/cb] Step 1: no AudD Deezer URL, trying Odesli");
                                // Try Odesli (song.link) for a direct Deezer URL
                                let odesli_deezer = if let Some(ref sl) = rec.song_link {
                                    log::info!("[recognize/cb] Step 2: song_link present: {}", sl);
                                    voice::lookup_deezer_via_odesli(&state.http, sl).await
                                } else {
                                    log::info!("[recognize/cb] Step 2: no song_link, skipping Odesli");
                                    None
                                };

                                if let Some(ref deezer_url) = odesli_deezer {
                                    log::info!("[recognize/cb] Step 2: using Odesli Deezer URL: {}", deezer_url);
                                    match deemix::add_to_queue(&state, deezer_url).await {
                                        Ok(_) => { bot.edit_message_text(msg.chat.id, msg.id, format!("✅ {} — {} added to queue!", rec.title, rec.artist)).await?; }
                                        Err(e) => {
                                            log::info!("[recognize/cb] Step 2 FAILED: add_to_queue error: {}", e);
                                            bot.edit_message_text(msg.chat.id, msg.id, format!("❌ Failed to queue: {}", e)).await?;
                                        }
                                    }
                                } else {
                                    log::info!("[recognize/cb] Step 2: song.link gave no Deezer URL, trying iTunes→Odesli");
                                    let itunes_deezer = voice::lookup_deezer_via_itunes(&state.http, &rec.title, &rec.artist).await;
                                    if let Some(ref deezer_url) = itunes_deezer {
                                        log::info!("[recognize/cb] Step 2b: using iTunes→Odesli Deezer URL: {}", deezer_url);
                                        match deemix::add_to_queue(&state, deezer_url).await {
                                            Ok(_) => { bot.edit_message_text(msg.chat.id, msg.id, format!("✅ {} — {} added to queue!", rec.title, rec.artist)).await?; }
                                            Err(e) => { log::info!("[recognize/cb] Step 2b FAILED: {}", e); bot.edit_message_text(msg.chat.id, msg.id, format!("❌ Failed to queue: {}", e)).await?; }
                                        }
                                    } else {
                                    // Step 2c: try Odesli API directly with Spotify URL
                                    let spotify_deezer = if let Some(ref sp_url) = rec.spotify_url {
                                        log::info!("[recognize/cb] Step 2c: trying Odesli via Spotify URL: {}", sp_url);
                                        voice::lookup_deezer_via_spotify(&state.http, sp_url).await
                                    } else {
                                        log::info!("[recognize/cb] Step 2c: no Spotify URL, skipping");
                                        None
                                    };
                                    if let Some(ref deezer_url) = spotify_deezer {
                                        log::info!("[recognize/cb] Step 2c: using Spotify→Odesli Deezer URL: {}", deezer_url);
                                        match deemix::add_to_queue(&state, deezer_url).await {
                                            Ok(_) => { bot.edit_message_text(msg.chat.id, msg.id, format!("✅ {} — {} added to queue!", rec.title, rec.artist)).await?; }
                                            Err(e) => { log::info!("[recognize/cb] Step 2c FAILED: {}", e); bot.edit_message_text(msg.chat.id, msg.id, format!("❌ Failed to queue: {}", e)).await?; }
                                        }
                                    } else {
                                    // Try Spotify metadata for proper Unicode title; fall back to arabizi
                                    let search_query = if let Some(ref sp_url) = rec.spotify_url {
                                        log::info!("[recognize/cb] Step 3: resolving Spotify URL: {}", sp_url);
                                        match spotify::resolve(sp_url).await {
                                            Some(meta) => { log::info!("[recognize/cb] Step 3: Spotify query: {:?}", meta.query); meta.query }
                                            None => { log::info!("[recognize/cb] Step 3: Spotify resolve failed, falling back to AudD text: {:?}", query); query.clone() }
                                        }
                                    } else {
                                        log::info!("[recognize/cb] Step 3: no Spotify URL, using AudD text: {:?}", query);
                                        query.clone()
                                    };
                                    log::info!("[recognize/cb] Step 3: Deezer text search query: {:?}", search_query);
                                    let deezer_search_url: String = {
                                        let encoded: String = search_query.bytes().map(|b| match b {
                                            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => char::from(b).to_string(),
                                            b' ' => "%20".to_string(),
                                            _ => format!("%{:02X}", b),
                                        }).collect();
                                        format!("https://www.deezer.com/search/{}", encoded)
                                    };
                                    let sent = bot.send_message(msg.chat.id, "Searching on Deezer...").await?;
                                    let results = match deemix::search(&state, &search_query, "track").await {
                                        Err(e) => { log::info!("[recognize/cb] Step 3: Deezer search error: {}", e); bot.edit_message_text(msg.chat.id, sent.id, format!("❌ Search failed: {}", e)).await?; return Ok(()); }
                                        Ok(r) if r.is_empty() => {
                                            log::info!("[recognize/cb] Step 3: full query 0 results, retrying title-only: {:?}", rec.title);
                                            deemix::search(&state, &rec.title, "track").await.unwrap_or_default()
                                        }
                                        Ok(r) => r,
                                    };
                                    if results.is_empty() {
                                        log::info!("[recognize/cb] Step 3: title-only search also 0 results");
                                        if let Ok(url) = reqwest::Url::parse(&deezer_search_url) {
                                            bot.edit_message_text(msg.chat.id, sent.id, format!("😕 No results for: {} — {}\n\nSearch on Deezer and paste the link back here.", rec.title, rec.artist))
                                                .reply_markup(InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::url("🔍 Search on Deezer", url)]]))
                                                .await?;
                                        } else {
                                            bot.edit_message_text(msg.chat.id, sent.id, format!("😕 No results for: {} — {}", rec.title, rec.artist)).await?;
                                        }
                                    } else {
                                        log::info!("[recognize/cb] Step 3: got {} Deezer results", results.len());
                                        let (listing, mut buttons) = build_search_results(&results, "🎵");
                                        if let Ok(url) = reqwest::Url::parse(&deezer_search_url) {
                                            buttons.push(vec![InlineKeyboardButton::url("🔍 None of these — search on Deezer", url)]);
                                        }
                                        buttons.push(vec![InlineKeyboardButton::callback("❌ Cancel", "cancel")]);
                                        bot.edit_message_text(msg.chat.id, sent.id, format!("Results for {} — {}:\n\n{}\nIf none match, search on Deezer and paste the link back here.", rec.title, rec.artist, listing))
                                            .reply_markup(InlineKeyboardMarkup::new(buttons)).await?;
                                    }
                                    } // closes 2c else
                                    } // closes iTunes else
                                }
                            }
                        }
                        Err(e) => { bot.edit_message_text(msg.chat.id, msg.id, format!("❌ Recognition failed: {}", e)).await?; }
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}

// ── Core Helpers ──────────────────────────────────────────────────────────────

async fn resolve_short_link(http: &Client, url: &str) -> Option<String> {
    let resp = http.head(url).send().await.ok()?;
    Some(resp.url().to_string())
}

async fn queue_url(bot: &Bot, msg: &Message, state: &Arc<BotState>, url: &str) -> ResponseResult<()> {
    let cap = match DEEZER_URL_RE.captures(url) {
        Some(c) => c,
        None => {
            bot.send_message(msg.chat.id, "❌ That doesn't look like a valid Deezer URL.").await?;
            return Ok(());
        }
    };
    let kind = cap.get(1).map(|m| m.as_str()).unwrap_or("item");
    let sent = bot.send_message(msg.chat.id, format!("⏳ Queuing {}...", kind)).await?;

    match deemix::add_to_queue(state, url).await {
        Ok(_) => { bot.edit_message_text(msg.chat.id, sent.id, format!("✅ {} added to queue!", capitalize(kind))).await?; }
        Err(e) => { bot.edit_message_text(msg.chat.id, sent.id, format!("❌ Failed to queue: {}", e)).await?; }
    }
    Ok(())
}

/// Telegram truncates long button labels, so the message text carries the full
/// numbered track list and the buttons reference the numbers.
fn build_search_results(results: &[serde_json::Value], icon: &str) -> (String, Vec<Vec<InlineKeyboardButton>>) {
    let mut listing = String::new();
    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for (i, item) in results.iter().enumerate() {
        let title = item["title"].as_str().unwrap_or("?");
        let artist = item["artist"]["name"].as_str().unwrap_or("?");
        listing.push_str(&format!("{}. {} — {}\n", i + 1, title, artist));
        let label = format!("{} {}. {} — {}", icon, i + 1, title, artist);
        let label = if label.chars().count() > 60 { format!("{}…", label.chars().take(59).collect::<String>()) } else { label };
        buttons.push(vec![InlineKeyboardButton::callback(label, format!("dl:{}", item["link"].as_str().unwrap_or("")))]);
    }
    (listing, buttons)
}

async fn do_search(bot: &Bot, msg: &Message, state: &Arc<BotState>, query: &str, search_type: &str) -> ResponseResult<()> {
    let sent = bot.send_message(msg.chat.id, format!("🔍 Searching for {}...", query)).await?;

    match deemix::search(state, query, search_type).await {
        Ok(results) if results.is_empty() => { bot.edit_message_text(msg.chat.id, sent.id, "😕 No results found.").await?; }
        Ok(results) => {
            let icon = if search_type == "track" { "🎵" } else { "💿" };
            let (listing, mut buttons) = build_search_results(&results, icon);
            buttons.push(vec![InlineKeyboardButton::callback("❌ Cancel", "cancel")]);
            bot.edit_message_text(msg.chat.id, sent.id, format!("Results for {}:\n\n{}\nTap a button to download.", query, listing))
                .reply_markup(InlineKeyboardMarkup::new(buttons))
                .await?;
        }
        Err(e) => { bot.edit_message_text(msg.chat.id, sent.id, format!("❌ Search failed: {}", e)).await?; }
    }
    Ok(())
}

async fn handle_streaming_link(bot: &Bot, msg: &Message, state: &Arc<BotState>, url: &str) -> ResponseResult<()> {
    let sent = bot.send_message(msg.chat.id, "🎵 Looking up link...").await?;

    // Spotify track or album: resolve metadata then show Deezer search results
    if SPOTIFY_TRACK_RE.is_match(url) || SPOTIFY_ALBUM_RE.is_match(url) {
        let search_type = if SPOTIFY_TRACK_RE.is_match(url) { "track" } else { "album" };
        match spotify::resolve(url).await {
            Some(meta) => {
                bot.edit_message_text(msg.chat.id, sent.id, format!("🔍 Found: {}\nSearching on Deezer...", meta.label)).await?;
                match deemix::search(state, &meta.query, search_type).await {
                    Ok(results) if results.is_empty() => {
                        // Fallback: try Odesli directly before giving up
                        match voice::lookup_deezer_via_spotify(&state.http, url).await {
                            Some(deezer_url) => {
                                match deemix::add_to_queue(state, &deezer_url).await {
                                    Ok(_) => { bot.edit_message_text(msg.chat.id, sent.id, format!("✅ {} added to queue!", meta.label)).await?; }
                                    Err(e) => { bot.edit_message_text(msg.chat.id, sent.id, format!("❌ Failed to queue: {}", e)).await?; }
                                }
                            }
                            None => { bot.edit_message_text(msg.chat.id, sent.id, format!("😕 No results found on Deezer for: {}", meta.query)).await?; }
                        }
                    }
                    Ok(results) => {
                        let icon = if search_type == "track" { "🎵" } else { "💿" };
                        let (listing, mut buttons) = build_search_results(&results, icon);
                        buttons.push(vec![InlineKeyboardButton::callback("❌ Cancel", "cancel")]);
                        bot.edit_message_text(msg.chat.id, sent.id, format!("Results for {}:\n\n{}\nTap a button to download.", meta.query, listing))
                            .reply_markup(InlineKeyboardMarkup::new(buttons))
                            .await?;
                    }
                    Err(e) => { bot.edit_message_text(msg.chat.id, sent.id, format!("❌ Search failed: {}", e)).await?; }
                }
            }
            None => { bot.edit_message_text(msg.chat.id, sent.id, "❌ Could not resolve Spotify link. Try /search instead.").await?; }
        }
        return Ok(());
    }

    // Playlist link: with a media server configured, ask whether to import
    // (download only) or clone (download + rebuild the playlist there)
    if is_playlist_link(url) {
        match &state.media {
            Some(server) => {
                let short_id = format!("{}", msg.id.0);
                state.pending_playlists.lock().await.insert(short_id.clone(), url.to_string());
                let buttons = vec![
                    vec![InlineKeyboardButton::callback("📥 Import — download the songs", format!("pi:{}", short_id))],
                    vec![InlineKeyboardButton::callback(
                        format!("🎧 Clone — also rebuild in {}", server.label()),
                        format!("pc:{}", short_id))],
                    vec![InlineKeyboardButton::callback("❌ Cancel", "cancel")],
                ];
                bot.edit_message_text(msg.chat.id, sent.id, "📜 That's a playlist! What should I do?")
                    .reply_markup(InlineKeyboardMarkup::new(buttons))
                    .await?;
            }
            None => {
                start_playlist_flow(bot, msg.chat.id, state, sent.id, url, false).await?;
            }
        }
        return Ok(());
    }

    queue_single_via_odesli(bot, msg.chat.id, state, sent.id, url).await
}

fn is_playlist_link(url: &str) -> bool {
    SPOTIFY_PLAYLIST_RE.is_match(url)
        || (YOUTUBE_RE.is_match(url) && youtube::playlist_id(url).is_some())
}

/// Scan a Spotify/YouTube playlist and queue each track. `rebuild` controls
/// whether a media-server playlist rebuild job is created afterwards.
async fn start_playlist_flow(
    bot: &Bot,
    chat: ChatId,
    state: &Arc<BotState>,
    status_id: teloxide::types::MessageId,
    url: &str,
    rebuild: bool,
) -> ResponseResult<()> {
    // Spotify playlist: scan the track list and queue each song individually,
    // so user-generated playlists work (Deezer rarely has a same-named playlist)
    if SPOTIFY_PLAYLIST_RE.is_match(url) {
        bot.edit_message_text(chat, status_id, "🔍 Reading Spotify playlist...").await?;
        match spotify::resolve_playlist(url).await {
            Some(pl) => { queue_playlist(bot, chat, state, status_id, &pl, rebuild).await?; }
            None => {
                bot.edit_message_text(chat, status_id,
                    "😕 Couldn't read that Spotify playlist. Make sure it's public and try again.").await?;
            }
        }
        return Ok(());
    }

    // YouTube playlist: same per-track treatment
    bot.edit_message_text(chat, status_id, "🔍 Reading YouTube playlist...").await?;
    match youtube::resolve_playlist(&state.http, url).await {
        Some(pl) => {
            queue_playlist(bot, chat, state, status_id, &pl, rebuild).await?;
        }
        None => {
            if url.contains("watch?v=") || url.contains("youtu.be/") {
                // Mixes and private lists can't be scanned — fall back to the single video
                let single = youtube::strip_playlist_params(url);
                queue_single_via_odesli(bot, chat, state, status_id, &single).await?;
            } else {
                bot.edit_message_text(chat, status_id,
                    "😕 Couldn't read that YouTube playlist. Make sure it's public and try again.").await?;
            }
        }
    }
    Ok(())
}

/// Single YouTube video / Apple Music: convert via Odesli to a Deezer URL and queue directly.
async fn queue_single_via_odesli(
    bot: &Bot,
    chat: ChatId,
    state: &Arc<BotState>,
    status_id: teloxide::types::MessageId,
    url: &str,
) -> ResponseResult<()> {
    let service = if YOUTUBE_RE.is_match(url) {
        "YouTube link".to_string()
    } else {
        "Apple Music link".to_string()
    };

    bot.edit_message_text(chat, status_id, format!("🔍 Looking up {} on Deezer...", service)).await?;

    match voice::lookup_deezer_via_spotify(&state.http, url).await {
        Some(deezer_url) => {
            match deemix::add_to_queue(state, &deezer_url).await {
                Ok(_) => { bot.edit_message_text(chat, status_id, format!("✅ {} added to queue!", capitalize(&service))).await?; }
                Err(e) => { bot.edit_message_text(chat, status_id, format!("❌ Failed to queue: {}", e)).await?; }
            }
        }
        None => {
            bot.edit_message_text(chat, status_id,
                format!("😕 Couldn't find this {} on Deezer. Try searching by name with /search.", service)).await?;
        }
    }

    Ok(())
}

/// Queue every track of a scanned playlist by searching it on Deezer and
/// queuing the first match. Edits the status message with progress and a
/// final summary of anything that couldn't be found. With `rebuild` set (and
/// a media server configured), also creates a persistent job that recreates
/// the playlist there once all downloads finish.
async fn queue_playlist(
    bot: &Bot,
    chat: ChatId,
    state: &Arc<BotState>,
    status_id: teloxide::types::MessageId,
    pl: &spotify::Playlist,
    rebuild: bool,
) -> ResponseResult<()> {
    let total = pl.tracks.len();
    let mut queued = 0usize;
    let mut skipped = 0usize;
    let mut not_found: Vec<String> = Vec::new();
    let mut job_tracks: Vec<jobs::JobTrack> = Vec::new();

    // Connect to the media server upfront so we can skip tracks that are
    // already in the library (avoids re-downloading them).
    let server_session = if let Some(server) = &state.media {
        match server.connect(&state.http).await {
            Ok(s) => Some(s),
            Err(e) => {
                log::warn!("[playlist] media server pre-check failed, skipping existence checks: {}", e);
                None
            }
        }
    } else {
        None
    };

    for (i, track) in pl.tracks.iter().enumerate() {
        if i % 5 == 0 {
            let skip_note = if skipped > 0 {
                format!(" ({} already in {}, skipped)",
                    skipped,
                    state.media.as_ref().map(|m| m.label()).unwrap_or("server"))
            } else {
                String::new()
            };
            let _ = bot.edit_message_text(chat, status_id,
                format!("⏳ Queuing \"{}\" — {}/{} tracks...{}", pl.name, i, total, skip_note)).await;
        }

        // Skip tracks already present in the media server.
        if let Some(session) = &server_session {
            if session.find_track(&state.http, &track.title, &track.artist).await.is_some() {
                skipped += 1;
                if rebuild {
                    // Include in the rebuild job so the existing track is
                    // still added to the rebuilt playlist.
                    job_tracks.push(jobs::JobTrack {
                        title: track.title.clone(),
                        artist: track.artist.clone(),
                        uuid: String::new(),
                    });
                }
                continue;
            }
        }

        let full_query = if track.artist.is_empty() {
            track.title.clone()
        } else {
            format!("{} {}", track.title, track.artist)
        };
        let mut found = deemix::search(state, &full_query, "track").await.ok()
            .and_then(|r| r.first().cloned());
        if found.is_none() && !track.artist.is_empty() {
            found = deemix::search(state, &track.title, "track").await.ok()
                .and_then(|r| r.first().cloned());
        }

        let label = if track.artist.is_empty() {
            track.title.clone()
        } else {
            format!("{} — {}", track.title, track.artist)
        };
        match found.as_ref().and_then(|f| f["link"].as_str()) {
            Some(link) => match deemix::add_to_queue(state, link).await {
                Ok(uuids) => {
                    queued += 1;
                    // Track with the Deezer metadata: that's what deemix tags
                    // the files with, so it's what the media server will index
                    let found = found.as_ref().unwrap();
                    job_tracks.push(jobs::JobTrack {
                        title: found["title"].as_str().unwrap_or(&track.title).to_string(),
                        artist: found["artist"]["name"].as_str().unwrap_or(&track.artist).to_string(),
                        uuid: uuids.into_iter().next().unwrap_or_default(),
                    });
                }
                Err(e) => {
                    log::warn!("[playlist] failed to queue {:?}: {}", label, e);
                    not_found.push(label);
                }
            },
            None => not_found.push(label),
        }
    }

    let mut text = format!("✅ Playlist \"{}\": queued {}/{} tracks.", pl.name, queued, total - skipped);
    if skipped > 0 {
        text.push_str(&format!("\n⏭ {} already in {} — skipped.",
            skipped,
            state.media.as_ref().map(|m| m.label()).unwrap_or("media server")));
    }
    if !not_found.is_empty() {
        text.push_str("\n\n😕 Couldn't queue these:\n");
        for t in not_found.iter().take(15) {
            let t = if t.chars().count() > 80 {
                format!("{}…", t.chars().take(79).collect::<String>())
            } else {
                t.clone()
            };
            text.push_str(&format!("• {}\n", t));
        }
        if not_found.len() > 15 {
            text.push_str(&format!("…and {} more", not_found.len() - 15));
        }
    }

    if rebuild {
        if let Some(server) = &state.media {
            if !job_tracks.is_empty() {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let job = jobs::PlaylistJob {
                    id: format!("{}-{}", chat.0, now),
                    chat_id: chat.0,
                    name: pl.name.clone(),
                    tracks: job_tracks,
                };
                jobs::add(state, job.clone());
                jobs::spawn(bot.clone(), Arc::clone(state), job);
                text.push_str(&format!(
                    "\n\n🎧 I'll rebuild this playlist in {} once all downloads finish.",
                    server.label()
                ));
            }
        }
    }

    // Clear the progress spinner and send the result as a new message so it
    // appears at the bottom of the chat even after a long queuing process.
    let _ = bot.delete_message(chat, status_id).await;
    bot.send_message(chat, text).await?;
    Ok(())
}

async fn handle_updatearl(bot: &Bot, msg: &Message, state: &Arc<BotState>, arl: &str) -> ResponseResult<()> {
    let sent = bot.send_message(msg.chat.id, "🔄 Validating new ARL...").await?;

    match deemix::login_arl(state, arl).await {
        Ok(_username) => {
            *state.current_arl.lock().await = arl.to_string();
            bot.edit_message_text(msg.chat.id, sent.id, format!("✅ Logged in!\n🔄 Updating .env file...")).await?;

            match std::fs::read_to_string(&state.config.env_file) {
                Ok(contents) => {
                    let updated = regex::Regex::new(r"DEEMIX_ARL=.*").unwrap()
                        .replace(&contents, format!("DEEMIX_ARL={}", arl))
                        .to_string();
                    if let Err(e) = std::fs::write(&state.config.env_file, &updated) {
                        bot.edit_message_text(msg.chat.id, sent.id, format!("⚠️ Logged in but could not update .env: {}", e)).await?;
                        return Ok(());
                    }
                }
                Err(e) => {
                    bot.edit_message_text(msg.chat.id, sent.id, format!("⚠️ Logged in but could not read .env: {}", e)).await?;
                    return Ok(());
                }
            }

            bot.edit_message_text(msg.chat.id, sent.id, "✅ ARL updated and saved! Downloads will use the new ARL immediately.").await?;
        }
        Err(e) => { bot.edit_message_text(msg.chat.id, sent.id, format!("❌ ARL rejected by deemix: {}", e)).await?; }
    }
    Ok(())
}


async fn handle_voice_note(
    bot: &Bot,
    msg: &Message,
    state: &Arc<BotState>,
    file_id: String,
) -> ResponseResult<()> {
    let user_settings = users::get_or_create(&state.users, msg.chat.id.0);
    let whisper_on = state.config.whisper_enabled() && user_settings.voice_search;
    let audd_on = state.config.audd_enabled() && user_settings.song_recognition;

    if !whisper_on && !audd_on {
        bot.send_message(
            msg.chat.id,
            "⚠️ Voice features are not configured or disabled.
Check /settings or ask your admin to add API keys.",
        )
        .await?;
        return Ok(());
    }

    // Show options based on what is enabled
    let mut buttons = vec![];
    if whisper_on {
        buttons.push(vec![InlineKeyboardButton::callback(
            "🗣️ Search what I said",
            format!("voice_search:{}", file_id),
        )]);
    }
    if audd_on {
        buttons.push(vec![InlineKeyboardButton::callback(
            "🎵 Recognize the song",
            format!("voice_recognize:{}", file_id),
        )]);
    }
    buttons.push(vec![InlineKeyboardButton::callback("❌ Cancel", "cancel")]);

    bot.send_message(msg.chat.id, "🎤 What should I do with this voice note?")
        .reply_markup(InlineKeyboardMarkup::new(buttons))
        .await?;

    Ok(())
}


fn bitrate_label(bitrate: u8) -> &'static str {
    match bitrate {
        9 => "FLAC (lossless)",
        3 => "MP3 320kbps",
        1 => "MP3 128kbps",
        _ => "Unknown",
    }
}

fn next_bitrate(current: u8) -> u8 {
    match current { 9 => 3, 3 => 1, _ => 9 }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}
