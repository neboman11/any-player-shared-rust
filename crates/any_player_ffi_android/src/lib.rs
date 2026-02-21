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
    playback_shuffle: bool,
    playback_repeat_mode: RepeatModeState,
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

    // Disconnect any existing player first so startQueue always gets a clean
    // slate. This also lets the connect() idempotency guard not block us here:
    // concurrent ensure_player_connected() callers will wait, see is_connected()
    // = true once our connect() finishes, and skip reconnection.
    TOKIO_RUNTIME.block_on(PLAYER.disconnect());

    // Connect librespot (establishes a real Spotify session on this device).
    if let Err(error) = TOKIO_RUNTIME.block_on(PLAYER.connect(&token_owned, &client_id_owned)) {
        return error_response("librespot_connect_failed", error.message);
    }

    // Start decoding + playing audio directly via rodio — no Spotify Connect device needed.
    if let Err(error) =
        TOKIO_RUNTIME.block_on(PLAYER.start_queue(&normalized_track_ids, start_index))
    {
        return error_response("librespot_start_queue_failed", error.message);
    }

    let mut state = match lock_state() {
        Ok(state) => state,
        Err(error) => return error_response("bridge_state_error", error),
    };
    // Persist the token so later transport commands can reconnect transparently.
    state.playback_access_token = Some(token_owned);
    state.playback_queue = normalized_track_ids;
    state.playback_index = start_index;

    success_response(json!({
        "started": true,
        "track_count": state.playback_queue.len(),
        "start_index": state.playback_index
    }))
}

/// Ensures the librespot player is connected, reconnecting from the stored token
/// if necessary. Also re-queues the stored track list after reconnect so that
/// transport commands (play, seek, etc.) have a loaded track to act on.
/// Returns an error string if the connection cannot be established.
fn ensure_player_connected() -> Result<(), String> {
    if TOKIO_RUNTIME.block_on(PLAYER.is_connected()) {
        return Ok(());
    }

    let (token, client_id, queue, index) = {
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
    }

    Ok(())
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

    let next_index = if state.playback_repeat_mode == RepeatModeState::One {
        // Replay the current track.
        state.playback_index
    } else if state.playback_index + 1 < state.playback_queue.len() {
        state.playback_index + 1
    } else if state.playback_repeat_mode == RepeatModeState::All {
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

    let track_ids = state.playback_queue.clone();
    state.playback_index = prev_index;
    drop(state);

    if let Err(msg) = ensure_player_connected() {
        return error_response("spotify_playback_command_failed", msg);
    }
    if let Err(error) = TOKIO_RUNTIME.block_on(PLAYER.start_queue(&track_ids, prev_index)) {
        return error_response("librespot_previous_failed", error.message);
    }

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

    let volume_percent = payload.volume_percent.clamp(0, 100) as u8;
    run_connected_player_command(json!({ "volume_percent": volume_percent }), || {
        TOKIO_RUNTIME.block_on(PLAYER.set_volume(volume_percent))
    })
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
        "end_of_track": snapshot.end_of_track,
        "volume_percent": snapshot.volume_percent,
        "shuffle_enabled": state.playback_shuffle,
        "repeat_mode": state.playback_repeat_mode.as_snapshot_str(),
        "current_track_id": current_track_id
    }))
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
    fn spotify_snapshot_returns_idle_state_when_not_playing() {
        let _guard = TEST_MUTEX.lock().expect("test mutex");

        // Without a librespot connection the snapshot should return a valid
        // ok response with is_playing=false (the player default).
        let payload = parse_json(&handle_spotify_snapshot());
        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(payload["data"]["is_playing"], Value::Bool(false));
    }
}
