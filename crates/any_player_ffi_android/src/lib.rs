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
use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::jstring;
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::runtime::{Builder, Runtime};
use tokio::time::{Duration, timeout};
use url::form_urlencoded;

struct BridgeState {
    audio_normalization: AudioNormalizationSettings,
    adaptive_normalization: AdaptiveNormalizationState,
}

impl Default for BridgeState {
    fn default() -> Self {
        Self {
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
static TOKIO_RUNTIME: Lazy<Runtime> = Lazy::new(|| {
    Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to initialize tokio runtime for android ffi bridge")
});
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
            "user-modify-playback-state".to_string(),
            "user-read-playback-state".to_string(),
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

    {
        let mut state = match lock_state() {
            Ok(state) => state,
            Err(error) => return error_response("bridge_state_error", error),
        };

        state.audio_normalization.enabled = payload.enabled;
        state.audio_normalization.strict_mode = payload.strict_mode;
        state.audio_normalization.target = INTERNAL_NORMALIZATION_TARGET;
    }

    success_response(json!({
        "enabled": payload.enabled,
        "strict_mode": payload.strict_mode,
        "target": INTERNAL_NORMALIZATION_TARGET,
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

/// Called by the JVM when our shared library is first loaded to initialize
/// `android_logger` for Rust logs in logcat under the `any_player_rust` tag.
///
/// # Safety
/// The JVM must invoke this function with a valid, non-null `JavaVM*` that
/// remains valid for the lifetime of the process, as guaranteed by the JNI
/// specification for `JNI_OnLoad`.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn JNI_OnLoad(
    _vm: *mut jni::sys::JavaVM,
    _reserved: *mut std::ffi::c_void,
) -> jni::sys::jint {
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("any_player_rust")
            .with_max_level(log::LevelFilter::Debug),
    );
    log::info!("any_player_rust JNI_OnLoad: logger initialised");

    jni::sys::JNI_VERSION_1_6
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
    fn set_audio_normalization_settings_updates_state_without_stale_volume_metadata() {
        let _guard = TEST_MUTEX.lock().expect("test mutex");

        {
            let mut state = lock_state().expect("lock_state");
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
        assert!(payload["data"].get("volume_percent").is_none());
        assert!(payload["data"].get("output_volume_percent").is_none());

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
