use super::required_session_param;
use crate::models::{Playlist, Source, Track};
use crate::provider_api::{ProviderApi, ProviderConnectionCheck};
use crate::providers::{ProviderAuthRequest, ProviderError};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;

const SPOTIFY_API_BASE_URL: &str = "https://api.spotify.com/v1";

pub struct SpotifyApiClient {
    client: Client,
}

impl Default for SpotifyApiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl SpotifyApiClient {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    pub fn with_client(client: Client) -> Self {
        Self { client }
    }

    fn require_access_token(session: &ProviderAuthRequest) -> Result<&str, ProviderError> {
        required_session_param(session, "Spotify", "access_token")
    }

    fn normalize_track_id(track_id: &str) -> String {
        let trimmed = track_id.trim();
        if let Some(stripped) = trimmed.strip_prefix("spotify:track:") {
            stripped.to_string()
        } else if let Some((_, rest)) = trimmed.split_once("/track/") {
            rest.split(['?', '/']).next().unwrap_or(rest).to_string()
        } else {
            trimmed.to_string()
        }
    }

    fn normalize_playlist_id(playlist_id: &str) -> String {
        let trimmed = playlist_id.trim();
        if let Some(stripped) = trimmed.strip_prefix("spotify:playlist:") {
            stripped.to_string()
        } else if let Some((_, rest)) = trimmed.split_once("/playlist/") {
            rest.split(['?', '/']).next().unwrap_or(rest).to_string()
        } else {
            trimmed.to_string()
        }
    }

    async fn execute_json(
        &self,
        path: &str,
        token: &str,
        query: &[(String, String)],
    ) -> Result<Value, ProviderError> {
        let endpoint = path.trim_start_matches('/');
        let url = format!("{}/{}", SPOTIFY_API_BASE_URL, endpoint);
        let mut request = self
            .client
            .get(url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/json");

        if !query.is_empty() {
            request = request.query(query);
        }

        let response = request
            .send()
            .await
            .map_err(|error| ProviderError(format!("Failed to call Spotify API: {}", error)))?;

        if !response.status().is_success() {
            return Err(ProviderError(format!(
                "Spotify API request failed: HTTP {}",
                response.status()
            )));
        }

        response
            .json::<Value>()
            .await
            .map_err(|error| ProviderError(format!("Failed to parse Spotify response: {}", error)))
    }

    fn parse_track(value: &Value) -> Option<Track> {
        let id = value.get("id").and_then(Value::as_str)?.to_string();
        let title = value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let artist = value
            .get("artists")
            .and_then(Value::as_array)
            .and_then(|artists| artists.first())
            .and_then(|artist| artist.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("Unknown Artist")
            .to_string();

        let album = value
            .get("album")
            .and_then(|album| album.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("Unknown Album")
            .to_string();

        let image_url = value
            .get("album")
            .and_then(|album| album.get("images"))
            .and_then(Value::as_array)
            .and_then(|images| images.first())
            .and_then(|image| image.get("url"))
            .and_then(Value::as_str)
            .map(str::to_string);

        let duration_ms = value
            .get("duration_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        Some(Track {
            id: id.clone(),
            title,
            artist,
            album,
            duration_ms,
            image_url,
            source: Source::Spotify,
            url: Some(format!("spotify:track:{}", id)),
            bitrate_kbps: None,
            sample_rate_hz: None,
            auth_headers: None,
            enriched: false,
        })
    }

    fn parse_playlist(value: &Value) -> Option<Playlist> {
        let id = value.get("id").and_then(Value::as_str)?.to_string();
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let description = value
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);
        let owner = value
            .get("owner")
            .and_then(|owner| owner.get("display_name"))
            .and_then(Value::as_str)
            .or_else(|| {
                value
                    .get("owner")
                    .and_then(|owner| owner.get("id"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("Spotify")
            .to_string();
        let image_url = value
            .get("images")
            .and_then(Value::as_array)
            .and_then(|images| images.first())
            .and_then(|image| image.get("url"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let track_count = value
            .get("tracks")
            .and_then(|tracks| tracks.get("total"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;

        Some(Playlist {
            id,
            name,
            description,
            owner,
            image_url,
            track_count,
            tracks: Vec::new(),
            source: Source::Spotify,
        })
    }
}

#[async_trait]
impl ProviderApi for SpotifyApiClient {
    fn source(&self) -> Source {
        Source::Spotify
    }

    async fn validate_connection(
        &self,
        session: &ProviderAuthRequest,
    ) -> Result<ProviderConnectionCheck, ProviderError> {
        let token = match Self::require_access_token(session) {
            Ok(token) => token,
            Err(error) => return Ok(ProviderConnectionCheck::Failed(error.0)),
        };

        let profile = match self.execute_json("me", token, &[]).await {
            Ok(profile) => profile,
            Err(error) => return Ok(ProviderConnectionCheck::Failed(error.0)),
        };

        let username = profile
            .get("display_name")
            .and_then(Value::as_str)
            .or_else(|| profile.get("id").and_then(Value::as_str))
            .map(str::to_string);

        let is_premium = profile
            .get("product")
            .and_then(Value::as_str)
            .map(|value| value == "premium")
            .unwrap_or(false);

        let mut metadata = HashMap::new();
        metadata.insert("isPremium".to_string(), is_premium.to_string());

        Ok(ProviderConnectionCheck::Connected { username, metadata })
    }

    async fn get_playlists(
        &self,
        session: &ProviderAuthRequest,
    ) -> Result<Vec<Playlist>, ProviderError> {
        let token = Self::require_access_token(session)?;
        let response = self
            .execute_json(
                "me/playlists",
                token,
                &[("limit".to_string(), "50".to_string())],
            )
            .await?;

        Ok(response
            .get("items")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Self::parse_playlist)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default())
    }

    async fn get_playlist(
        &self,
        session: &ProviderAuthRequest,
        id: &str,
    ) -> Result<Playlist, ProviderError> {
        let token = Self::require_access_token(session)?;
        let playlist_id = Self::normalize_playlist_id(id);
        if playlist_id.is_empty() {
            return Err(ProviderError("Spotify playlist ID is required".to_string()));
        }

        let metadata = self
            .execute_json(&format!("playlists/{}", playlist_id), token, &[])
            .await?;
        let tracks_response = self
            .execute_json(
                &format!("playlists/{}/tracks", playlist_id),
                token,
                &[
                    ("market".to_string(), "from_token".to_string()),
                    ("limit".to_string(), "100".to_string()),
                ],
            )
            .await?;

        let mut playlist = Self::parse_playlist(&metadata).ok_or_else(|| {
            ProviderError("Failed to parse Spotify playlist metadata".to_string())
        })?;
        let tracks = tracks_response
            .get("items")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("track"))
                    .filter_map(Self::parse_track)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if let Some(total) = tracks_response.get("total").and_then(Value::as_u64) {
            playlist.track_count = total as usize;
        } else {
            playlist.track_count = tracks.len();
        }
        playlist.tracks = tracks;
        Ok(playlist)
    }

    async fn get_track(
        &self,
        session: &ProviderAuthRequest,
        id: &str,
    ) -> Result<Track, ProviderError> {
        let token = Self::require_access_token(session)?;
        let track_id = Self::normalize_track_id(id);
        if track_id.is_empty() {
            return Err(ProviderError("Spotify track ID is required".to_string()));
        }

        let response = self
            .execute_json(
                &format!("tracks/{}", track_id),
                token,
                &[("market".to_string(), "from_token".to_string())],
            )
            .await?;
        Self::parse_track(&response)
            .ok_or_else(|| ProviderError(format!("Spotify track not found: {}", track_id)))
    }

    async fn search_tracks(
        &self,
        session: &ProviderAuthRequest,
        query: &str,
    ) -> Result<Vec<Track>, ProviderError> {
        let token = Self::require_access_token(session)?;
        let normalized = query.trim();
        if normalized.is_empty() {
            return Ok(Vec::new());
        }

        let response = self
            .execute_json(
                "search",
                token,
                &[
                    ("q".to_string(), normalized.to_string()),
                    ("type".to_string(), "track".to_string()),
                    ("market".to_string(), "from_token".to_string()),
                    ("limit".to_string(), "50".to_string()),
                ],
            )
            .await?;

        Ok(response
            .get("tracks")
            .and_then(|tracks| tracks.get("items"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Self::parse_track)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default())
    }

    async fn search_playlists(
        &self,
        session: &ProviderAuthRequest,
        query: &str,
    ) -> Result<Vec<Playlist>, ProviderError> {
        let token = Self::require_access_token(session)?;
        let normalized = query.trim();
        if normalized.is_empty() {
            return Ok(Vec::new());
        }

        let response = self
            .execute_json(
                "search",
                token,
                &[
                    ("q".to_string(), normalized.to_string()),
                    ("type".to_string(), "playlist".to_string()),
                    ("limit".to_string(), "50".to_string()),
                ],
            )
            .await?;

        Ok(response
            .get("playlists")
            .and_then(|playlists| playlists.get("items"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Self::parse_playlist)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default())
    }

    async fn get_stream_url(
        &self,
        _session: &ProviderAuthRequest,
        track_id: &str,
    ) -> Result<String, ProviderError> {
        let normalized = Self::normalize_track_id(track_id);
        if normalized.is_empty() {
            return Err(ProviderError("Spotify track ID is required".to_string()));
        }

        Ok(format!("spotify:track:{}", normalized))
    }
}
