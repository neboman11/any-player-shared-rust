use crate::models::{Playlist, Source, Track};
use async_trait::async_trait;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

/// Error type for provider operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError(pub String);

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ProviderError {}

/// Generic provider authentication request payload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderAuthRequest {
    params: HashMap<String, String>,
}

impl ProviderAuthRequest {
    pub fn new(params: HashMap<String, String>) -> Self {
        Self { params }
    }

    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut params = HashMap::new();
        for (key, value) in pairs {
            params.insert(key.into(), value.into());
        }
        Self { params }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(|value| value.as_str())
    }

    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.params.insert(key.into(), value.into());
    }

    pub fn as_map(&self) -> &HashMap<String, String> {
        &self.params
    }

    pub fn into_inner(self) -> HashMap<String, String> {
        self.params
    }
}

/// Generic provider authentication response payload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderAuthResponse {
    data: HashMap<String, String>,
}

impl ProviderAuthResponse {
    pub fn new(data: HashMap<String, String>) -> Self {
        Self { data }
    }

    pub fn with_auth_url(url: String) -> Self {
        let mut data = HashMap::new();
        data.insert("auth_url".to_string(), url);
        Self { data }
    }

    pub fn auth_url(&self) -> Option<&str> {
        self.data.get("auth_url").map(|value| value.as_str())
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.data.get(key).map(|value| value.as_str())
    }

    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.data.insert(key.into(), value.into());
    }

    pub fn as_map(&self) -> &HashMap<String, String> {
        &self.data
    }

    pub fn into_inner(self) -> HashMap<String, String> {
        self.data
    }
}

/// Core trait that all music providers must implement.
#[async_trait]
pub trait MusicProvider: Send + Sync {
    fn as_any(&self) -> &dyn Any;

    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Get the source provider type.
    fn source(&self) -> Source;

    /// Authenticate with the provider.
    async fn authenticate(&mut self) -> Result<(), ProviderError>;

    /// Begin provider authentication flow (e.g., generate OAuth URL).
    async fn begin_auth(
        &mut self,
        _request: ProviderAuthRequest,
    ) -> Result<ProviderAuthResponse, ProviderError> {
        Err(ProviderError(
            "Authentication start is not supported for this provider".to_string(),
        ))
    }

    /// Complete provider authentication flow (e.g., exchange code or verify API key).
    async fn complete_auth(&mut self, _request: ProviderAuthRequest) -> Result<(), ProviderError> {
        Err(ProviderError(
            "Authentication completion is not supported for this provider".to_string(),
        ))
    }

    /// Check if provider is authenticated.
    fn is_authenticated(&self) -> bool;

    /// Get user's playlists.
    async fn get_playlists(&self) -> Result<Vec<Playlist>, ProviderError>;

    /// Get a specific playlist by ID.
    async fn get_playlist(&self, id: &str) -> Result<Playlist, ProviderError>;

    /// Get a specific track by ID.
    async fn get_track(&self, id: &str) -> Result<Track, ProviderError>;

    /// Search for tracks by query.
    async fn search_tracks(&self, query: &str) -> Result<Vec<Track>, ProviderError>;

    /// Search for playlists by query.
    async fn search_playlists(&self, query: &str) -> Result<Vec<Playlist>, ProviderError>;

    /// Get a streamable URL for a track.
    async fn get_stream_url(&self, track_id: &str) -> Result<String, ProviderError>;

    /// Optional auth headers for streaming requests.
    async fn get_auth_headers(&self) -> Option<Vec<(String, String)>> {
        None
    }

    /// Create a new playlist.
    async fn create_playlist(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<Playlist, ProviderError>;

    /// Add a track to a playlist.
    async fn add_track_to_playlist(
        &self,
        playlist_id: &str,
        track: &Track,
    ) -> Result<(), ProviderError>;

    /// Remove a track from a playlist.
    async fn remove_track_from_playlist(
        &self,
        playlist_id: &str,
        track_id: &str,
    ) -> Result<(), ProviderError>;

    /// Get recently played tracks.
    async fn get_recently_played(&self, limit: usize) -> Result<Vec<Track>, ProviderError>;

    /// Optional access token for providers that support token-based sessions.
    async fn get_access_token(&self) -> Option<String> {
        None
    }

    /// Refresh provider authentication state.
    async fn refresh_auth(&mut self) -> Result<(), ProviderError> {
        Err(ProviderError(
            "Authentication refresh is not supported for this provider".to_string(),
        ))
    }

    /// Provider-specific premium status (if applicable).
    async fn premium_status(&self) -> Option<bool> {
        None
    }

    /// Disconnect provider and clear any in-memory state.
    async fn disconnect(&mut self) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// Shared provider handle type.
pub type ProviderHandle = Arc<tokio::sync::Mutex<Box<dyn MusicProvider>>>;

/// Runtime-agnostic provider registry.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: HashMap<Source, ProviderHandle>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: impl MusicProvider + 'static) -> ProviderHandle {
        let source = provider.source();
        let boxed: Box<dyn MusicProvider> = Box::new(provider);
        self.register_boxed_with_source(source, boxed)
    }

    pub fn register_boxed(&mut self, provider: Box<dyn MusicProvider>) -> ProviderHandle {
        let source = provider.source();
        self.register_boxed_with_source(source, provider)
    }

    fn register_boxed_with_source(
        &mut self,
        source: Source,
        provider: Box<dyn MusicProvider>,
    ) -> ProviderHandle {
        let handle: ProviderHandle = Arc::new(tokio::sync::Mutex::new(provider));
        self.providers.insert(source, handle.clone());
        handle
    }

    pub fn get(&self, source: Source) -> Option<ProviderHandle> {
        self.providers.get(&source).cloned()
    }

    pub fn contains(&self, source: Source) -> bool {
        self.providers.contains_key(&source)
    }

    pub fn get_all(&self) -> Vec<ProviderHandle> {
        self.providers.values().cloned().collect()
    }

    fn require_provider(&self, source: Source) -> Result<ProviderHandle, ProviderError> {
        self.get(source).ok_or_else(|| {
            ProviderError(format!("Provider not initialized for source: {}", source))
        })
    }

    pub async fn begin_auth(
        &self,
        source: Source,
        request: ProviderAuthRequest,
    ) -> Result<ProviderAuthResponse, ProviderError> {
        let provider = self.require_provider(source)?;
        let mut provider = provider.lock().await;
        provider.begin_auth(request).await
    }

    pub async fn complete_auth(
        &self,
        source: Source,
        request: ProviderAuthRequest,
    ) -> Result<(), ProviderError> {
        let provider = self.require_provider(source)?;
        let mut provider = provider.lock().await;
        provider.complete_auth(request).await
    }

    pub async fn authenticate(&self, source: Source) -> Result<(), ProviderError> {
        let provider = self.require_provider(source)?;
        let mut provider = provider.lock().await;
        provider.authenticate().await
    }

    pub async fn is_authenticated(&self, source: Source) -> bool {
        if let Some(provider) = self.get(source) {
            let provider = provider.lock().await;
            provider.is_authenticated()
        } else {
            false
        }
    }

    pub async fn get_playlists(&self, source: Source) -> Result<Vec<Playlist>, ProviderError> {
        let provider = self.require_provider(source)?;
        let provider = provider.lock().await;
        provider.get_playlists().await
    }

    pub async fn get_playlist(&self, source: Source, id: &str) -> Result<Playlist, ProviderError> {
        let provider = self.require_provider(source)?;
        let provider = provider.lock().await;
        provider.get_playlist(id).await
    }

    pub async fn get_track(&self, source: Source, id: &str) -> Result<Track, ProviderError> {
        let provider = self.require_provider(source)?;
        let provider = provider.lock().await;
        provider.get_track(id).await
    }

    pub async fn search_tracks(
        &self,
        source: Source,
        query: &str,
    ) -> Result<Vec<Track>, ProviderError> {
        let provider = self.require_provider(source)?;
        let provider = provider.lock().await;
        provider.search_tracks(query).await
    }

    pub async fn search_playlists(
        &self,
        source: Source,
        query: &str,
    ) -> Result<Vec<Playlist>, ProviderError> {
        let provider = self.require_provider(source)?;
        let provider = provider.lock().await;
        provider.search_playlists(query).await
    }

    pub async fn get_stream_url(
        &self,
        source: Source,
        track_id: &str,
    ) -> Result<String, ProviderError> {
        let provider = self.require_provider(source)?;
        let provider = provider.lock().await;
        provider.get_stream_url(track_id).await
    }

    pub async fn create_playlist(
        &self,
        source: Source,
        name: &str,
        description: Option<&str>,
    ) -> Result<Playlist, ProviderError> {
        let provider = self.require_provider(source)?;
        let provider = provider.lock().await;
        provider.create_playlist(name, description).await
    }

    pub async fn add_track_to_playlist(
        &self,
        source: Source,
        playlist_id: &str,
        track: &Track,
    ) -> Result<(), ProviderError> {
        let provider = self.require_provider(source)?;
        let provider = provider.lock().await;
        provider.add_track_to_playlist(playlist_id, track).await
    }

    pub async fn remove_track_from_playlist(
        &self,
        source: Source,
        playlist_id: &str,
        track_id: &str,
    ) -> Result<(), ProviderError> {
        let provider = self.require_provider(source)?;
        let provider = provider.lock().await;
        provider
            .remove_track_from_playlist(playlist_id, track_id)
            .await
    }

    pub async fn get_recently_played(
        &self,
        source: Source,
        limit: usize,
    ) -> Result<Vec<Track>, ProviderError> {
        let provider = self.require_provider(source)?;
        let provider = provider.lock().await;
        provider.get_recently_played(limit).await
    }

    pub async fn get_auth_headers(&self, source: Source) -> Option<Vec<(String, String)>> {
        if let Some(provider) = self.get(source) {
            let provider = provider.lock().await;
            provider.get_auth_headers().await
        } else {
            None
        }
    }

    pub async fn get_access_token(&self, source: Source) -> Option<String> {
        if let Some(provider) = self.get(source) {
            let provider = provider.lock().await;
            provider.get_access_token().await
        } else {
            None
        }
    }

    pub async fn refresh_auth(&self, source: Source) -> Result<(), ProviderError> {
        let provider = self.require_provider(source)?;
        let mut provider = provider.lock().await;
        provider.refresh_auth().await
    }

    pub async fn premium_status(&self, source: Source) -> Option<bool> {
        if let Some(provider) = self.get(source) {
            let provider = provider.lock().await;
            provider.premium_status().await
        } else {
            None
        }
    }

    pub async fn disconnect(&mut self, source: Source) -> Result<(), ProviderError> {
        if let Some(handle) = self.providers.remove(&source) {
            let mut provider = handle.lock().await;
            provider.disconnect().await
        } else {
            Ok(())
        }
    }
}
