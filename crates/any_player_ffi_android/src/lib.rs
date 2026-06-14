use any_player_core::audio_normalization::{
    AdaptiveNormalizationState, AudioNormalizationSettings, AudioNormalizationSource,
    INTERNAL_NORMALIZATION_TARGET, effective_output_volume,
};
use any_player_core::provider_api::ProviderApi;
use any_player_core::provider_api::ProviderConnectionCheck;
use any_player_core::provider_clients::{
    jellyfin::JellyfinApiClient, plex::PlexApiClient, spotify::SpotifyApiClient,
};
use any_player_core::providers::ProviderAuthRequest;
use any_player_spotify_engine::{
    LibrespotPlayer, SpotifyEngineError, SpotifySessionBackend, SpotifySessionConfig,
    SpotifySessionEngine, SpotifyToken,
};
use async_trait::async_trait;
use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::jstring;
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::runtime::{Builder, Runtime};
use tokio::time::{Duration, timeout};
use url::form_urlencoded;

type SharedEngine = Arc<SpotifySessionEngine<AndroidSpotifyBackend>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum RepeatModeState {
    #[default]
    Off,
    One,
    All,
}

impl RepeatModeState {
    fn from_input(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "one" | "track" => Some(Self::One),
            "all" | "context" => Some(Self::All),
            _ => None,
        }
    }

    fn as_snapshot_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::One => "one",
            Self::All => "all",
        }
    }
}

struct BridgeState {
    client_id: Option<String>,
    engine: Option<SharedEngine>,
    // Last access token used for playback — stored so transport commands can
    // transparently reconnect the librespot session after expiry or app restart.
    playback_access_token: Option<String>,
    // Local playback state mirrored for snapshot responses.
    playback_queue: Vec<String>,
    playback_index: usize,
    playback_volume_percent: u8,
    playback_shuffle: bool,
    playback_repeat_mode: RepeatModeState,
    audio_normalization: AudioNormalizationSettings,
    adaptive_normalization: AdaptiveNormalizationState,
}

impl Default for BridgeState {
    fn default() -> Self {
        Self {
            client_id: None,
            engine: None,
            playback_access_token: None,
            playback_queue: Vec::new(),
            playback_index: 0,
            playback_volume_percent: 100,
            playback_shuffle: false,
            playback_repeat_mode: RepeatModeState::Off,
            audio_normalization: AudioNormalizationSettings {
                enabled: false,
                target: INTERNAL_NORMALIZATION_TARGET,
                strict_mode: false,
            },
            adaptive_normalization: AdaptiveNormalizationState::default(),
        }
    }
}

static BRIDGE_STATE: Lazy<Mutex<BridgeState>> = Lazy::new(|| Mutex::new(BridgeState::default()));
/// The librespot player lives in its own static so it can hold async state cleanly.
static PLAYER: Lazy<LibrespotPlayer> = Lazy::new(LibrespotPlayer::new);
static TOKIO_RUNTIME: Lazy<Runtime> = Lazy::new(|| {
    Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to initialize tokio runtime for android ffi bridge")
});

#[derive(Clone, Default)]
struct AndroidSpotifyBackend;

#[derive(Debug, Clone)]
struct AndroidSessionHandle {
    initialized_at_epoch_seconds: u64,
}

#[async_trait]
impl SpotifySessionBackend for AndroidSpotifyBackend {
    type SessionHandle = AndroidSessionHandle;

    async fn connect(
        &self,
        _config: &SpotifySessionConfig,
        access_token: &str,
    ) -> Result<Self::SessionHandle, SpotifyEngineError> {
        if access_token.trim().is_empty() {
            return Err(SpotifyEngineError::new(
                "spotify_access_token_missing",
                "Spotify access_token is required",
            ));
        }

        Ok(AndroidSessionHandle {
            initialized_at_epoch_seconds: current_epoch_seconds(),
        })
    }

    async fn disconnect(&self, session: &Self::SessionHandle) -> Result<(), SpotifyEngineError> {
        let _ = session.initialized_at_epoch_seconds;
        Ok(())
    }
}

#[derive(Deserialize)]
struct InitConfigPayload {
    client_id: String,
}

#[derive(Deserialize)]
struct BeginAuthPayload {
    client_id: String,
    redirect_uri: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    code_challenge: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
}

#[derive(Deserialize)]
struct TokenPayload {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at_epoch_seconds: Option<u64>,
}

#[derive(Deserialize)]
struct StartQueuePayload {
    access_token: String,
    track_ids: Vec<String>,
    #[serde(default)]
    start_index: usize,
}

#[derive(Deserialize)]
struct SeekPayload {
    position_ms: u64,
}

#[derive(Deserialize)]
struct VolumePayload {
    volume_percent: i32,
}

#[derive(Deserialize)]
struct ShufflePayload {
    enabled: bool,
}

#[derive(Deserialize)]
struct RepeatModePayload {
    mode: String,
}

#[derive(Deserialize)]
struct AudioNormalizationSettingsPayload {
    enabled: bool,
    #[serde(default)]
    strict_mode: bool,
}

#[derive(Deserialize)]
struct ApplyNormalizationPayload {
    volume_percent: i32,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProviderApiCallPayload {
    source: String,
    operation: String,
    session: HashMap<String, String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

fn current_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn success_response(data: Value) -> String {
    json!({
        "ok": true,
        "data": data
    })
    .to_string()
}

fn error_response(code: &str, message: impl Into<String>) -> String {
    json!({
        "ok": false,
        "error": {
            "code": code,
            "message": message.into()
        }
    })
    .to_string()
}

fn lock_state() -> Result<std::sync::MutexGuard<'static, BridgeState>, String> {
    BRIDGE_STATE
        .lock()
        .map_err(|_| "Bridge state mutex poisoned".to_string())
}

fn parse_normalization_source(source: Option<&str>) -> AudioNormalizationSource {
    match source
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "other".to_string())
        .as_str()
    {
        "spotify" => AudioNormalizationSource::Spotify,
        _ => AudioNormalizationSource::Other,
    }
}

fn strict_gain_for_source(
    settings: &AudioNormalizationSettings,
    adaptive_state: &AdaptiveNormalizationState,
    source: AudioNormalizationSource,
) -> f32 {
    if !settings.strict_mode {
        return 1.0;
    }

    let source_key = match source {
        AudioNormalizationSource::Spotify => "spotify",
        AudioNormalizationSource::Other => "other",
    };

    adaptive_state.strict_compensation_gain(source_key)
}

fn normalized_volume_for_source(
    base_volume_percent: i32,
    source: AudioNormalizationSource,
    settings: &AudioNormalizationSettings,
    adaptive_state: &AdaptiveNormalizationState,
) -> u8 {
    let clamped_base = base_volume_percent.clamp(0, 100) as u32;
    let strict_gain = strict_gain_for_source(settings, adaptive_state, source);
    effective_output_volume(clamped_base, source, settings, strict_gain) as u8
}

fn parse_required_json<T>(raw_json: &str, field_name: &str) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str::<T>(raw_json)
        .map_err(|error| format!("Invalid {} payload: {}", field_name.trim(), error))
}

fn parse_token(raw_input: &str) -> Result<SpotifyToken, String> {
    let trimmed = raw_input.trim();
    if trimmed.is_empty() {
        return Err("Spotify token payload is required".to_string());
    }

    if trimmed.starts_with('{') {
        let parsed: TokenPayload = serde_json::from_str(trimmed)
            .map_err(|error| format!("Invalid Spotify token JSON payload: {}", error))?;
        let access_token = parsed.access_token.trim();
        if access_token.is_empty() {
            return Err("Spotify access_token is required".to_string());
        }

        return Ok(SpotifyToken {
            access_token: access_token.to_string(),
            refresh_token: parsed
                .refresh_token
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            expires_at_epoch_seconds: parsed.expires_at_epoch_seconds,
        });
    }

    Ok(SpotifyToken::with_access_token(trimmed.to_string()))
}

/// Accepts bare base62 track IDs ("4uLU6hMCjMI75M1A2tKUQC"),
/// full Spotify URIs ("spotify:track:4uLU6hMCjMI75M1A2tKUQC"),
/// and open.spotify.com URLs. Returns None for blank/invalid input.
fn normalize_spotify_track_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let id = if let Some(stripped) = trimmed.strip_prefix("spotify:track:") {
        stripped
    } else if let Some((_, rest)) = trimmed.split_once("/track/") {
        rest.split(['?', '/']).next().unwrap_or(rest)
    } else {
        trimmed
    };
    let id = id.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

fn require_engine() -> Result<SharedEngine, String> {
    let state = lock_state()?;
    state
        .engine
        .clone()
        .ok_or_else(|| "Bridge is not initialized. Call init(config_json) first.".to_string())
}

fn handle_init(config_json: &str) -> String {
    let payload = match parse_required_json::<InitConfigPayload>(config_json, "init") {
        Ok(payload) => payload,
        Err(error) => return error_response("invalid_init_payload", error),
    };

    let client_id = payload.client_id.trim();
    if client_id.is_empty() {
        return error_response("spotify_client_id_missing", "Spotify client_id is required");
    }

    let config = SpotifySessionConfig::new(client_id.to_string());
    let engine = Arc::new(SpotifySessionEngine::new(
        config.clone(),
        AndroidSpotifyBackend,
    ));
    let ready = TOKIO_RUNTIME.block_on(engine.is_ready());

    let mut state = match lock_state() {
        Ok(state) => state,
        Err(error) => return error_response("bridge_state_error", error),
    };
    state.client_id = Some(config.client_id.clone());
    state.engine = Some(engine);

    success_response(json!({
        "initialized": true,
        "ready": ready,
        "client_id_present": true
    }))
}

fn handle_spotify_begin_auth(config_json: &str) -> String {
    let payload = match parse_required_json::<BeginAuthPayload>(config_json, "spotify_begin_auth") {
        Ok(payload) => payload,
        Err(error) => return error_response("invalid_begin_auth_payload", error),
    };

    let client_id = payload.client_id.trim();
    let redirect_uri = payload.redirect_uri.trim();
    if client_id.is_empty() {
        return error_response("spotify_client_id_missing", "Spotify client_id is required");
    }
    if redirect_uri.is_empty() {
        return error_response(
            "spotify_redirect_uri_missing",
            "Spotify redirect_uri is required",
        );
    }

    let mut serializer = form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("response_type", "code");
    serializer.append_pair("client_id", client_id);
    serializer.append_pair("redirect_uri", redirect_uri);

    let scopes = if payload.scopes.is_empty() {
        vec![
            "streaming".to_string(),
            "user-read-email".to_string(),
            "user-read-private".to_string(),
        ]
    } else {
        payload.scopes
    };
    serializer.append_pair("scope", &scopes.join(" "));

    if let Some(state) = payload
        .state
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        serializer.append_pair("state", &state);
    }

    if let Some(code_challenge) = payload
        .code_challenge
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        serializer.append_pair("code_challenge", &code_challenge);
        serializer.append_pair("code_challenge_method", "S256");
    }

    let auth_url = format!(
        "https://accounts.spotify.com/authorize?{}",
        serializer.finish()
    );
    success_response(json!({
        "auth_url": auth_url
    }))
}

fn handle_spotify_exchange_code(code: &str, verifier: &str, redirect: &str) -> String {
    if code.trim().is_empty() {
        return error_response(
            "spotify_authorization_code_missing",
            "Spotify code is required",
        );
    }
    if verifier.trim().is_empty() {
        return error_response(
            "spotify_code_verifier_missing",
            "Spotify verifier is required",
        );
    }
    if redirect.trim().is_empty() {
        return error_response(
            "spotify_redirect_uri_missing",
            "Spotify redirect URI is required",
        );
    }

    error_response(
        "platform_auth_required",
        "Spotify code exchange remains platform-owned. Use Android SpotifyClient exchangeAuthorizationCode.",
    )
}

fn handle_spotify_validate_token(token_input: &str) -> String {
    let token = match parse_token(token_input) {
        Ok(token) => token,
        Err(error) => return error_response("invalid_spotify_token", error),
    };

    let expired = token.is_expired_at(current_epoch_seconds());
    let valid = !expired && !token.access_token.trim().is_empty();
    success_response(json!({
        "valid": valid,
        "expired": expired,
        "has_refresh_token": token.refresh_token.is_some()
    }))
}

fn handle_spotify_init_session(token_input: &str) -> String {
    let token = match parse_token(token_input) {
        Ok(token) => token,
        Err(error) => return error_response("invalid_spotify_token", error),
    };

    let engine = match require_engine() {
        Ok(engine) => engine,
        Err(error) => return error_response("bridge_not_initialized", error),
    };

    // Persist the fresh, validated token so ensure_player_connected always has
    // a current token after a re-auth, even before startQueue is called.
    let access_token = token.access_token.clone();
    if let Ok(mut state) = lock_state() {
        state.playback_access_token = Some(access_token.clone());
    }

    match TOKIO_RUNTIME.block_on(engine.initialize_with_token(token)) {
        Ok(()) => {
            let ready = TOKIO_RUNTIME.block_on(engine.is_ready());
            success_response(json!({
                "ready": ready
            }))
        }
        Err(error) => error_response(error.code.as_str(), error.message),
    }
}

fn handle_spotify_session_ready() -> String {
    let engine = match require_engine() {
        Ok(engine) => engine,
        Err(_) => {
            return success_response(json!({
                "initialized": false,
                "ready": false
            }));
        }
    };

    let ready = TOKIO_RUNTIME.block_on(engine.is_ready());
    success_response(json!({
        "initialized": true,
        "ready": ready
    }))
}

fn handle_spotify_start_queue(config_json: &str) -> String {
    let payload = match parse_required_json::<StartQueuePayload>(config_json, "spotify_start_queue")
    {
        Ok(payload) => payload,
        Err(error) => return error_response("invalid_start_queue_payload", error),
    };

    let token = payload.access_token.trim();
    if token.is_empty() {
        return error_response(
            "spotify_playback_token_missing",
            "Spotify access token is required for playback",
        );
    }

    if payload.track_ids.is_empty() {
        return error_response(
            "spotify_track_ids_missing",
            "track_ids must include at least one track",
        );
    }

    let normalized_track_ids: Vec<String> = payload
        .track_ids
        .iter()
        .filter_map(|track| normalize_spotify_track_id(track))
        .collect();

    if normalized_track_ids.is_empty() {
        return error_response(
            "spotify_track_ids_missing",
            "No valid Spotify track IDs were provided",
        );
    }

    let start_index = payload.start_index.min(normalized_track_ids.len() - 1);
    let token_owned = token.to_string();

    // Read the OAuth client_id stored during init so we can pass it to
    // librespot. Login5 binds stored credentials to the originating client ID;
    // a mismatch produces INVALID_CREDENTIALS when loading tracks.
    let client_id_owned = {
        let state = match lock_state() {
            Ok(s) => s,
            Err(e) => return error_response("bridge_state_error", e),
        };
        match state.client_id.clone() {
            Some(id) => id,
            None => {
                return error_response(
                    "spotify_client_id_missing",
                    "client_id not set — call init() first",
                );
            }
        }
    };

    // If the player is already healthy, skip the expensive disconnect/reconnect
    // cycle and just load the new track directly. This avoids a ~250ms+ gap where
    // the old track's audio buffer continues playing while a brand-new session
    // connects, which caused the "wrong audio on manual skip" bug.
    let already_healthy = TOKIO_RUNTIME.block_on(PLAYER.is_healthy());

    if !already_healthy {
        // Fresh connection (or stale session): tear down, then connect.
        TOKIO_RUNTIME.block_on(PLAYER.disconnect());

        if let Err(error) = TOKIO_RUNTIME.block_on(PLAYER.connect(&token_owned, &client_id_owned)) {
            return error_response("librespot_connect_failed", error.message);
        }
    }

    if let Err(error) =
        TOKIO_RUNTIME.block_on(PLAYER.start_queue(&normalized_track_ids, start_index))
    {
        return error_response("librespot_start_queue_failed", error.message);
    }

    // Apply current effective output volume right after queue start.
    let output_volume = {
        let state = match lock_state() {
            Ok(state) => state,
            Err(error) => return error_response("bridge_state_error", error),
        };
        normalized_volume_for_source(
            state.playback_volume_percent as i32,
            AudioNormalizationSource::Spotify,
            &state.audio_normalization,
            &state.adaptive_normalization,
        )
    };
    if let Err(error) = TOKIO_RUNTIME.block_on(PLAYER.set_volume(output_volume)) {
        return error_response("librespot_set_volume_failed", error.message);
    }

    let (track_count, start_index, preload_track_id) = {
        let mut state = match lock_state() {
            Ok(state) => state,
            Err(error) => return error_response("bridge_state_error", error),
        };
        // Persist the token so later transport commands can reconnect transparently.
        state.playback_access_token = Some(token_owned);
        state.playback_queue = normalized_track_ids;
        state.playback_index = start_index;
        let track_count = state.playback_queue.len();
        let next_preload_index = if state.playback_index + 1 < track_count {
            Some(state.playback_index + 1)
        } else if state.playback_repeat_mode == RepeatModeState::All && track_count > 0 {
            Some(0)
        } else {
            None
        };
        let preload_track_id = next_preload_index
            .and_then(|index| state.playback_queue.get(index))
            .cloned();
        (track_count, state.playback_index, preload_track_id)
    };

    // Proactively preload the next track while the current one starts playing.
    try_preload_spotify_track_id(preload_track_id.as_deref());

    success_response(json!({
        "started": true,
        "track_count": track_count,
        "start_index": start_index
    }))
}

/// Ensures the librespot player is connected, reconnecting from the stored token
/// if necessary. Also re-queues the stored track list after reconnect so that
/// transport commands (play, seek, etc.) have a loaded track to act on.
/// Returns an error string if the connection cannot be established.
fn ensure_player_connected() -> Result<(), String> {
    if TOKIO_RUNTIME.block_on(PLAYER.is_healthy()) {
        return Ok(());
    }

    // If connected but unhealthy (stale session / dead player thread),
    // tear down the old state so the reconnect below starts fresh.
    if TOKIO_RUNTIME.block_on(PLAYER.is_connected()) {
        TOKIO_RUNTIME.block_on(PLAYER.disconnect());
    }

    let (token, client_id, queue, index, repeat_mode) = {
        let state = lock_state()?;
        let token = state.playback_access_token.clone().ok_or_else(|| {
            "librespot_not_connected: No stored access token — call startQueue first".to_string()
        })?;
        let client_id = state.client_id.clone().ok_or_else(|| {
            "librespot_not_connected: client_id not set — call init() first".to_string()
        })?;
        (
            token,
            client_id,
            state.playback_queue.clone(),
            state.playback_index,
            state.playback_repeat_mode,
        )
    };

    TOKIO_RUNTIME
        .block_on(PLAYER.connect(&token, &client_id))
        .map_err(|e| e.message)?;

    // Restore the last-known track so subsequent play/seek/pause work.
    if !queue.is_empty() {
        TOKIO_RUNTIME
            .block_on(PLAYER.start_queue(&queue, index))
            .map_err(|e| e.message)?;

        try_preload_next_spotify_track(&queue, index, repeat_mode);
    }

    Ok(())
}

/// Best-effort preload of the track after `current_index` in `queue`.
/// Failures are logged and silently swallowed — a missed preload degrades to
/// a normal stream start on track change, but does not break playback.
fn try_preload_next_spotify_track(
    queue: &[String],
    current_index: usize,
    repeat_mode: RepeatModeState,
) {
    let next_preload_index = if current_index + 1 < queue.len() {
        Some(current_index + 1)
    } else if repeat_mode == RepeatModeState::All && !queue.is_empty() {
        Some(0)
    } else {
        None
    };

    try_preload_spotify_track_id(
        next_preload_index.and_then(|index| queue.get(index).map(String::as_str)),
    );
}

fn try_preload_spotify_track_id(next_track_id: Option<&str>) {
    if let Some(next_id) = next_track_id
        && let Err(e) = TOKIO_RUNTIME.block_on(PLAYER.preload(next_id))
    {
        log::debug!("spotify preload next track skipped: {}", e.message);
    }
}

fn run_connected_player_command<F>(success_payload: serde_json::Value, command: F) -> String
where
    F: FnOnce() -> Result<(), SpotifyEngineError>,
{
    if let Err(msg) = ensure_player_connected() {
        return error_response("spotify_playback_command_failed", msg);
    }

    match command() {
        Ok(()) => success_response(success_payload),
        Err(error) => error_response("spotify_playback_command_failed", error.message),
    }
}

fn handle_spotify_play() -> String {
    run_connected_player_command(json!({ "playing": true }), || {
        TOKIO_RUNTIME.block_on(PLAYER.play())
    })
}

fn handle_spotify_pause() -> String {
    run_connected_player_command(json!({ "playing": false }), || {
        TOKIO_RUNTIME.block_on(PLAYER.pause())
    })
}

fn handle_spotify_next() -> String {
    let mut state = match lock_state() {
        Ok(state) => state,
        Err(error) => return error_response("bridge_state_error", error),
    };

    if state.playback_queue.is_empty() {
        return error_response("librespot_empty_queue", "No playback queue loaded");
    }

    let repeat_mode = state.playback_repeat_mode;
    let next_index = if repeat_mode == RepeatModeState::One {
        // Replay the current track.
        state.playback_index
    } else if state.playback_index + 1 < state.playback_queue.len() {
        state.playback_index + 1
    } else if repeat_mode == RepeatModeState::All {
        0
    } else {
        return success_response(json!({ "ok": true, "end_of_queue": true }));
    };

    let track_ids = state.playback_queue.clone();
    state.playback_index = next_index;
    drop(state);

    if let Err(msg) = ensure_player_connected() {
        return error_response("spotify_playback_command_failed", msg);
    }
    if let Err(error) = TOKIO_RUNTIME.block_on(PLAYER.start_queue(&track_ids, next_index)) {
        return error_response("librespot_next_failed", error.message);
    }

    try_preload_next_spotify_track(&track_ids, next_index, repeat_mode);

    success_response(json!({ "ok": true }))
}

fn handle_spotify_previous() -> String {
    let mut state = match lock_state() {
        Ok(state) => state,
        Err(error) => return error_response("bridge_state_error", error),
    };

    if state.playback_queue.is_empty() {
        return error_response("librespot_empty_queue", "No playback queue loaded");
    }

    let prev_index = if state.playback_index > 0 {
        state.playback_index - 1
    } else if state.playback_repeat_mode == RepeatModeState::All {
        state.playback_queue.len() - 1
    } else {
        0
    };
    let repeat_mode = state.playback_repeat_mode;

    let track_ids = state.playback_queue.clone();
    state.playback_index = prev_index;
    drop(state);

    if let Err(msg) = ensure_player_connected() {
        return error_response("spotify_playback_command_failed", msg);
    }
    if let Err(error) = TOKIO_RUNTIME.block_on(PLAYER.start_queue(&track_ids, prev_index)) {
        return error_response("librespot_previous_failed", error.message);
    }

    try_preload_next_spotify_track(&track_ids, prev_index, repeat_mode);

    success_response(json!({ "ok": true }))
}

fn handle_spotify_seek(config_json: &str) -> String {
    let payload = match parse_required_json::<SeekPayload>(config_json, "spotify_seek") {
        Ok(payload) => payload,
        Err(error) => return error_response("invalid_seek_payload", error),
    };

    run_connected_player_command(json!({ "position_ms": payload.position_ms }), || {
        TOKIO_RUNTIME.block_on(PLAYER.seek(payload.position_ms))
    })
}

fn handle_spotify_set_volume(config_json: &str) -> String {
    let payload = match parse_required_json::<VolumePayload>(config_json, "spotify_set_volume") {
        Ok(payload) => payload,
        Err(error) => return error_response("invalid_set_volume_payload", error),
    };

    let (requested_volume_percent, output_volume_percent) = {
        let mut state = match lock_state() {
            Ok(state) => state,
            Err(error) => return error_response("bridge_state_error", error),
        };

        let requested = payload.volume_percent.clamp(0, 100) as u8;
        let output = normalized_volume_for_source(
            requested as i32,
            AudioNormalizationSource::Spotify,
            &state.audio_normalization,
            &state.adaptive_normalization,
        );
        state.playback_volume_percent = requested;
        (requested, output)
    };

    run_connected_player_command(
        json!({
            "volume_percent": requested_volume_percent,
            "output_volume_percent": output_volume_percent
        }),
        || TOKIO_RUNTIME.block_on(PLAYER.set_volume(output_volume_percent)),
    )
}

fn handle_get_audio_normalization_settings() -> String {
    let state = match lock_state() {
        Ok(state) => state,
        Err(error) => return error_response("bridge_state_error", error),
    };

    success_response(json!({
        "enabled": state.audio_normalization.enabled,
        "strict_mode": state.audio_normalization.strict_mode,
        "target": state.audio_normalization.target,
    }))
}

fn handle_set_audio_normalization_settings(config_json: &str) -> String {
    let payload = match parse_required_json::<AudioNormalizationSettingsPayload>(
        config_json,
        "set_audio_normalization_settings",
    ) {
        Ok(payload) => payload,
        Err(error) => return error_response("invalid_audio_normalization_payload", error),
    };

    let (requested_volume_percent, output_volume_percent) = {
        let mut state = match lock_state() {
            Ok(state) => state,
            Err(error) => return error_response("bridge_state_error", error),
        };

        state.audio_normalization.enabled = payload.enabled;
        state.audio_normalization.strict_mode = payload.strict_mode;
        state.audio_normalization.target = INTERNAL_NORMALIZATION_TARGET;

        let output = normalized_volume_for_source(
            state.playback_volume_percent as i32,
            AudioNormalizationSource::Spotify,
            &state.audio_normalization,
            &state.adaptive_normalization,
        );
        (state.playback_volume_percent, output)
    };

    if TOKIO_RUNTIME.block_on(PLAYER.is_connected())
        && let Err(error) = TOKIO_RUNTIME.block_on(PLAYER.set_volume(output_volume_percent))
    {
        return error_response("librespot_set_volume_failed", error.message);
    }

    success_response(json!({
        "enabled": payload.enabled,
        "strict_mode": payload.strict_mode,
        "target": INTERNAL_NORMALIZATION_TARGET,
        "volume_percent": requested_volume_percent,
        "output_volume_percent": output_volume_percent,
    }))
}

fn handle_apply_audio_normalization_volume(config_json: &str) -> String {
    let payload = match parse_required_json::<ApplyNormalizationPayload>(
        config_json,
        "apply_audio_normalization_volume",
    ) {
        Ok(payload) => payload,
        Err(error) => return error_response("invalid_apply_normalization_payload", error),
    };

    let source = parse_normalization_source(payload.source.as_deref());
    let normalized_volume = {
        let state = match lock_state() {
            Ok(state) => state,
            Err(error) => return error_response("bridge_state_error", error),
        };

        normalized_volume_for_source(
            payload.volume_percent,
            source,
            &state.audio_normalization,
            &state.adaptive_normalization,
        )
    };

    success_response(json!({
        "normalized_volume_percent": normalized_volume,
    }))
}

fn handle_spotify_set_shuffle(config_json: &str) -> String {
    let payload = match parse_required_json::<ShufflePayload>(config_json, "spotify_set_shuffle") {
        Ok(payload) => payload,
        Err(error) => return error_response("invalid_set_shuffle_payload", error),
    };

    if let Ok(mut state) = lock_state() {
        state.playback_shuffle = payload.enabled;
    }

    // Shuffle is maintained in the queue manager (Kotlin side). Librespot itself
    // does not expose a shuffle API; the queue order is controlled by the caller.
    success_response(json!({ "shuffle_enabled": payload.enabled }))
}

fn handle_spotify_set_repeat_mode(config_json: &str) -> String {
    let payload =
        match parse_required_json::<RepeatModePayload>(config_json, "spotify_set_repeat_mode") {
            Ok(payload) => payload,
            Err(error) => return error_response("invalid_set_repeat_mode_payload", error),
        };

    let mode = match RepeatModeState::from_input(&payload.mode) {
        Some(mode) => mode,
        None => {
            return error_response(
                "spotify_repeat_mode_invalid",
                "repeat mode must be one of: off, one, all",
            );
        }
    };

    if let Ok(mut state) = lock_state() {
        state.playback_repeat_mode = mode;
    }

    // Repeat mode is tracked here and passed back to Kotlin to govern queue wrap-around.
    success_response(json!({ "repeat_mode": mode.as_snapshot_str() }))
}

fn handle_spotify_snapshot() -> String {
    let snapshot = TOKIO_RUNTIME.block_on(PLAYER.snapshot());
    let state = match lock_state() {
        Ok(state) => state,
        Err(error) => return error_response("bridge_state_error", error),
    };
    let current_track_id = snapshot
        .current_track_id
        .or_else(|| state.playback_queue.get(state.playback_index).cloned());
    success_response(json!({
        "is_playing": snapshot.is_playing,
        "progress_ms": snapshot.progress_ms,
        "end_of_track_count": snapshot.end_of_track_count,
        "volume_percent": state.playback_volume_percent,
        "output_volume_percent": snapshot.volume_percent,
        "shuffle_enabled": state.playback_shuffle,
        "repeat_mode": state.playback_repeat_mode.as_snapshot_str(),
        "current_track_id": current_track_id
    }))
}

fn page_slice<T>(items: Vec<T>, offset: usize, limit: usize) -> Vec<T> {
    items.into_iter().skip(offset).take(limit).collect()
}

fn require_field(value: Option<String>, field: &str) -> Result<String, String> {
    match value.map(|v| v.trim().to_string()) {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Err(format!("Missing required provider field: {}", field)),
    }
}

fn json_value<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value)
        .map_err(|error| format!("Failed to encode provider response: {}", error))
}

async fn dispatch_provider_operation(
    client: &dyn ProviderApi,
    operation: &str,
    session: &ProviderAuthRequest,
    payload: &ProviderApiCallPayload,
    offset: usize,
    limit: usize,
) -> Result<Value, String> {
    // Log incoming provider dispatch for visibility (avoid printing auth tokens)
    let session_keys: Vec<String> = session.as_map().keys().cloned().collect();
    log::info!(
        "dispatch_provider_operation: source={} operation={} id={:?} offset={} limit={} session_keys={:?}",
        client.source(),
        operation,
        payload.id.clone().unwrap_or_default(),
        offset,
        limit,
        session_keys
    );
    match operation {
        "validate_connection" => {
            // Use 10-second timeout for connection validation
            let validation = timeout(
                Duration::from_secs(10),
                client.validate_connection(session)
            )
            .await
            .map_err(|_| "Connection validation timed out after 10 seconds. Provider server is not responding.".to_string())?
            .map_err(|error| error.0)?;
            match validation {
                ProviderConnectionCheck::Connected { username, metadata } => Ok(json!({
                    "connected": true,
                    "username": username,
                    "metadata": metadata
                })),
                ProviderConnectionCheck::Failed(message) => Ok(json!({
                    "connected": false,
                    "message": message
                })),
            }
        }
        "get_playlists" => {
            // Use 30-second timeout for playlist listing
            let playlists = timeout(Duration::from_secs(30), client.get_playlists(session))
                .await
                .map_err(|_| {
                    "Playlists fetch timed out after 30 seconds. Provider server is not responding."
                        .to_string()
                })?
                .map_err(|error| error.0)?;
            Ok(json!({ "playlists": json_value(page_slice(playlists, offset, limit))? }))
        }
        "get_playlist" => {
            let id = require_field(payload.id.clone(), "id")?;
            // Use 120-second timeout for playlist fetch (especially important for Plex)
            let timeout_secs = 120;
            let result = timeout(Duration::from_secs(timeout_secs), client.get_playlist(session, &id))
                .await
                .map_err(|_| {
                    format!(
                        "Playlist fetch timed out after {} seconds (id={}, offset={}, limit={}). Provider server is not responding.",
                        timeout_secs, id, offset, limit
                    )
                })?
                .map_err(|error| error.0)?;
            let mut playlist = result;
            playlist.tracks = page_slice(playlist.tracks, offset, limit);
            Ok(json!({ "playlist": json_value(playlist)? }))
        }
        "search_tracks" => {
            let query = require_field(payload.query.clone(), "query")?;
            let tracks = timeout(
                Duration::from_secs(30),
                client.search_tracks(session, &query),
            )
            .await
            .map_err(|_| {
                "Track search timed out after 30 seconds. Provider server is not responding."
                    .to_string()
            })?
            .map_err(|error| error.0)?;
            Ok(json!({ "tracks": json_value(page_slice(tracks, offset, limit))? }))
        }
        "search_playlists" => {
            let query = require_field(payload.query.clone(), "query")?;
            let playlists = timeout(
                Duration::from_secs(30),
                client.search_playlists(session, &query),
            )
            .await
            .map_err(|_| {
                "Playlist search timed out after 30 seconds. Provider server is not responding."
                    .to_string()
            })?
            .map_err(|error| error.0)?;
            Ok(json!({ "playlists": json_value(page_slice(playlists, offset, limit))? }))
        }
        "get_recently_played" => {
            let tracks = timeout(
                Duration::from_secs(30),
                client.get_recently_played(session, limit)
            )
            .await
            .map_err(|_| "Recently played fetch timed out after 30 seconds. Provider server is not responding.".to_string())?
            .map_err(|error| error.0)?;
            Ok(json!({ "tracks": json_value(tracks)? }))
        }
        _ => Err(format!(
            "Unsupported provider operation for {}: {}",
            client.source(),
            operation
        )),
    }
}

/// Clamps `limit` to `1..=1000` (defaulting to 300 when absent) and ensures
/// `page_size` is present in `session`, deriving it from `limit` when absent.
/// This keeps provider pagination aligned with the requested limit.
fn prepare_provider_call(
    session: HashMap<String, String>,
    limit: Option<usize>,
) -> (HashMap<String, String>, usize) {
    // Limit is now configurable via the request (e.g., from Android's configured page size).
    // Enforce a reasonable maximum of 1000 to prevent excessive memory allocation.
    let limit = limit.unwrap_or(300).clamp(1, 1000);

    // If the caller has not explicitly provided a `page_size` in the session payload,
    // derive it from `limit` so that provider pagination stays aligned.
    let mut session = session;
    session
        .entry("page_size".to_string())
        .or_insert_with(|| limit.to_string());

    (session, limit)
}

fn handle_provider_api_call(config_json: &str) -> String {
    let payload =
        match parse_required_json::<ProviderApiCallPayload>(config_json, "provider_api_call") {
            Ok(payload) => payload,
            Err(error) => return error_response("invalid_provider_api_payload", error),
        };

    let source = payload.source.trim().to_ascii_lowercase();
    let operation = payload.operation.trim().to_ascii_lowercase();
    let offset = payload.offset.unwrap_or(0);

    let (session_map, limit) = prepare_provider_call(payload.session.clone(), payload.limit);
    let session = ProviderAuthRequest::new(session_map);

    let result: Result<Value, String> = TOKIO_RUNTIME.block_on(async {
        match source.as_str() {
            "jellyfin" => {
                let client = JellyfinApiClient::new();
                dispatch_provider_operation(&client, &operation, &session, &payload, offset, limit)
                    .await
            }
            "plex" => {
                let client = PlexApiClient::new();
                dispatch_provider_operation(&client, &operation, &session, &payload, offset, limit)
                    .await
            }
            "spotify" => {
                let client = SpotifyApiClient::new();
                dispatch_provider_operation(&client, &operation, &session, &payload, offset, limit)
                    .await
            }
            _ => Err(format!("Unsupported provider source: {}", source)),
        }
    });

    match result {
        Ok(data) => success_response(data),
        Err(error) => error_response("provider_api_error", error),
    }
}

fn into_jstring(env: &mut JNIEnv<'_>, payload: String) -> jstring {
    match env.new_string(payload) {
        Ok(value) => value.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn read_jstring(
    env: &mut JNIEnv<'_>,
    argument: JString<'_>,
    argument_name: &'static str,
) -> Result<String, String> {
    env.get_string(&argument)
        .map(|value| value.into())
        .map_err(|error| format!("Failed to read {} argument: {}", argument_name, error))
}

/// Called by the Android runtime as soon as `libany_player_ffi_android.so` is
/// loaded. We use it to wire up `android_logger` so that all `log::*` calls
/// Called by the JVM when our shared library is first loaded.
///
/// 1. Initialises the `android_logger` backend so Rust `log::*` calls are
///    visible in logcat under the tag `any_player_rust`.
/// 2. Registers the Android JavaVM + Application-context pointer with
///    `ndk_context` so that cpal/AAudio can open audio streams without
///    hitting the "android context was not initialized" panic.
///
/// # Safety
/// The JVM must invoke this function with a valid, non-null `JavaVM*` that
/// remains valid for the lifetime of the process, as guaranteed by the JNI
/// specification for `JNI_OnLoad`.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn JNI_OnLoad(
    vm: *mut jni::sys::JavaVM,
    _reserved: *mut std::ffi::c_void,
) -> jni::sys::jint {
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("any_player_rust")
            .with_max_level(log::LevelFilter::Debug),
    );
    log::info!("any_player_rust JNI_OnLoad: logger initialised");

    // Initialise ndk-context so that cpal's AAudio backend can obtain the
    // Android AudioManager via JNI.  We do this by calling into Java to
    // retrieve the current Application context and storing the pair
    // (JavaVM*, jobject context) as a global reference.
    //
    // Safety: `vm` is valid for the lifetime of the process; the global JNI
    // reference to the context is kept alive indefinitely (intentional — we
    // need it for the lifetime of the player).
    unsafe {
        match init_ndk_context(vm) {
            Ok(()) => log::info!("any_player_rust JNI_OnLoad: ndk-context initialised"),
            Err(e) => log::error!(
                "any_player_rust JNI_OnLoad: failed to initialise ndk-context: {}. \
                 Audio playback may not work.",
                e
            ),
        }
    }

    jni::sys::JNI_VERSION_1_6
}

/// Retrieves the Android Application context via reflection and stores it in
/// `ndk_context` so that cpal's AAudio host can use it.
///
/// # Safety
/// `vm` must be a valid, non-null `JavaVM*` pointer for the lifetime of the
/// process.
unsafe fn init_ndk_context(vm: *mut jni::sys::JavaVM) -> Result<(), String> {
    use jni::{JNIEnv, JavaVM};

    let jvm = unsafe { JavaVM::from_raw(vm) }.map_err(|e| format!("JavaVM::from_raw: {e}"))?;

    let mut guard = jvm
        .attach_current_thread()
        .map_err(|e| format!("attach_current_thread: {e}"))?;
    let env: &mut JNIEnv<'_> = &mut guard;

    // ActivityThread.currentApplication() is a static method that returns the
    // global Application singleton — it's available from any thread, even
    // before any Activity is started.
    let activity_thread = env
        .find_class("android/app/ActivityThread")
        .map_err(|e| format!("find_class ActivityThread: {e}"))?;

    let app_obj = env
        .call_static_method(
            activity_thread,
            "currentApplication",
            "()Landroid/app/Application;",
            &[],
        )
        .map_err(|e| format!("call currentApplication: {e}"))?
        .l()
        .map_err(|e| format!("result as object: {e}"))?;

    if app_obj.is_null() {
        return Err("currentApplication() returned null".to_string());
    }

    // Promote to a global JNI reference so the object is not collected.
    let global_app = env
        .new_global_ref(app_obj)
        .map_err(|e| format!("new_global_ref: {e}"))?;

    // Leak the global ref intentionally — we need it for the process lifetime.
    let ctx_ptr = global_app.as_raw() as *mut std::ffi::c_void;
    std::mem::forget(global_app);

    // Safety: both `vm` and `ctx_ptr` are valid for the process lifetime.
    unsafe {
        ndk_context::initialize_android_context(vm as *mut std::ffi::c_void, ctx_ptr);
    }

    Ok(())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_init(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    config_json: JString<'_>,
) -> jstring {
    let payload = match read_jstring(&mut env, config_json, "config_json") {
        Ok(value) => handle_init(&value),
        Err(error) => error_response("jni_argument_error", error),
    };
    into_jstring(&mut env, payload)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_spotifyBeginAuth(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    config_json: JString<'_>,
) -> jstring {
    let payload = match read_jstring(&mut env, config_json, "config_json") {
        Ok(value) => handle_spotify_begin_auth(&value),
        Err(error) => error_response("jni_argument_error", error),
    };
    into_jstring(&mut env, payload)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_spotifyExchangeCode(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    code: JString<'_>,
    verifier: JString<'_>,
    redirect: JString<'_>,
) -> jstring {
    let payload = match (
        read_jstring(&mut env, code, "code"),
        read_jstring(&mut env, verifier, "verifier"),
        read_jstring(&mut env, redirect, "redirect"),
    ) {
        (Ok(code), Ok(verifier), Ok(redirect)) => {
            handle_spotify_exchange_code(&code, &verifier, &redirect)
        }
        (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
            error_response("jni_argument_error", error)
        }
    };
    into_jstring(&mut env, payload)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_spotifyValidateToken(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    token_input: JString<'_>,
) -> jstring {
    let payload = match read_jstring(&mut env, token_input, "token_input") {
        Ok(value) => handle_spotify_validate_token(&value),
        Err(error) => error_response("jni_argument_error", error),
    };
    into_jstring(&mut env, payload)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_spotifyInitSession(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    token_input: JString<'_>,
) -> jstring {
    let payload = match read_jstring(&mut env, token_input, "token_input") {
        Ok(value) => handle_spotify_init_session(&value),
        Err(error) => error_response("jni_argument_error", error),
    };
    into_jstring(&mut env, payload)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_spotifySessionReady(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    let payload = handle_spotify_session_ready();
    into_jstring(&mut env, payload)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_spotifyStartQueue(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    config_json: JString<'_>,
) -> jstring {
    let payload = match read_jstring(&mut env, config_json, "config_json") {
        Ok(value) => handle_spotify_start_queue(&value),
        Err(error) => error_response("jni_argument_error", error),
    };
    into_jstring(&mut env, payload)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_spotifyPlay(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    into_jstring(&mut env, handle_spotify_play())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_spotifyPause(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    into_jstring(&mut env, handle_spotify_pause())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_spotifyNext(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    into_jstring(&mut env, handle_spotify_next())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_spotifyPrevious(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    into_jstring(&mut env, handle_spotify_previous())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_spotifySeek(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    config_json: JString<'_>,
) -> jstring {
    let payload = match read_jstring(&mut env, config_json, "config_json") {
        Ok(value) => handle_spotify_seek(&value),
        Err(error) => error_response("jni_argument_error", error),
    };
    into_jstring(&mut env, payload)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_spotifySetVolume(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    config_json: JString<'_>,
) -> jstring {
    let payload = match read_jstring(&mut env, config_json, "config_json") {
        Ok(value) => handle_spotify_set_volume(&value),
        Err(error) => error_response("jni_argument_error", error),
    };
    into_jstring(&mut env, payload)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_getAudioNormalizationSettings(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    into_jstring(&mut env, handle_get_audio_normalization_settings())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_setAudioNormalizationSettings(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    config_json: JString<'_>,
) -> jstring {
    let payload = match read_jstring(&mut env, config_json, "config_json") {
        Ok(value) => handle_set_audio_normalization_settings(&value),
        Err(error) => error_response("jni_argument_error", error),
    };
    into_jstring(&mut env, payload)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_applyAudioNormalizationVolume(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    config_json: JString<'_>,
) -> jstring {
    let payload = match read_jstring(&mut env, config_json, "config_json") {
        Ok(value) => handle_apply_audio_normalization_volume(&value),
        Err(error) => error_response("jni_argument_error", error),
    };
    into_jstring(&mut env, payload)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_spotifySetShuffle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    config_json: JString<'_>,
) -> jstring {
    let payload = match read_jstring(&mut env, config_json, "config_json") {
        Ok(value) => handle_spotify_set_shuffle(&value),
        Err(error) => error_response("jni_argument_error", error),
    };
    into_jstring(&mut env, payload)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_spotifySetRepeatMode(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    config_json: JString<'_>,
) -> jstring {
    let payload = match read_jstring(&mut env, config_json, "config_json") {
        Ok(value) => handle_spotify_set_repeat_mode(&value),
        Err(error) => error_response("jni_argument_error", error),
    };
    into_jstring(&mut env, payload)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_spotifySnapshot(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    into_jstring(&mut env, handle_spotify_snapshot())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anyplayer_android_core_rust_RustBridgeNative_providerApiCall(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    config_json: JString<'_>,
) -> jstring {
    let payload = match read_jstring(&mut env, config_json, "config_json") {
        Ok(value) => handle_provider_api_call(&value),
        Err(error) => error_response("jni_argument_error", error),
    };
    into_jstring(&mut env, payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    fn parse_json(payload: &str) -> Value {
        serde_json::from_str(payload).expect("expected valid json response")
    }

    #[test]
    fn smoke_spotify_token_session_readiness_flow() {
        let _guard = TEST_MUTEX.lock().expect("test mutex");

        let init = parse_json(&handle_init(r#"{"client_id":"test-client-id"}"#));
        assert_eq!(init["ok"], Value::Bool(true));

        let validate = parse_json(&handle_spotify_validate_token("test-access-token"));
        assert_eq!(validate["ok"], Value::Bool(true));
        assert_eq!(validate["data"]["valid"], Value::Bool(true));

        let init_session = parse_json(&handle_spotify_init_session("test-access-token"));
        assert_eq!(init_session["ok"], Value::Bool(true));
        assert_eq!(init_session["data"]["ready"], Value::Bool(true));

        let ready = parse_json(&handle_spotify_session_ready());
        assert_eq!(ready["ok"], Value::Bool(true));
        assert_eq!(ready["data"]["initialized"], Value::Bool(true));
        assert_eq!(ready["data"]["ready"], Value::Bool(true));
    }

    #[test]
    fn spotify_exchange_code_reports_platform_auth_required() {
        let _guard = TEST_MUTEX.lock().expect("test mutex");

        let payload = parse_json(&handle_spotify_exchange_code(
            "code", "verifier", "redirect",
        ));
        assert_eq!(payload["ok"], Value::Bool(false));
        assert_eq!(
            payload["error"]["code"],
            Value::String("platform_auth_required".to_string())
        );
    }

    #[test]
    fn spotify_start_queue_requires_token() {
        let _guard = TEST_MUTEX.lock().expect("test mutex");

        let payload = parse_json(&handle_spotify_start_queue(
            r#"{"access_token":"","track_ids":["abc"],"start_index":0}"#,
        ));
        assert_eq!(payload["ok"], Value::Bool(false));
        assert_eq!(
            payload["error"]["code"],
            Value::String("spotify_playback_token_missing".to_string())
        );
    }

    #[test]
    fn spotify_snapshot_returns_idle_state_when_not_playing() {
        let _guard = TEST_MUTEX.lock().expect("test mutex");

        // Without a librespot connection the snapshot should return a valid
        // ok response with is_playing=false (the player default).
        let payload = parse_json(&handle_spotify_snapshot());
        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(payload["data"]["is_playing"], Value::Bool(false));
    }

    #[test]
    fn get_audio_normalization_settings_reflects_state() {
        let _guard = TEST_MUTEX.lock().expect("test mutex");

        {
            let mut state = lock_state().expect("lock_state");
            state.audio_normalization.enabled = true;
            state.audio_normalization.strict_mode = true;
            state.audio_normalization.target = INTERNAL_NORMALIZATION_TARGET;
        }

        let payload = parse_json(&handle_get_audio_normalization_settings());
        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(payload["data"]["enabled"], Value::Bool(true));
        assert_eq!(payload["data"]["strict_mode"], Value::Bool(true));
        assert_eq!(
            payload["data"]["target"],
            Value::Number(INTERNAL_NORMALIZATION_TARGET.into())
        );
    }

    #[test]
    fn set_audio_normalization_settings_updates_state_and_returns_ok() {
        let _guard = TEST_MUTEX.lock().expect("test mutex");

        {
            let mut state = lock_state().expect("lock_state");
            state.playback_volume_percent = 50;
            state.audio_normalization.enabled = false;
            state.audio_normalization.strict_mode = false;
        }

        let payload = parse_json(&handle_set_audio_normalization_settings(
            r#"{"enabled":true,"strict_mode":true}"#,
        ));
        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(payload["data"]["enabled"], Value::Bool(true));
        assert_eq!(payload["data"]["strict_mode"], Value::Bool(true));
        assert_eq!(
            payload["data"]["target"],
            Value::Number(INTERNAL_NORMALIZATION_TARGET.into())
        );
        assert_eq!(payload["data"]["volume_percent"], Value::Number(50.into()));

        let state = lock_state().expect("lock_state");
        assert!(state.audio_normalization.enabled);
        assert!(state.audio_normalization.strict_mode);
    }

    #[test]
    fn apply_audio_normalization_volume_returns_normalized_volume() {
        let _guard = TEST_MUTEX.lock().expect("test mutex");

        {
            let mut state = lock_state().expect("lock_state");
            state.audio_normalization.enabled = true;
            state.audio_normalization.strict_mode = false;
            state.audio_normalization.target = INTERNAL_NORMALIZATION_TARGET;
        }

        let payload = parse_json(&handle_apply_audio_normalization_volume(
            r#"{"volume_percent":60,"source":"spotify"}"#,
        ));
        assert_eq!(payload["ok"], Value::Bool(true));

        let normalized = payload["data"]["normalized_volume_percent"]
            .as_u64()
            .expect("normalized_volume_percent should be a number");
        assert!(normalized <= 100);
    }

    #[test]
    fn apply_audio_normalization_volume_rejects_invalid_payload() {
        let _guard = TEST_MUTEX.lock().expect("test mutex");

        let payload = parse_json(&handle_apply_audio_normalization_volume("not-json"));
        assert_eq!(payload["ok"], Value::Bool(false));
        assert_eq!(
            payload["error"]["code"],
            Value::String("invalid_apply_normalization_payload".to_string())
        );
    }

    #[test]
    fn provider_api_call_rejects_unknown_source() {
        let _guard = TEST_MUTEX.lock().expect("test mutex");

        let payload = parse_json(&handle_provider_api_call(
            r#"{"source":"unknown","operation":"get_playlists","session":{}}"#,
        ));
        assert_eq!(payload["ok"], Value::Bool(false));
        assert_eq!(
            payload["error"]["code"],
            Value::String("provider_api_error".to_string())
        );
    }

    #[test]
    fn provider_api_call_rejects_invalid_payload() {
        let _guard = TEST_MUTEX.lock().expect("test mutex");

        let payload = parse_json(&handle_provider_api_call("not-json"));
        assert_eq!(payload["ok"], Value::Bool(false));
        assert_eq!(
            payload["error"]["code"],
            Value::String("invalid_provider_api_payload".to_string())
        );
    }

    #[test]
    fn provider_api_call_jellyfin_rejects_unknown_operation() {
        let _guard = TEST_MUTEX.lock().expect("test mutex");

        let payload = parse_json(&handle_provider_api_call(
            r#"{"source":"jellyfin","operation":"unsupported_op","session":{}}"#,
        ));
        assert_eq!(payload["ok"], Value::Bool(false));
        assert_eq!(
            payload["error"]["code"],
            Value::String("provider_api_error".to_string())
        );
    }

    #[test]
    fn provider_api_call_plex_rejects_unknown_operation() {
        let _guard = TEST_MUTEX.lock().expect("test mutex");

        let payload = parse_json(&handle_provider_api_call(
            r#"{"source":"plex","operation":"unsupported_op","session":{}}"#,
        ));
        assert_eq!(payload["ok"], Value::Bool(false));
        assert_eq!(
            payload["error"]["code"],
            Value::String("provider_api_error".to_string())
        );
    }

    #[test]
    fn prepare_provider_call_defaults_limit_to_300_and_sets_page_size() {
        let (session, limit) = prepare_provider_call(HashMap::new(), None);
        assert_eq!(limit, 300);
        assert_eq!(session["page_size"], "300");
    }

    #[test]
    fn prepare_provider_call_uses_provided_limit_as_page_size() {
        let (session, limit) = prepare_provider_call(HashMap::new(), Some(500));
        assert_eq!(limit, 500);
        assert_eq!(session["page_size"], "500");
    }

    #[test]
    fn prepare_provider_call_clamps_limit_to_max_1000() {
        let (session, limit) = prepare_provider_call(HashMap::new(), Some(2000));
        assert_eq!(limit, 1000);
        assert_eq!(session["page_size"], "1000");
    }

    #[test]
    fn prepare_provider_call_clamps_limit_to_min_1() {
        let (session, limit) = prepare_provider_call(HashMap::new(), Some(0));
        assert_eq!(limit, 1);
        assert_eq!(session["page_size"], "1");
    }

    #[test]
    fn prepare_provider_call_preserves_existing_page_size_in_session() {
        let mut existing_session = HashMap::new();
        existing_session.insert("page_size".to_string(), "50".to_string());
        let (session, limit) = prepare_provider_call(existing_session, Some(200));
        assert_eq!(limit, 200);
        // page_size from the caller's session must not be overridden
        assert_eq!(session["page_size"], "50");
    }
}
