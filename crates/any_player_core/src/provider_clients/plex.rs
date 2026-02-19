use crate::models::{Playlist, Source, Track};
use crate::provider_api::{ProviderApi, ProviderConnectionCheck};
use crate::providers::{ProviderAuthRequest, ProviderError};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;

const PLEX_TYPE_PLAYLIST: &str = "15";

pub struct PlexApiClient {
    client: Client,
}

#[derive(Debug, Deserialize)]
struct PlexResponse {
    #[serde(rename = "MediaContainer")]
    media_container: PlexMediaContainer,
}

#[derive(Debug, Deserialize)]
struct PlexMediaContainer {
    #[serde(default, rename = "Metadata")]
    metadata: Vec<PlexMetadata>,
}

#[derive(Debug, Deserialize)]
struct PlexMetadata {
    #[serde(rename = "ratingKey")]
    rating_key: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default, rename = "leafCount")]
    leaf_count: Option<u32>,
    #[serde(default)]
    thumb: Option<String>,
    #[serde(default)]
    duration: Option<u64>,
    #[serde(default, rename = "grandparentTitle")]
    grandparent_title: Option<String>,
    #[serde(default, rename = "parentTitle")]
    parent_title: Option<String>,
    #[serde(default)]
    #[serde(rename = "Media")]
    media: Option<Vec<PlexMedia>>,
    #[serde(default, rename = "type")]
    item_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PlexMedia {
    #[serde(default)]
    bitrate: Option<u32>,
    #[serde(default, rename = "audioSamplingRate")]
    audio_sampling_rate: Option<u32>,
    #[serde(default, rename = "Part")]
    part: Option<Vec<PlexPart>>,
}

#[derive(Debug, Deserialize)]
struct PlexPart {
    #[serde(default)]
    key: Option<String>,
}

impl Default for PlexApiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl PlexApiClient {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    pub fn with_client(client: Client) -> Self {
        Self { client }
    }

    fn required_param<'a>(
        session: &'a ProviderAuthRequest,
        key: &'static str,
    ) -> Result<&'a str, ProviderError> {
        session
            .get(key)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ProviderError(format!("Missing Plex {}", key)))
    }

    fn session_base_url(session: &ProviderAuthRequest) -> Result<String, ProviderError> {
        let url = Self::required_param(session, "url")?;
        Ok(url.trim_end_matches('/').to_string())
    }

    fn session_token(session: &ProviderAuthRequest) -> Result<&str, ProviderError> {
        Self::required_param(session, "token")
    }

    fn authed_url(base_url: &str, token: &str, path_with_query: &str) -> String {
        let path = path_with_query.trim_start_matches('/');
        if path.contains('?') {
            format!("{}/{}&X-Plex-Token={}", base_url, path, token)
        } else {
            format!("{}/{}?X-Plex-Token={}", base_url, path, token)
        }
    }

    fn image_url_from_path(
        base_url: &str,
        token: &str,
        maybe_path: &Option<String>,
    ) -> Option<String> {
        maybe_path.as_ref().map(|path| {
            let path = path.trim_start_matches('/');
            format!("{}/{}?X-Plex-Token={}", base_url, path, token)
        })
    }

    fn track_from_metadata(base_url: &str, token: &str, item: PlexMetadata) -> Option<Track> {
        let id = item.rating_key?;
        let artist = item
            .grandparent_title
            .unwrap_or_else(|| "Unknown Artist".to_string());
        let album = item
            .parent_title
            .unwrap_or_else(|| "Unknown Album".to_string());
        let image_url = Self::image_url_from_path(base_url, token, &item.thumb);

        let stream_key = item
            .media
            .as_ref()
            .and_then(|medias| medias.first())
            .and_then(|media| media.part.as_ref())
            .and_then(|parts| parts.first())
            .and_then(|part| part.key.as_ref())
            .cloned();

        let stream_url = stream_key.map(|key| {
            let key = key.trim_start_matches('/');
            format!("{}/{}?X-Plex-Token={}", base_url, key, token)
        });

        let bitrate_kbps = item
            .media
            .as_ref()
            .and_then(|medias| medias.first())
            .and_then(|media| media.bitrate);
        let sample_rate_hz = item
            .media
            .as_ref()
            .and_then(|medias| medias.first())
            .and_then(|media| media.audio_sampling_rate);

        Some(Track {
            id,
            title: item.title,
            artist,
            album,
            duration_ms: item.duration.unwrap_or(0),
            image_url,
            source: Source::Plex,
            url: stream_url,
            bitrate_kbps,
            sample_rate_hz,
            auth_headers: None,
            enriched: false,
        })
    }

    fn playlist_from_metadata(base_url: &str, token: &str, item: PlexMetadata) -> Option<Playlist> {
        let id = item.rating_key?;
        Some(Playlist {
            id,
            name: item.title,
            description: item.summary,
            owner: "Plex".to_string(),
            image_url: Self::image_url_from_path(base_url, token, &item.thumb),
            track_count: item.leaf_count.unwrap_or(0) as usize,
            tracks: Vec::new(),
            source: Source::Plex,
        })
    }

    async fn parse_json_response<T: for<'de> Deserialize<'de>>(
        response: reqwest::Response,
        action: &'static str,
    ) -> Result<T, ProviderError> {
        let body = response
            .text()
            .await
            .map_err(|error| ProviderError(format!("Failed to read {} body: {}", action, error)))?;
        serde_json::from_str::<T>(&body).map_err(|error| {
            ProviderError(format!("Failed to parse {} response: {}", action, error))
        })
    }

    async fn get_tracks_from_endpoint(
        &self,
        base_url: &str,
        token: &str,
        endpoint: &str,
    ) -> Result<Vec<Track>, ProviderError> {
        let response = self
            .client
            .get(Self::authed_url(base_url, token, endpoint))
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|error| ProviderError(format!("Failed to fetch Plex tracks: {}", error)))?;

        if !response.status().is_success() {
            return Err(ProviderError(format!(
                "Plex track request failed: HTTP {}",
                response.status()
            )));
        }

        let parsed = Self::parse_json_response::<PlexResponse>(response, "Plex track").await?;
        Ok(parsed
            .media_container
            .metadata
            .into_iter()
            .filter_map(|item| Self::track_from_metadata(base_url, token, item))
            .collect())
    }

    async fn get_playlists_from_endpoint(
        &self,
        base_url: &str,
        token: &str,
        endpoint: &str,
        type_filter: Option<&str>,
    ) -> Result<Vec<Playlist>, ProviderError> {
        let response = self
            .client
            .get(Self::authed_url(base_url, token, endpoint))
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|error| ProviderError(format!("Failed to fetch Plex playlists: {}", error)))?;

        if !response.status().is_success() {
            return Err(ProviderError(format!(
                "Plex playlist request failed: HTTP {}",
                response.status()
            )));
        }

        let parsed = Self::parse_json_response::<PlexResponse>(response, "Plex playlist").await?;
        Ok(parsed
            .media_container
            .metadata
            .into_iter()
            .filter(|item| {
                type_filter.is_none_or(|filter_type| item.item_type.as_deref() == Some(filter_type))
            })
            .filter_map(|item| Self::playlist_from_metadata(base_url, token, item))
            .collect())
    }
}

#[async_trait]
impl ProviderApi for PlexApiClient {
    fn source(&self) -> Source {
        Source::Plex
    }

    async fn validate_connection(
        &self,
        session: &ProviderAuthRequest,
    ) -> Result<ProviderConnectionCheck, ProviderError> {
        let base_url = Self::session_base_url(session)?;
        let token = Self::session_token(session)?;
        let response = self
            .client
            .get(Self::authed_url(&base_url, token, "identity"))
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|error| ProviderError(format!("Failed to connect to Plex: {}", error)));

        match response {
            Ok(response) if response.status().is_success() => {
                let mut metadata = HashMap::new();
                metadata.insert("server".to_string(), base_url);
                Ok(ProviderConnectionCheck::Connected {
                    username: None,
                    metadata,
                })
            }
            Ok(response) => Ok(ProviderConnectionCheck::Failed(format!(
                "Plex authentication failed: HTTP {}",
                response.status()
            ))),
            Err(error) => Ok(ProviderConnectionCheck::Failed(error.0)),
        }
    }

    async fn get_playlists(
        &self,
        session: &ProviderAuthRequest,
    ) -> Result<Vec<Playlist>, ProviderError> {
        let base_url = Self::session_base_url(session)?;
        let token = Self::session_token(session)?;
        self.get_playlists_from_endpoint(&base_url, token, "playlists/all?type=15", None)
            .await
    }

    async fn get_playlist(
        &self,
        session: &ProviderAuthRequest,
        id: &str,
    ) -> Result<Playlist, ProviderError> {
        let base_url = Self::session_base_url(session)?;
        let token = Self::session_token(session)?;

        let mut playlists = self
            .get_playlists_from_endpoint(
                &base_url,
                token,
                &format!("library/metadata/{}", id),
                None,
            )
            .await?;
        let mut playlist = playlists
            .pop()
            .ok_or_else(|| ProviderError(format!("Plex playlist not found: {}", id)))?;

        let tracks = self
            .get_tracks_from_endpoint(&base_url, token, &format!("playlists/{}/items", id))
            .await?;
        playlist.track_count = tracks.len();
        playlist.tracks = tracks;
        Ok(playlist)
    }

    async fn get_track(
        &self,
        session: &ProviderAuthRequest,
        id: &str,
    ) -> Result<Track, ProviderError> {
        let base_url = Self::session_base_url(session)?;
        let token = Self::session_token(session)?;
        let tracks = self
            .get_tracks_from_endpoint(&base_url, token, &format!("library/metadata/{}", id))
            .await?;
        tracks
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError(format!("Plex track not found: {}", id)))
    }

    async fn search_tracks(
        &self,
        session: &ProviderAuthRequest,
        query: &str,
    ) -> Result<Vec<Track>, ProviderError> {
        let base_url = Self::session_base_url(session)?;
        let token = Self::session_token(session)?;
        let encoded_query: String =
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
        self.get_tracks_from_endpoint(
            &base_url,
            token,
            &format!("search?query={}&limit=50", encoded_query),
        )
        .await
    }

    async fn search_playlists(
        &self,
        session: &ProviderAuthRequest,
        query: &str,
    ) -> Result<Vec<Playlist>, ProviderError> {
        let base_url = Self::session_base_url(session)?;
        let token = Self::session_token(session)?;
        let encoded_query: String =
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
        self.get_playlists_from_endpoint(
            &base_url,
            token,
            &format!("search?query={}&limit=50", encoded_query),
            Some(PLEX_TYPE_PLAYLIST),
        )
        .await
    }

    async fn get_stream_url(
        &self,
        session: &ProviderAuthRequest,
        track_id: &str,
    ) -> Result<String, ProviderError> {
        let track = self.get_track(session, track_id).await?;
        track
            .url
            .ok_or_else(|| ProviderError("No stream URL available for Plex track".to_string()))
    }
}
