use any_player_spotify_engine::{
    SpotifyEngineError, SpotifySessionBackend, SpotifySessionConfig, SpotifySessionEngine,
    SpotifyToken,
};
use async_trait::async_trait;
use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::jstring;
use once_cell::sync::Lazy;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::runtime::{Builder, Runtime};
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

    fn from_spotify_api(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "track" => Self::One,
            "context" => Self::All,
            _ => Self::Off,
        }
    }

    fn as_snapshot_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::One => "one",
            Self::All => "all",
        }
    }

    fn as_spotify_api_state(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::One => "track",
            Self::All => "context",
        }
    }
}

struct BridgeState {
    client_id: Option<String>,
    engine: Option<SharedEngine>,
    playback_access_token: Option<String>,
    playback_queue: Vec<String>,
    playback_index: usize,
    playback_shuffle: bool,
    playback_repeat_mode: RepeatModeState,
    playback_volume_percent: u8,
    playback_is_playing: bool,
    playback_progress_ms: u64,
    playback_current_track_id: Option<String>,
}

impl Default for BridgeState {
    fn default() -> Self {
        Self {
            client_id: None,
            engine: None,
            playback_access_token: None,
            playback_queue: Vec::new(),
            playback_index: 0,
            playback_shuffle: false,
            playback_repeat_mode: RepeatModeState::Off,
            playback_volume_percent: 100,
            playback_is_playing: false,
            playback_progress_ms: 0,
            playback_current_track_id: None,
        }
    }
}

static BRIDGE_STATE: Lazy<Mutex<BridgeState>> = Lazy::new(|| Mutex::new(BridgeState::default()));
static TOKIO_RUNTIME: Lazy<Runtime> = Lazy::new(|| {
    Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("failed to initialize tokio runtime for android ffi bridge")
});
static HTTP_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .build()
        .expect("spotify http client")
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
    #[serde(default)]
    device_id: Option<String>,
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

fn require_engine() -> Result<SharedEngine, String> {
    let state = lock_state()?;
    state
        .engine
        .clone()
        .ok_or_else(|| "Bridge is not initialized. Call init(config_json) first.".to_string())
}

fn normalize_spotify_track_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let normalized = if let Some(stripped) = trimmed.strip_prefix("spotify:track:") {
        stripped
    } else if let Some((_, rest)) = trimmed.split_once("/track/") {
        rest.split(['?', '/']).next().unwrap_or(rest)
    } else {
        trimmed
    };

    let normalized = normalized.trim();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized.to_string())
    }
}

fn truncate_for_error(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn extract_spotify_error_message(raw_body: &str) -> Option<String> {
    if raw_body.trim().is_empty() {
        return None;
    }

    let parsed = serde_json::from_str::<Value>(raw_body).ok()?;
    let error = parsed.get("error")?;

    if let Some(message) = error.get("message").and_then(Value::as_str) {
        return Some(message.to_string());
    }

    error.as_str().map(str::to_string)
}

async fn execute_spotify_request(
    method: Method,
    path: &str,
    token: &str,
    query: Vec<(String, String)>,
    body: Option<Value>,
) -> Result<Option<Value>, String> {
    let normalized_token = token.trim();
    if normalized_token.is_empty() {
        return Err("Spotify access token is required".to_string());
    }

    let endpoint = path.trim_start_matches('/');
    let url = format!("https://api.spotify.com/v1/{}", endpoint);

    let mut request = HTTP_CLIENT
        .request(method, &url)
        .bearer_auth(normalized_token)
        .header(reqwest::header::ACCEPT, "application/json");

    if !query.is_empty() {
        request = request.query(&query);
    }

    if let Some(payload) = body {
        request = request.json(&payload);
    }

    let response = request
        .send()
        .await
        .map_err(|error| format!("Spotify API request failed: {}", error))?;

    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();

    if status.is_success() {
        if body_text.trim().is_empty() {
            return Ok(None);
        }

        return serde_json::from_str::<Value>(&body_text)
            .map(Some)
            .map_err(|error| format!("Failed to parse Spotify API JSON response: {}", error));
    }

    let detail = extract_spotify_error_message(&body_text)
        .unwrap_or_else(|| truncate_for_error(&body_text, 220));

    if detail.is_empty() {
        Err(format!(
            "Spotify API request failed (HTTP {})",
            status.as_u16()
        ))
    } else {
        Err(format!(
            "Spotify API request failed (HTTP {}): {}",
            status.as_u16(),
            detail
        ))
    }
}

async fn resolve_spotify_device_id(token: &str) -> Result<Option<String>, String> {
    let payload =
        execute_spotify_request(Method::GET, "me/player/devices", token, Vec::new(), None).await?;

    let Some(payload) = payload else {
        return Ok(None);
    };

    let Some(devices) = payload.get("devices").and_then(Value::as_array) else {
        return Ok(None);
    };

    if devices.is_empty() {
        return Ok(None);
    }

    let preferred = devices
        .iter()
        .find(|device| {
            device
                .get("is_active")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .or_else(|| {
            devices.iter().find(|device| {
                !device
                    .get("is_restricted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
        })
        .or_else(|| devices.first());

    Ok(preferred
        .and_then(|device| device.get("id").and_then(Value::as_str))
        .map(str::to_string))
}

fn require_playback_token() -> Result<String, String> {
    let state = lock_state()?;
    state
        .playback_access_token
        .clone()
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| {
            "Playback token is missing. Start playback with spotifyStartQueue(...) first."
                .to_string()
        })
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
    let uris: Vec<String> = normalized_track_ids
        .iter()
        .map(|track_id| format!("spotify:track:{}", track_id))
        .collect();

    let requested_device_id = payload
        .device_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let resolved_device_id = match requested_device_id {
        Some(value) => Some(value),
        None => match TOKIO_RUNTIME.block_on(resolve_spotify_device_id(token)) {
            Ok(device_id) => device_id,
            Err(error) => return error_response("spotify_device_lookup_failed", error),
        },
    };

    let mut query = Vec::new();
    if let Some(device_id) = resolved_device_id.as_ref() {
        query.push(("device_id".to_string(), device_id.clone()));
    }

    let body = json!({
        "uris": uris,
        "offset": {
            "position": start_index
        }
    });

    if let Err(error) = TOKIO_RUNTIME.block_on(execute_spotify_request(
        Method::PUT,
        "me/player/play",
        token,
        query,
        Some(body),
    )) {
        let message = if error.contains("HTTP 404") {
            "No active Spotify device found. Open Spotify on a device and retry.".to_string()
        } else {
            error
        };
        return error_response("spotify_start_queue_failed", message);
    }

    let mut state = match lock_state() {
        Ok(state) => state,
        Err(error) => return error_response("bridge_state_error", error),
    };
    state.playback_access_token = Some(token.to_string());
    state.playback_queue = normalized_track_ids;
    state.playback_index = start_index;
    state.playback_is_playing = true;
    state.playback_progress_ms = 0;
    state.playback_current_track_id = state.playback_queue.get(start_index).cloned();

    success_response(json!({
        "started": true,
        "device_id": resolved_device_id,
        "track_count": state.playback_queue.len(),
        "start_index": state.playback_index
    }))
}

fn handle_spotify_play() -> String {
    let token = match require_playback_token() {
        Ok(token) => token,
        Err(error) => return error_response("spotify_playback_token_missing", error),
    };

    if let Err(error) = TOKIO_RUNTIME.block_on(execute_spotify_request(
        Method::PUT,
        "me/player/play",
        &token,
        Vec::new(),
        Some(json!({})),
    )) {
        return error_response("spotify_playback_command_failed", error);
    }

    if let Ok(mut state) = lock_state() {
        state.playback_is_playing = true;
    }

    success_response(json!({ "playing": true }))
}

fn handle_spotify_pause() -> String {
    let token = match require_playback_token() {
        Ok(token) => token,
        Err(error) => return error_response("spotify_playback_token_missing", error),
    };

    if let Err(error) = TOKIO_RUNTIME.block_on(execute_spotify_request(
        Method::PUT,
        "me/player/pause",
        &token,
        Vec::new(),
        Some(json!({})),
    )) {
        return error_response("spotify_playback_command_failed", error);
    }

    if let Ok(mut state) = lock_state() {
        state.playback_is_playing = false;
    }

    success_response(json!({ "playing": false }))
}

fn handle_spotify_next() -> String {
    let token = match require_playback_token() {
        Ok(token) => token,
        Err(error) => return error_response("spotify_playback_token_missing", error),
    };

    if let Err(error) = TOKIO_RUNTIME.block_on(execute_spotify_request(
        Method::POST,
        "me/player/next",
        &token,
        Vec::new(),
        None,
    )) {
        return error_response("spotify_playback_command_failed", error);
    }

    if let Ok(mut state) = lock_state() {
        if !state.playback_queue.is_empty() {
            if state.playback_index + 1 < state.playback_queue.len() {
                state.playback_index += 1;
            } else if state.playback_repeat_mode == RepeatModeState::All {
                state.playback_index = 0;
            }
            state.playback_current_track_id =
                state.playback_queue.get(state.playback_index).cloned();
            state.playback_progress_ms = 0;
        }
        state.playback_is_playing = true;
    }

    success_response(json!({ "ok": true }))
}

fn handle_spotify_previous() -> String {
    let token = match require_playback_token() {
        Ok(token) => token,
        Err(error) => return error_response("spotify_playback_token_missing", error),
    };

    if let Err(error) = TOKIO_RUNTIME.block_on(execute_spotify_request(
        Method::POST,
        "me/player/previous",
        &token,
        Vec::new(),
        None,
    )) {
        return error_response("spotify_playback_command_failed", error);
    }

    if let Ok(mut state) = lock_state() {
        if !state.playback_queue.is_empty() {
            if state.playback_index > 0 {
                state.playback_index -= 1;
            } else if state.playback_repeat_mode == RepeatModeState::All {
                state.playback_index = state.playback_queue.len().saturating_sub(1);
            }
            state.playback_current_track_id =
                state.playback_queue.get(state.playback_index).cloned();
            state.playback_progress_ms = 0;
        }
        state.playback_is_playing = true;
    }

    success_response(json!({ "ok": true }))
}

fn handle_spotify_seek(config_json: &str) -> String {
    let payload = match parse_required_json::<SeekPayload>(config_json, "spotify_seek") {
        Ok(payload) => payload,
        Err(error) => return error_response("invalid_seek_payload", error),
    };

    let token = match require_playback_token() {
        Ok(token) => token,
        Err(error) => return error_response("spotify_playback_token_missing", error),
    };

    if let Err(error) = TOKIO_RUNTIME.block_on(execute_spotify_request(
        Method::PUT,
        "me/player/seek",
        &token,
        vec![("position_ms".to_string(), payload.position_ms.to_string())],
        Some(json!({})),
    )) {
        return error_response("spotify_playback_command_failed", error);
    }

    if let Ok(mut state) = lock_state() {
        state.playback_progress_ms = payload.position_ms;
    }

    success_response(json!({ "position_ms": payload.position_ms }))
}

fn handle_spotify_set_volume(config_json: &str) -> String {
    let payload = match parse_required_json::<VolumePayload>(config_json, "spotify_set_volume") {
        Ok(payload) => payload,
        Err(error) => return error_response("invalid_set_volume_payload", error),
    };

    let volume_percent = payload.volume_percent.clamp(0, 100) as u8;
    let token = match require_playback_token() {
        Ok(token) => token,
        Err(error) => return error_response("spotify_playback_token_missing", error),
    };

    if let Err(error) = TOKIO_RUNTIME.block_on(execute_spotify_request(
        Method::PUT,
        "me/player/volume",
        &token,
        vec![("volume_percent".to_string(), volume_percent.to_string())],
        Some(json!({})),
    )) {
        return error_response("spotify_playback_command_failed", error);
    }

    if let Ok(mut state) = lock_state() {
        state.playback_volume_percent = volume_percent;
    }

    success_response(json!({ "volume_percent": volume_percent }))
}

fn handle_spotify_set_shuffle(config_json: &str) -> String {
    let payload = match parse_required_json::<ShufflePayload>(config_json, "spotify_set_shuffle") {
        Ok(payload) => payload,
        Err(error) => return error_response("invalid_set_shuffle_payload", error),
    };

    let token = match require_playback_token() {
        Ok(token) => token,
        Err(error) => return error_response("spotify_playback_token_missing", error),
    };

    if let Err(error) = TOKIO_RUNTIME.block_on(execute_spotify_request(
        Method::PUT,
        "me/player/shuffle",
        &token,
        vec![("state".to_string(), payload.enabled.to_string())],
        Some(json!({})),
    )) {
        return error_response("spotify_playback_command_failed", error);
    }

    if let Ok(mut state) = lock_state() {
        state.playback_shuffle = payload.enabled;
    }

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

    let token = match require_playback_token() {
        Ok(token) => token,
        Err(error) => return error_response("spotify_playback_token_missing", error),
    };

    if let Err(error) = TOKIO_RUNTIME.block_on(execute_spotify_request(
        Method::PUT,
        "me/player/repeat",
        &token,
        vec![("state".to_string(), mode.as_spotify_api_state().to_string())],
        Some(json!({})),
    )) {
        return error_response("spotify_playback_command_failed", error);
    }

    if let Ok(mut state) = lock_state() {
        state.playback_repeat_mode = mode;
    }

    success_response(json!({ "repeat_mode": mode.as_snapshot_str() }))
}

fn handle_spotify_snapshot() -> String {
    let token = match require_playback_token() {
        Ok(token) => token,
        Err(error) => return error_response("spotify_playback_token_missing", error),
    };

    let request_result = TOKIO_RUNTIME.block_on(execute_spotify_request(
        Method::GET,
        "me/player",
        &token,
        Vec::new(),
        None,
    ));

    match request_result {
        Ok(Some(payload)) => {
            let is_playing = payload
                .get("is_playing")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let progress_ms = payload
                .get("progress_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let volume_percent = payload
                .get("device")
                .and_then(|device| device.get("volume_percent"))
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .min(100) as u8;
            let shuffle_enabled = payload
                .get("shuffle_state")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let repeat_mode = RepeatModeState::from_spotify_api(
                payload
                    .get("repeat_state")
                    .and_then(Value::as_str)
                    .unwrap_or("off"),
            );
            let current_track_id = payload
                .get("item")
                .and_then(|item| item.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);

            if let Ok(mut state) = lock_state() {
                state.playback_is_playing = is_playing;
                state.playback_progress_ms = progress_ms;
                state.playback_volume_percent = volume_percent;
                state.playback_shuffle = shuffle_enabled;
                state.playback_repeat_mode = repeat_mode;
                state.playback_current_track_id = current_track_id.clone();

                if let Some(track_id) = current_track_id.as_ref() {
                    if let Some(index) = state
                        .playback_queue
                        .iter()
                        .position(|entry| entry == track_id)
                    {
                        state.playback_index = index;
                    }
                }
            }

            success_response(json!({
                "is_playing": is_playing,
                "progress_ms": progress_ms,
                "volume_percent": volume_percent,
                "shuffle_enabled": shuffle_enabled,
                "repeat_mode": repeat_mode.as_snapshot_str(),
                "current_track_id": current_track_id
            }))
        }
        Ok(None) => {
            let state = match lock_state() {
                Ok(state) => state,
                Err(error) => return error_response("bridge_state_error", error),
            };

            success_response(json!({
                "is_playing": state.playback_is_playing,
                "progress_ms": state.playback_progress_ms,
                "volume_percent": state.playback_volume_percent,
                "shuffle_enabled": state.playback_shuffle,
                "repeat_mode": state.playback_repeat_mode.as_snapshot_str(),
                "current_track_id": state.playback_current_track_id
            }))
        }
        Err(error) => error_response("spotify_playback_state_failed", error),
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
    fn spotify_snapshot_requires_playback_token() {
        let _guard = TEST_MUTEX.lock().expect("test mutex");

        {
            let mut state = BRIDGE_STATE.lock().expect("state lock");
            state.playback_access_token = None;
        }

        let payload = parse_json(&handle_spotify_snapshot());
        assert_eq!(payload["ok"], Value::Bool(false));
        assert_eq!(
            payload["error"]["code"],
            Value::String("spotify_playback_token_missing".to_string())
        );
    }
}
