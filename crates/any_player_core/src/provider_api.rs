use crate::models::{Playlist, Source, Track};
use crate::providers::{ProviderAuthRequest, ProviderAuthResponse, ProviderError};
use async_trait::async_trait;
use std::collections::HashMap;

/// Connection health result returned by provider API checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderConnectionCheck {
    Connected {
        username: Option<String>,
        metadata: HashMap<String, String>,
    },
    Failed(String),
}

/// Platform-owned authentication orchestration boundary.
#[async_trait]
pub trait AuthFlow: Send + Sync {
    /// Begin auth flow and return provider-specific data (e.g. authorization URL).
    async fn begin_auth(
        &self,
        source: Source,
        request: ProviderAuthRequest,
    ) -> Result<ProviderAuthResponse, ProviderError>;

    /// Complete auth flow and return normalized provider session params for shared APIs.
    async fn complete_auth(
        &self,
        source: Source,
        request: ProviderAuthRequest,
    ) -> Result<ProviderAuthRequest, ProviderError>;
}

/// Shared provider API surface implemented by provider HTTP clients.
#[async_trait]
pub trait ProviderApi: Send + Sync {
    fn source(&self) -> Source;

    async fn validate_connection(
        &self,
        session: &ProviderAuthRequest,
    ) -> Result<ProviderConnectionCheck, ProviderError>;

    async fn get_playlists(
        &self,
        session: &ProviderAuthRequest,
    ) -> Result<Vec<Playlist>, ProviderError>;

    async fn get_playlist(
        &self,
        session: &ProviderAuthRequest,
        id: &str,
    ) -> Result<Playlist, ProviderError>;

    async fn get_track(
        &self,
        session: &ProviderAuthRequest,
        id: &str,
    ) -> Result<Track, ProviderError>;

    async fn search_tracks(
        &self,
        session: &ProviderAuthRequest,
        query: &str,
    ) -> Result<Vec<Track>, ProviderError>;

    async fn search_playlists(
        &self,
        session: &ProviderAuthRequest,
        query: &str,
    ) -> Result<Vec<Playlist>, ProviderError>;

    async fn get_stream_url(
        &self,
        session: &ProviderAuthRequest,
        track_id: &str,
    ) -> Result<String, ProviderError>;
}
