use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// Shared Spotify session engine configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpotifySessionConfig {
    /// Spotify OAuth client ID owned by the platform shell.
    pub client_id: String,
}

impl SpotifySessionConfig {
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
        }
    }

    fn validate(&self) -> Result<(), SpotifyEngineError> {
        if self.client_id.trim().is_empty() {
            return Err(SpotifyEngineError::new(
                "spotify_client_id_missing",
                "Spotify client_id is required",
            ));
        }
        Ok(())
    }
}

/// Normalized Spotify token representation for platform/FFI boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpotifyToken {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Expiry as unix epoch seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_epoch_seconds: Option<u64>,
}

impl SpotifyToken {
    pub fn with_access_token(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            refresh_token: None,
            expires_at_epoch_seconds: None,
        }
    }

    pub fn is_expired_at(&self, now_epoch_seconds: u64) -> bool {
        self.expires_at_epoch_seconds
            .map(|expires_at| now_epoch_seconds >= expires_at)
            .unwrap_or(false)
    }
}

/// Runtime-agnostic session backend.
#[async_trait]
pub trait SpotifySessionBackend: Send + Sync {
    type SessionHandle: Clone + Send + Sync + 'static;

    async fn connect(
        &self,
        config: &SpotifySessionConfig,
        access_token: &str,
    ) -> Result<Self::SessionHandle, SpotifyEngineError>;

    async fn disconnect(&self, session: &Self::SessionHandle) -> Result<(), SpotifyEngineError>;
}

/// Platform-owned token refresh strategy.
#[async_trait]
pub trait SpotifyTokenRefresher: Send + Sync {
    async fn refresh(
        &self,
        refresh_token: &str,
        config: &SpotifySessionConfig,
    ) -> Result<SpotifyToken, SpotifyEngineError>;
}

/// Runtime state for the session engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpotifySessionState {
    Idle,
    Ready,
    Closed,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpotifyEngineError {
    pub code: String,
    pub message: String,
}

impl SpotifyEngineError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for SpotifyEngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for SpotifyEngineError {}

struct InnerState<H> {
    token: Option<SpotifyToken>,
    session: Option<H>,
    state: SpotifySessionState,
}

impl<H> Default for InnerState<H> {
    fn default() -> Self {
        Self {
            token: None,
            session: None,
            state: SpotifySessionState::Idle,
        }
    }
}

/// Shared Spotify session lifecycle engine.
///
/// Keeps session orchestration and token refresh handling in shared Rust,
/// while backend implementations remain platform-specific.
pub struct SpotifySessionEngine<B: SpotifySessionBackend> {
    config: SpotifySessionConfig,
    backend: B,
    token_refresher: Option<Arc<dyn SpotifyTokenRefresher>>,
    state: Mutex<InnerState<B::SessionHandle>>,
}

impl<B: SpotifySessionBackend> SpotifySessionEngine<B> {
    pub fn new(config: SpotifySessionConfig, backend: B) -> Self {
        Self {
            config,
            backend,
            token_refresher: None,
            state: Mutex::new(InnerState::default()),
        }
    }

    pub fn with_token_refresher(
        config: SpotifySessionConfig,
        backend: B,
        token_refresher: Arc<dyn SpotifyTokenRefresher>,
    ) -> Self {
        Self {
            config,
            backend,
            token_refresher: Some(token_refresher),
            state: Mutex::new(InnerState::default()),
        }
    }

    /// Initialize or reinitialize the backend session with a token.
    pub async fn initialize_with_token(
        &self,
        mut token: SpotifyToken,
    ) -> Result<(), SpotifyEngineError> {
        self.config.validate()?;
        token.access_token = token.access_token.trim().to_string();
        if token.access_token.is_empty() {
            return Err(SpotifyEngineError::new(
                "spotify_access_token_missing",
                "Spotify access_token is required",
            ));
        }

        let previous_session = {
            let mut state = self.state.lock().await;
            let previous = state.session.take();
            state.state = SpotifySessionState::Idle;
            previous
        };

        if let Some(previous) = previous_session {
            self.backend.disconnect(&previous).await?;
        }

        let session = self
            .backend
            .connect(&self.config, token.access_token.as_str())
            .await?;

        let mut state = self.state.lock().await;
        state.token = Some(token);
        state.session = Some(session);
        state.state = SpotifySessionState::Ready;
        Ok(())
    }

    /// Refresh token if expired and refresh support is configured.
    ///
    /// Returns `Ok(true)` if refresh occurred, `Ok(false)` when not needed.
    pub async fn refresh_if_needed(&self) -> Result<bool, SpotifyEngineError> {
        let now_epoch_seconds = current_epoch_seconds();
        let token = {
            let state = self.state.lock().await;
            state.token.clone().ok_or_else(|| {
                SpotifyEngineError::new(
                    "spotify_session_missing_token",
                    "Spotify token is not initialized",
                )
            })?
        };

        if !token.is_expired_at(now_epoch_seconds) {
            return Ok(false);
        }

        let refresh_token = token.refresh_token.clone().ok_or_else(|| {
            SpotifyEngineError::new(
                "spotify_refresh_token_missing",
                "Spotify token is expired and no refresh token is available",
            )
        })?;

        let refresher = self.token_refresher.clone().ok_or_else(|| {
            SpotifyEngineError::new(
                "spotify_refresher_missing",
                "Spotify token is expired but no token refresher is configured",
            )
        })?;

        let mut refreshed_token = refresher.refresh(&refresh_token, &self.config).await?;
        if refreshed_token.refresh_token.is_none() {
            refreshed_token.refresh_token = Some(refresh_token);
        }

        self.initialize_with_token(refreshed_token).await?;
        Ok(true)
    }

    pub async fn is_ready(&self) -> bool {
        let state = self.state.lock().await;
        state.state == SpotifySessionState::Ready && state.session.is_some()
    }

    pub async fn state(&self) -> SpotifySessionState {
        self.state.lock().await.state
    }

    pub async fn access_token(&self) -> Option<String> {
        self.state
            .lock()
            .await
            .token
            .as_ref()
            .map(|token| token.access_token.clone())
    }

    pub async fn session_handle(&self) -> Option<B::SessionHandle> {
        self.state.lock().await.session.clone()
    }

    /// Close the current session and clear token state.
    pub async fn close(&self) -> Result<(), SpotifyEngineError> {
        let session = {
            let mut state = self.state.lock().await;
            state.token = None;
            let session = state.session.take();
            state.state = SpotifySessionState::Closed;
            session
        };

        if let Some(session) = session {
            self.backend.disconnect(&session).await?;
        }

        Ok(())
    }
}

fn current_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct MockBackend {
        connect_calls: Arc<Mutex<Vec<String>>>,
        disconnect_calls: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl SpotifySessionBackend for MockBackend {
        type SessionHandle = String;

        async fn connect(
            &self,
            _config: &SpotifySessionConfig,
            access_token: &str,
        ) -> Result<Self::SessionHandle, SpotifyEngineError> {
            self.connect_calls
                .lock()
                .await
                .push(access_token.to_string());
            Ok(format!("session:{}", access_token))
        }

        async fn disconnect(
            &self,
            _session: &Self::SessionHandle,
        ) -> Result<(), SpotifyEngineError> {
            let mut calls = self.disconnect_calls.lock().await;
            *calls += 1;
            Ok(())
        }
    }

    #[derive(Clone)]
    struct MockRefresher {
        refreshed: SpotifyToken,
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl SpotifyTokenRefresher for MockRefresher {
        async fn refresh(
            &self,
            _refresh_token: &str,
            _config: &SpotifySessionConfig,
        ) -> Result<SpotifyToken, SpotifyEngineError> {
            let mut calls = self.calls.lock().await;
            *calls += 1;
            Ok(self.refreshed.clone())
        }
    }

    #[tokio::test]
    async fn initialize_sets_ready_state() {
        let backend = MockBackend::default();
        let engine = SpotifySessionEngine::new(SpotifySessionConfig::new("client"), backend);

        engine
            .initialize_with_token(SpotifyToken::with_access_token("token-1"))
            .await
            .expect("initialize should succeed");

        assert!(engine.is_ready().await);
        assert_eq!(engine.access_token().await.as_deref(), Some("token-1"));
    }

    #[tokio::test]
    async fn refresh_if_needed_is_noop_for_valid_token() {
        let backend = MockBackend::default();
        let engine =
            SpotifySessionEngine::new(SpotifySessionConfig::new("client"), backend.clone());
        let valid_until = current_epoch_seconds() + 3_600;

        engine
            .initialize_with_token(SpotifyToken {
                access_token: "valid-token".to_string(),
                refresh_token: Some("refresh".to_string()),
                expires_at_epoch_seconds: Some(valid_until),
            })
            .await
            .expect("initialize should succeed");

        let refreshed = engine
            .refresh_if_needed()
            .await
            .expect("refresh_if_needed should succeed");

        assert!(!refreshed);
        assert_eq!(backend.connect_calls.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn refresh_if_needed_reinitializes_with_new_token() {
        let backend = MockBackend::default();
        let refresher = Arc::new(MockRefresher {
            refreshed: SpotifyToken {
                access_token: "new-token".to_string(),
                refresh_token: Some("new-refresh".to_string()),
                expires_at_epoch_seconds: Some(current_epoch_seconds() + 3_600),
            },
            calls: Arc::new(Mutex::new(0)),
        });
        let engine = SpotifySessionEngine::with_token_refresher(
            SpotifySessionConfig::new("client"),
            backend.clone(),
            refresher.clone(),
        );

        engine
            .initialize_with_token(SpotifyToken {
                access_token: "expired-token".to_string(),
                refresh_token: Some("old-refresh".to_string()),
                expires_at_epoch_seconds: Some(current_epoch_seconds() - 1),
            })
            .await
            .expect("initialize should succeed");

        let refreshed = engine
            .refresh_if_needed()
            .await
            .expect("refresh_if_needed should refresh");

        assert!(refreshed);
        assert_eq!(engine.access_token().await.as_deref(), Some("new-token"));
        assert_eq!(*refresher.calls.lock().await, 1);
        assert_eq!(backend.connect_calls.lock().await.len(), 2);
        assert_eq!(*backend.disconnect_calls.lock().await, 1);
    }

    #[tokio::test]
    async fn close_clears_state() {
        let backend = MockBackend::default();
        let engine =
            SpotifySessionEngine::new(SpotifySessionConfig::new("client"), backend.clone());

        engine
            .initialize_with_token(SpotifyToken::with_access_token("token-1"))
            .await
            .expect("initialize should succeed");

        engine.close().await.expect("close should succeed");

        assert!(!engine.is_ready().await);
        assert_eq!(engine.state().await, SpotifySessionState::Closed);
        assert_eq!(engine.access_token().await, None);
        assert_eq!(*backend.disconnect_calls.lock().await, 1);
    }
}
