use librespot_core::{
    authentication::Credentials, config::SessionConfig, session::Session, spotify_uri::SpotifyUri,
};
use librespot_playback::{
    audio_backend,
    config::{AudioFormat, PlayerConfig, VolumeCtrl},
    mixer::{self, Mixer, MixerConfig},
    player::{Player, PlayerEvent},
};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;

use crate::SpotifyEngineError;

/// Live playback statistics updated by the librespot event loop.
/// Shared between `PlayerState` and the background event task via `Arc`.
#[derive(Debug, Default)]
struct PlaybackStats {
    is_playing: bool,
    /// Position at the last Playing/seek/start_queue event.
    progress_ms: u64,
    /// Wall-clock instant when the current playing stretch began (i.e. when
    /// `is_playing` last became true). Used by `snapshot()` to compute the
    /// live position as `progress_ms + elapsed_since_play_started`. Reset to
    /// `None` whenever playback stops or is paused.
    play_started_at: Option<std::time::Instant>,
    current_track_id: Option<String>,
    /// Monotonically increasing counter incremented by each `EndOfTrack` event.
    /// Kotlin reads the counter value via `snapshot()` and acknowledges it via
    /// `acknowledge_end_of_track(n)` so the signal is never silently lost.
    end_of_track_count: u64,
    /// Full Spotify URI of the next track to preload. Set when a preload is
    /// requested; read by the `TimeToPreloadNextTrack` event handler to
    /// re-trigger if the earlier pre-fetch window was missed.
    next_preload_uri: Option<String>,
    /// Set the instant `play()`/`start_queue()` optimistically marks playback
    /// as active; cleared the moment any real `PlayerEvent` arrives (proof the
    /// event loop — and therefore the underlying session — is still alive).
    /// If this stays set past `PLAYING_CONFIRMATION_TIMEOUT`, the session's
    /// socket likely died silently in the background (Doze/network loss)
    /// while librespot's `Session`/`Player` objects remained technically
    /// valid, so `is_healthy()`/`snapshot()` treat it as dead. Without this,
    /// such a session reports healthy and "playing" forever with no audio,
    /// and previously required a force-stop to recover.
    awaiting_playing_confirmation_since: Option<std::time::Instant>,
}

/// How long to wait for a real `PlayerEvent` to confirm a `play()`/`start_queue()`
/// call before treating the session as silently stalled. Generous enough to
/// cover legitimate slow network starts.
const PLAYING_CONFIRMATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// The state held by a running librespot player instance.
struct PlayerState {
    session: Session,
    player: Arc<Player>,
    mixer: Arc<dyn Mixer>,
    volume_percent: u8,
    /// Shared with the background event loop so progress/play-state stay current.
    stats: Arc<StdMutex<PlaybackStats>>,
}

pub struct LibrespotPlayer {
    inner: Mutex<Option<PlayerState>>,
    connecting: Mutex<()>,
}

impl LibrespotPlayer {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
            connecting: Mutex::new(()),
        }
    }

    /// Connect to Spotify using an OAuth access token and start a librespot session.
    /// This must be called before any playback commands.
    ///
    /// `client_id` must be the **same** OAuth client ID that was used to obtain
    /// the `access_token`. librespot's Login5 service binds stored credentials
    /// to the client ID that originally authenticated them; passing a mismatched
    /// client ID causes `INVALID_CREDENTIALS` when loading tracks.
    ///
    /// Concurrent callers are serialised: only the first proceeds; subsequent
    /// callers return immediately once the first succeeds (idempotent when
    /// already connected). To force a *new* connection (e.g. for a fresh queue)
    /// call `disconnect()` first, then `connect()`.
    pub async fn connect(
        &self,
        access_token: &str,
        client_id: &str,
    ) -> Result<(), SpotifyEngineError> {
        // Serialise concurrent connect() attempts.
        let _lock = self.connecting.lock().await;

        // If a previous concurrent caller already finished connecting, skip.
        if self.inner.lock().await.is_some() {
            log::debug!("librespot: connect() skipped — already connected");
            return Ok(());
        }
        let token = access_token.trim();
        if token.is_empty() {
            return Err(SpotifyEngineError::new(
                "spotify_access_token_missing",
                "Access token is required to connect",
            ));
        }
        let cid = client_id.trim();
        if cid.is_empty() {
            return Err(SpotifyEngineError::new(
                "spotify_client_id_missing",
                "Client ID is required to connect",
            ));
        }

        // Use default session config (KEYMASTER_CLIENT_ID via OS="linux" in
        // config.rs). This is required for the clienttoken.spotify.com
        // request to succeed; Login5 itself is bypassed below via
        // inject_bearer_token since it rejects this app's third-party OAuth
        // client_id under every method (StoredCredential, OneTimeToken).
        let session_config = SessionConfig::default();

        let credentials = Credentials::with_access_token(token);

        let session = Session::new(session_config, None);
        session
            .connect(credentials, false)
            .await
            .map_err(|e| SpotifyEngineError::new("librespot_connect_failed", e.to_string()))?;

        // Login5 (login5.spotify.com) rejects this app's third-party OAuth
        // client_id outright for both StoredCredential and OneTimeToken
        // exchange (confirmed via BAD_REQUEST/INVALID_CREDENTIALS testing).
        // Bypass Login5 entirely and use the OAuth access token directly as
        // the spclient bearer token.
        session.login5().inject_bearer_token(token, 0);

        log::debug!("librespot: session connected (bearer token injected, Login5 bypassed)");

        let player_config = PlayerConfig::default();
        // F32 has broader device compatibility than the S16 default; this is
        // important on Android where some AAudio configurations reject I16.
        let audio_format = AudioFormat::F32;

        let backend = audio_backend::find(None).ok_or_else(|| {
            SpotifyEngineError::new(
                "librespot_backend_not_found",
                "No audio backend available (rodio backend expected)",
            )
        })?;

        let mixer_config = MixerConfig {
            volume_ctrl: VolumeCtrl::Linear,
            ..Default::default()
        };
        let mixer = mixer::find(None).ok_or_else(|| {
            SpotifyEngineError::new("librespot_mixer_not_found", "No audio mixer available")
        })?(mixer_config)
        .map_err(|e| SpotifyEngineError::new("librespot_mixer_init_failed", e.to_string()))?;

        let player = Player::new(
            player_config,
            session.clone(),
            mixer.get_soft_volume(),
            move || {
                // Wrap the rodio/cpal backend initialisation with a panic guard.
                // The backend calls `.unwrap()` internally; if audio-device setup
                // fails (common on emulators or when AAudio is unavailable) it
                // panics, which would silently kill the player OS-thread.
                // Catching the panic here lets us log the error clearly.
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    backend(None, audio_format)
                })) {
                    Ok(sink) => {
                        log::debug!("librespot: audio sink initialised successfully");
                        sink
                    }
                    Err(e) => {
                        let msg = e
                            .downcast_ref::<String>()
                            .map(|s| s.as_str())
                            .or_else(|| e.downcast_ref::<&str>().copied())
                            .unwrap_or("unknown panic payload");
                        log::error!(
                            "librespot: audio backend panicked during init: {}. \
                             Check that the device has a usable audio output.",
                            msg
                        );
                        // Propagate the panic so the player thread exits cleanly
                        // and the caller sees an error rather than a zombie player.
                        std::panic::resume_unwind(e);
                    }
                }
            },
        );

        // Give the player's OS-thread a brief moment to initialise the audio
        // sink. If the thread has already exited by then, the sink panicked and
        // we should surface the error immediately rather than storing a zombie.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if player.is_invalid() {
            return Err(SpotifyEngineError::new(
                "librespot_audio_backend_failed",
                "Audio backend failed to initialise (see logs). \
                 The player thread exited immediately after creation.",
            ));
        }

        let mut event_channel = player.get_player_event_channel();

        // Shared stats updated by the background event loop below.
        let stats: Arc<StdMutex<PlaybackStats>> = Arc::new(StdMutex::new(PlaybackStats::default()));
        let stats_ref = Arc::clone(&stats);
        let player_ref = Arc::downgrade(&player);

        // Drive the player event channel so it never fills up, and keep
        // playback stats in sync with what librespot actually reports.
        tokio::spawn(async move {
            while let Some(event) = event_channel.recv().await {
                // Any real event proves the event loop (and the session behind
                // it) is still alive, regardless of which event it is.
                if let Ok(mut s) = stats_ref.lock() {
                    s.awaiting_playing_confirmation_since = None;
                }
                match event {
                    PlayerEvent::Playing {
                        position_ms,
                        track_id,
                        ..
                    } => {
                        log::info!("librespot: playing track={} at {}ms", track_id, position_ms);
                        if let Ok(mut s) = stats_ref.lock() {
                            s.is_playing = true;
                            s.progress_ms = position_ms as u64;
                            s.current_track_id = Some(track_id.to_string());
                            // Record the wall-clock start so snapshot() can
                            // compute the live position without librespot ticks.
                            s.play_started_at = Some(std::time::Instant::now());
                        }
                    }
                    PlayerEvent::Paused {
                        position_ms,
                        track_id,
                        ..
                    } => {
                        log::info!("librespot: paused track={} at {}ms", track_id, position_ms);
                        if let Ok(mut s) = stats_ref.lock() {
                            s.is_playing = false;
                            s.progress_ms = position_ms as u64;
                            s.current_track_id = Some(track_id.to_string());
                            s.play_started_at = None;
                        }
                    }
                    PlayerEvent::Stopped { track_id, .. } => {
                        log::info!("librespot: stopped track={}", track_id);
                        if let Ok(mut s) = stats_ref.lock() {
                            s.is_playing = false;
                            s.current_track_id = Some(track_id.to_string());
                            s.play_started_at = None;
                        }
                    }
                    PlayerEvent::Loading {
                        track_id,
                        position_ms,
                        ..
                    } => {
                        log::info!("librespot: loading track={} at {}ms", track_id, position_ms);
                        if let Ok(mut s) = stats_ref.lock() {
                            s.progress_ms = position_ms as u64;
                            s.current_track_id = Some(track_id.to_string());
                        }
                    }
                    PlayerEvent::EndOfTrack { track_id, .. } => {
                        log::info!("librespot: end of track track={}", track_id);
                        if let Ok(mut s) = stats_ref.lock() {
                            s.is_playing = false;
                            s.current_track_id = Some(track_id.to_string());
                            s.play_started_at = None;
                            s.end_of_track_count += 1;
                        }
                    }
                    PlayerEvent::Unavailable { track_id, .. } => {
                        log::warn!("librespot: track unavailable track={}", track_id);
                        if let Ok(mut s) = stats_ref.lock() {
                            s.is_playing = false;
                            s.current_track_id = Some(track_id.to_string());
                            s.play_started_at = None;
                        }
                    }
                    PlayerEvent::VolumeChanged { volume } => {
                        log::debug!("librespot: volume changed to {}", volume);
                    }
                    PlayerEvent::TimeToPreloadNextTrack { .. } => {
                        // Re-trigger preload when librespot signals the window is open.
                        // This handles the case where our proactive preload was too early
                        // and the buffered data was evicted before playback reached it.
                        let next_uri = stats_ref
                            .lock()
                            .ok()
                            .and_then(|s| s.next_preload_uri.clone());
                        if let Some(uri_str) = next_uri
                            && let Ok(uri) = SpotifyUri::from_uri(&uri_str)
                        {
                            log::debug!(
                                "librespot: TimeToPreloadNextTrack — re-preloading {}",
                                uri_str
                            );
                            if let Some(player) = player_ref.upgrade() {
                                player.preload(uri);
                            }
                        }
                    }
                    other => {
                        log::debug!("librespot: event {:?}", other);
                    }
                }
            }
            log::info!("librespot: event channel closed");
        });

        let mut guard = self.inner.lock().await;
        *guard = Some(PlayerState {
            session,
            player,
            mixer,
            volume_percent: 100,
            stats,
        });

        Ok(())
    }

    /// Returns true if there is an active connected session whose underlying
    /// librespot `Session` and `Player` thread are both still alive.
    pub async fn is_healthy(&self) -> bool {
        let guard = self.inner.lock().await;
        match guard.as_ref() {
            None => false,
            Some(state) => {
                !state.session.is_invalid()
                    && !state.player.is_invalid()
                    && !Self::playback_confirmation_stalled(&state.stats)
            }
        }
    }

    /// True when a `play()`/`start_queue()` call has gone unconfirmed by any
    /// real `PlayerEvent` for longer than `PLAYING_CONFIRMATION_TIMEOUT` —
    /// the signature of a session whose socket died silently in the background.
    fn playback_confirmation_stalled(stats: &Arc<StdMutex<PlaybackStats>>) -> bool {
        match stats.lock() {
            Ok(s) => s
                .awaiting_playing_confirmation_since
                .is_some_and(|since| since.elapsed() > PLAYING_CONFIRMATION_TIMEOUT),
            Err(_) => true,
        }
    }

    /// Returns true if there is an active connected session.
    pub async fn is_connected(&self) -> bool {
        self.inner.lock().await.is_some()
    }

    /// Load and immediately play a set of tracks beginning at `start_index`.
    pub async fn start_queue(
        &self,
        track_ids: &[String],
        start_index: usize,
    ) -> Result<(), SpotifyEngineError> {
        let mut guard = self.inner.lock().await;
        let state = guard.as_mut().ok_or_else(not_connected_error)?;

        if track_ids.is_empty() {
            return Err(SpotifyEngineError::new(
                "librespot_empty_queue",
                "Track list must not be empty",
            ));
        }

        let index = start_index.min(track_ids.len() - 1);
        let track_id = &track_ids[index];

        // Accept both bare base62 IDs ("4uLU6hMCjMI75M1A2tKUQC") and full URIs
        // ("spotify:track:4uLU6hMCjMI75M1A2tKUQC").
        let uri_str = if track_id.starts_with("spotify:") {
            track_id.clone()
        } else {
            format!("spotify:track:{}", track_id)
        };

        let spotify_uri = SpotifyUri::from_uri(&uri_str).map_err(|_| {
            SpotifyEngineError::new(
                "librespot_invalid_track_id",
                format!("Invalid Spotify track URI: {}", uri_str),
            )
        })?;

        state.player.load(spotify_uri, true, 0);
        if let Ok(mut s) = state.stats.lock() {
            s.is_playing = true;
            s.progress_ms = 0;
            s.play_started_at = Some(std::time::Instant::now());
            s.current_track_id = Some(track_id.clone());
            s.awaiting_playing_confirmation_since = Some(std::time::Instant::now());
        }

        Ok(())
    }

    /// Pre-fetch and decode the given track without starting playback.
    /// When the current track is about to end and `load()` is called for this
    /// track, librespot reuses the pre-fetched decoder data for a gapless transition.
    /// Safe to call multiple times; idempotent if the same track is already preloading.
    pub async fn preload(&self, track_id: &str) -> Result<(), SpotifyEngineError> {
        let mut guard = self.inner.lock().await;
        let state = guard.as_mut().ok_or_else(not_connected_error)?;

        let uri_str = if track_id.starts_with("spotify:") {
            track_id.to_string()
        } else {
            format!("spotify:track:{}", track_id)
        };

        let spotify_uri = SpotifyUri::from_uri(&uri_str).map_err(|_| {
            SpotifyEngineError::new(
                "librespot_invalid_track_id",
                format!("Invalid Spotify track URI for preload: {}", uri_str),
            )
        })?;

        if let Ok(mut s) = state.stats.lock() {
            s.next_preload_uri = Some(uri_str.clone());
        }

        log::debug!("librespot: preloading {}", uri_str);
        state.player.preload(spotify_uri);
        Ok(())
    }

    pub async fn play(&self) -> Result<(), SpotifyEngineError> {
        let mut guard = self.inner.lock().await;
        let state = guard.as_mut().ok_or_else(not_connected_error)?;

        // `Player::play()` is only meaningful once a track has been `load()`ed
        // into it via `start_queue()`. A freshly (re)connected session — e.g.
        // right after re-authenticating — has a brand-new `Player` with
        // nothing loaded, so calling `play()` here would be a silent no-op:
        // no audio ever starts, yet we'd report success and skip the
        // reload-queue fallback callers rely on. Error out instead so callers
        // fall back to `start_queue()`.
        let has_loaded_track = state
            .stats
            .lock()
            .map(|s| s.current_track_id.is_some())
            .unwrap_or(false);
        if !has_loaded_track {
            return Err(SpotifyEngineError::new(
                "librespot_no_track_loaded",
                "No track has been loaded yet; call start_queue() instead",
            ));
        }

        state.player.play();
        // Optimistic update; PlayerEvent::Playing will confirm the real position.
        if let Ok(mut s) = state.stats.lock() {
            s.is_playing = true;
            s.play_started_at = Some(std::time::Instant::now());
            s.awaiting_playing_confirmation_since = Some(std::time::Instant::now());
        }
        Ok(())
    }

    pub async fn pause(&self) -> Result<(), SpotifyEngineError> {
        let mut guard = self.inner.lock().await;
        let state = guard.as_mut().ok_or_else(not_connected_error)?;
        state.player.pause();
        if let Ok(mut s) = state.stats.lock() {
            s.is_playing = false;
            s.play_started_at = None;
        }
        Ok(())
    }

    pub async fn seek(&self, position_ms: u64) -> Result<(), SpotifyEngineError> {
        let mut guard = self.inner.lock().await;
        let state = guard.as_mut().ok_or_else(not_connected_error)?;
        state.player.seek(position_ms.min(u32::MAX as u64) as u32);
        if let Ok(mut s) = state.stats.lock() {
            s.progress_ms = position_ms;
            // Reset the wall-clock anchor for live-position computation.
            if s.is_playing {
                s.play_started_at = Some(std::time::Instant::now());
            }
        }
        Ok(())
    }

    pub async fn set_volume(&self, volume_percent: u8) -> Result<(), SpotifyEngineError> {
        let mut guard = self.inner.lock().await;
        let state = guard.as_mut().ok_or_else(not_connected_error)?;
        // librespot/mixer volume is 0–65535
        let librespot_volume = ((volume_percent.min(100) as u32 * 65535) / 100) as u16;
        state.mixer.set_volume(librespot_volume);
        state.volume_percent = volume_percent;
        Ok(())
    }

    /// Returns a point-in-time snapshot of playback state. `progress_ms` gives
    /// the *live* position: when playing, elapsed wall-clock time since the last
    /// `PlayerEvent::Playing` / seek / start_queue is added to the stored base
    /// position, so callers see a continuously advancing value without needing
    /// librespot to emit periodic tick events.
    pub async fn snapshot(&self) -> PlayerSnapshot {
        let guard = self.inner.lock().await;
        match guard.as_ref() {
            None => PlayerSnapshot::default(),
            Some(state) => {
                let (is_playing, progress_ms, end_of_track_count, current_track_id) = state
                    .stats
                    .lock()
                    .map(|s| {
                        // A play()/start_queue() that never got a real event
                        // confirming it is a session whose socket likely died
                        // silently in the background. Report it as not-playing
                        // so the existing "expected PLAYING but snapshot says
                        // paused" recovery path in Kotlin kicks in and forces
                        // a fresh reconnect, instead of masking the failure
                        // forever behind an optimistic flag.
                        let stalled = s
                            .awaiting_playing_confirmation_since
                            .is_some_and(|since| since.elapsed() > PLAYING_CONFIRMATION_TIMEOUT);
                        let effective_is_playing = s.is_playing && !stalled;
                        let live_ms = if effective_is_playing {
                            s.play_started_at
                                .map(|t| s.progress_ms + t.elapsed().as_millis() as u64)
                                .unwrap_or(s.progress_ms)
                        } else {
                            s.progress_ms
                        };
                        (
                            effective_is_playing,
                            live_ms,
                            s.end_of_track_count,
                            s.current_track_id.clone(),
                        )
                    })
                    .unwrap_or_default();
                PlayerSnapshot {
                    is_playing,
                    progress_ms,
                    end_of_track_count,
                    volume_percent: state.volume_percent,
                    current_track_id,
                }
            }
        }
    }

    pub async fn disconnect(&self) {
        let mut guard = self.inner.lock().await;
        if let Some(state) = guard.take() {
            drop(state.player);
            state.session.shutdown();
        }
    }
}

impl Default for LibrespotPlayer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default)]
pub struct PlayerSnapshot {
    pub is_playing: bool,
    pub progress_ms: u64,
    pub end_of_track_count: u64,
    pub volume_percent: u8,
    pub current_track_id: Option<String>,
}

fn not_connected_error() -> SpotifyEngineError {
    SpotifyEngineError::new(
        "librespot_not_connected",
        "Player is not connected. Call connect() first.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_confirmation_stalled_with_poisoned_lock() {
        let stats = Arc::new(StdMutex::new(PlaybackStats::default()));
        let poisoned_stats = Arc::clone(&stats);

        let _ = std::panic::catch_unwind(move || {
            let _guard = poisoned_stats.lock().unwrap();
            panic!("poison playback stats mutex");
        });

        assert!(LibrespotPlayer::playback_confirmation_stalled(&stats));
    }

    #[test]
    fn playback_confirmation_stalled_after_timeout() {
        let stats = Arc::new(StdMutex::new(PlaybackStats {
            awaiting_playing_confirmation_since: Some(
                std::time::Instant::now()
                    - PLAYING_CONFIRMATION_TIMEOUT
                    - std::time::Duration::from_millis(1),
            ),
            ..PlaybackStats::default()
        }));

        assert!(LibrespotPlayer::playback_confirmation_stalled(&stats));
    }
}
