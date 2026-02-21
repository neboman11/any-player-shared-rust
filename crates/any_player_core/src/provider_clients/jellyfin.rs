use crate::models::{Playlist, Source, Track};
use crate::provider_api::{ProviderApi, ProviderConnectionCheck};
use crate::providers::{ProviderAuthRequest, ProviderError};
use super::required_session_param;
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, RequestBuilder};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;

const JELLYFIN_STREAM_CONTAINERS: &str = "opus,mp3,aac,m4a,flac,webma,webm,wav,ogg";
const JELLYFIN_STREAM_CODECS: &str = "aac,mp3,vorbis,opus";
const JELLYFIN_MAX_PLAYLIST_PAGES: usize = 1000;

pub struct JellyfinApiClient {
    client: Client,
}

#[derive(Debug, Clone, Deserialize)]
struct JellyfinUser {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct JellyfinItem {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Type")]
    item_type: String,
    #[serde(rename = "Album")]
    album: Option<String>,
    #[serde(rename = "AlbumId")]
    album_id: Option<String>,
    #[serde(rename = "Artists")]
    artists: Option<Vec<String>>,
    #[serde(rename = "RunTimeTicks")]
    runtime_ticks: Option<u64>,
    #[serde(rename = "ImageTags")]
    image_tags: Option<Value>,
    #[serde(rename = "AlbumPrimaryImageTag")]
    album_primary_image_tag: Option<String>,
    #[serde(rename = "ChildCount")]
    child_count: Option<u32>,
    #[serde(rename = "Bitrate")]
    #[serde(alias = "BitRate")]
    bitrate: Option<u32>,
    #[serde(rename = "SampleRate")]
    #[serde(alias = "SamplingRate")]
    sample_rate: Option<u32>,
    #[serde(rename = "MediaStreams")]
    media_streams: Option<Vec<JellyfinMediaStream>>,
    #[serde(rename = "MediaSources")]
    media_sources: Option<Vec<JellyfinMediaSource>>,
}

#[derive(Debug, Clone, Deserialize)]
struct JellyfinMediaStream {
    #[serde(rename = "Type")]
    stream_type: Option<String>,
    #[serde(rename = "BitRate")]
    #[serde(alias = "Bitrate")]
    bit_rate: Option<u32>,
    #[serde(rename = "SampleRate")]
    #[serde(alias = "SamplingRate")]
    sample_rate: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
struct JellyfinMediaSource {
    #[serde(rename = "Bitrate")]
    #[serde(alias = "BitRate")]
    bitrate: Option<u32>,
    #[serde(rename = "MediaStreams")]
    media_streams: Option<Vec<JellyfinMediaStream>>,
}

#[derive(Debug, Clone, Deserialize)]
struct JellyfinItemsResponse {
    #[serde(rename = "Items")]
    items: Vec<JellyfinItem>,
    #[serde(rename = "TotalRecordCount")]
    total_record_count: u32,
}

impl Default for JellyfinApiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl JellyfinApiClient {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    pub fn with_client(client: Client) -> Self {
        Self { client }
    }

    fn base_url(session: &ProviderAuthRequest) -> Result<String, ProviderError> {
        let raw_url = required_session_param(session, "Jellyfin", "url")?;
        Ok(raw_url.trim_end_matches('/').to_string())
    }

    fn api_key(session: &ProviderAuthRequest) -> Result<&str, ProviderError> {
        required_session_param(session, "Jellyfin", "api_key")
    }

    fn build_headers(api_key: &str) -> Result<HeaderMap, ProviderError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Emby-Token",
            HeaderValue::from_str(api_key).map_err(|error| {
                ProviderError(format!("Invalid Jellyfin API key header value: {}", error))
            })?,
        );
        headers.insert(
            "X-Emby-Authorization",
            HeaderValue::from_str(&format!(
                "MediaBrowser Token=\"{}\", Client=\"AnyPlayer\", Device=\"AnyPlayer\", DeviceId=\"AnyPlayer\", Version=\"1.0.0\"",
                api_key
            ))
            .map_err(|error| {
                ProviderError(format!(
                    "Invalid Jellyfin authorization header value: {}",
                    error
                ))
            })?,
        );
        Ok(headers)
    }

    async fn send_json<T: DeserializeOwned>(
        &self,
        request: RequestBuilder,
        action: &'static str,
    ) -> Result<T, ProviderError> {
        let response = request
            .send()
            .await
            .map_err(|error| ProviderError(format!("Failed to {}: {}", action, error)))?;

        if !response.status().is_success() {
            return Err(ProviderError(format!(
                "Failed to {}: HTTP {}",
                action,
                response.status()
            )));
        }

        response.json::<T>().await.map_err(|error| {
            ProviderError(format!("Failed to parse {} response: {}", action, error))
        })
    }

    async fn resolve_user(
        &self,
        session: &ProviderAuthRequest,
        base_url: &str,
        api_key: &str,
    ) -> Result<JellyfinUser, ProviderError> {
        let users: Vec<JellyfinUser> = self
            .send_json(
                self.client
                    .get(format!("{}/Users", base_url))
                    .headers(Self::build_headers(api_key)?),
                "fetch Jellyfin users",
            )
            .await?;

        if users.is_empty() {
            return Err(ProviderError(
                "No users found on Jellyfin server".to_string(),
            ));
        }

        if let Some(configured_user_id) = session
            .get("user_id")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            && let Some(user) = users.iter().find(|user| user.id == configured_user_id)
        {
            return Ok(user.clone());
        }

        Ok(users[0].clone())
    }

    fn get_image_url(base_url: &str, api_key: &str, item: &JellyfinItem) -> Option<String> {
        if item.item_type == "Audio"
            && let (Some(album_id), Some(album_tag)) =
                (&item.album_id, &item.album_primary_image_tag)
        {
            return Some(format!(
                "{}/Items/{}/Images/Primary?tag={}&api_key={}",
                base_url, album_id, album_tag, api_key
            ));
        }

        if let Some(tags) = &item.image_tags
            && let Some(primary_tag) = tags.get("Primary").and_then(Value::as_str)
        {
            return Some(format!(
                "{}/Items/{}/Images/Primary?tag={}&api_key={}",
                base_url, item.id, primary_tag, api_key
            ));
        }

        None
    }

    fn build_auth_headers_vec(api_key: &str) -> Vec<(String, String)> {
        vec![
            ("X-Emby-Token".to_string(), api_key.to_string()),
            (
                "X-Emby-Authorization".to_string(),
                format!(
                    "MediaBrowser Token=\"{}\", Client=\"AnyPlayer\", Device=\"AnyPlayer\", DeviceId=\"AnyPlayer\", Version=\"1.0.0\"",
                    api_key
                ),
            ),
        ]
    }

    fn get_audio_quality(item: &JellyfinItem) -> (Option<u32>, Option<u32>) {
        if let Some(sources) = &item.media_sources
            && let Some(source) = sources.first()
        {
            if let Some(streams) = &source.media_streams
                && let Some(audio_stream) = streams.iter().find(|stream| {
                    stream
                        .stream_type
                        .as_deref()
                        .map(|value| value.eq_ignore_ascii_case("Audio"))
                        .unwrap_or(false)
                })
            {
                let bitrate_kbps = audio_stream.bit_rate.map(|value| value / 1000);
                let fallback_bitrate = source.bitrate.map(|value| value / 1000);
                return (bitrate_kbps.or(fallback_bitrate), audio_stream.sample_rate);
            }

            if source.bitrate.is_some() {
                return (source.bitrate.map(|value| value / 1000), None);
            }
        }

        if let Some(streams) = &item.media_streams
            && let Some(audio_stream) = streams.iter().find(|stream| {
                stream
                    .stream_type
                    .as_deref()
                    .map(|value| value.eq_ignore_ascii_case("Audio"))
                    .unwrap_or(false)
            })
        {
            return (
                audio_stream.bit_rate.map(|value| value / 1000),
                audio_stream.sample_rate,
            );
        }

        (item.bitrate.map(|value| value / 1000), item.sample_rate)
    }

    fn item_to_track(base_url: &str, api_key: &str, user_id: &str, item: &JellyfinItem) -> Track {
        let duration_ms = item.runtime_ticks.map(|ticks| ticks / 10_000).unwrap_or(0);
        let artist = item
            .artists
            .as_ref()
            .and_then(|artists| artists.first())
            .cloned()
            .unwrap_or_else(|| "Unknown Artist".to_string());
        let album = item
            .album
            .clone()
            .unwrap_or_else(|| "Unknown Album".to_string());
        let image_url = Self::get_image_url(base_url, api_key, item);
        let (bitrate_kbps, sample_rate_hz) = Self::get_audio_quality(item);
        let stream_url = format!(
            "{}/Audio/{}/universal?UserId={}&Container={}&AudioCodec={}",
            base_url, item.id, user_id, JELLYFIN_STREAM_CONTAINERS, JELLYFIN_STREAM_CODECS
        );

        Track {
            id: item.id.clone(),
            title: item.name.clone(),
            artist,
            album,
            duration_ms,
            image_url,
            source: Source::Jellyfin,
            url: Some(stream_url),
            bitrate_kbps,
            sample_rate_hz,
            auth_headers: Some(Self::build_auth_headers_vec(api_key)),
            enriched: false,
        }
    }

    fn item_to_playlist(base_url: &str, api_key: &str, item: &JellyfinItem) -> Playlist {
        Playlist {
            id: item.id.clone(),
            name: item.name.clone(),
            description: None,
            owner: "Jellyfin".to_string(),
            image_url: Self::get_image_url(base_url, api_key, item),
            track_count: item.child_count.unwrap_or(0) as usize,
            tracks: Vec::new(),
            source: Source::Jellyfin,
        }
    }

    fn create_fallback_playlist(id: &str, tracks: Vec<Track>) -> Playlist {
        let track_count = tracks.len();
        Playlist {
            id: id.to_string(),
            name: format!("Playlist {}", id),
            description: None,
            owner: "Jellyfin".to_string(),
            image_url: None,
            track_count,
            tracks,
            source: Source::Jellyfin,
        }
    }
}

#[async_trait]
impl ProviderApi for JellyfinApiClient {
    fn source(&self) -> Source {
        Source::Jellyfin
    }

    async fn validate_connection(
        &self,
        session: &ProviderAuthRequest,
    ) -> Result<ProviderConnectionCheck, ProviderError> {
        let base_url = Self::base_url(session)?;
        let api_key = Self::api_key(session)?;
        let headers = Self::build_headers(api_key)?;

        let check_result = self
            .send_json::<Value>(
                self.client
                    .get(format!("{}/System/Info", base_url))
                    .headers(headers),
                "validate Jellyfin connection",
            )
            .await;

        if let Err(error) = check_result {
            return Ok(ProviderConnectionCheck::Failed(error.0));
        }

        let user = self.resolve_user(session, &base_url, api_key).await?;
        let mut metadata = HashMap::new();
        metadata.insert("user_id".to_string(), user.id.clone());

        Ok(ProviderConnectionCheck::Connected {
            username: Some(user.name),
            metadata,
        })
    }

    async fn get_playlists(
        &self,
        session: &ProviderAuthRequest,
    ) -> Result<Vec<Playlist>, ProviderError> {
        let base_url = Self::base_url(session)?;
        let api_key = Self::api_key(session)?;
        let user = self.resolve_user(session, &base_url, api_key).await?;
        let headers = Self::build_headers(api_key)?;

        let response: JellyfinItemsResponse = self
            .send_json(
                self.client
                    .get(format!("{}/Users/{}/Items", base_url, user.id))
                    .headers(headers)
                    .query(&[
                        ("Filters", "IsFolder"),
                        ("Recursive", "true"),
                        ("IncludeItemTypes", "Playlist"),
                    ]),
                "fetch Jellyfin playlists",
            )
            .await?;

        Ok(response
            .items
            .iter()
            .map(|item| Self::item_to_playlist(&base_url, api_key, item))
            .collect())
    }

    async fn get_playlist(
        &self,
        session: &ProviderAuthRequest,
        id: &str,
    ) -> Result<Playlist, ProviderError> {
        let base_url = Self::base_url(session)?;
        let api_key = Self::api_key(session)?;
        let user = self.resolve_user(session, &base_url, api_key).await?;

        let mut all_tracks = Vec::new();
        let limit = 300usize;
        let mut start_index = 0usize;

        for _ in 0..JELLYFIN_MAX_PLAYLIST_PAGES {
            let params = vec![
                ("ParentId".to_string(), id.to_string()),
                (
                    "Fields".to_string(),
                    "AudioInfo,MediaSources,ParentId".to_string(),
                ),
                ("Limit".to_string(), limit.to_string()),
                ("StartIndex".to_string(), start_index.to_string()),
            ];

            let response: JellyfinItemsResponse = self
                .send_json(
                    self.client
                        .get(format!("{}/Users/{}/Items", base_url, user.id))
                        .headers(Self::build_headers(api_key)?)
                        .query(&params),
                    "fetch Jellyfin playlist items",
                )
                .await?;

            let tracks: Vec<Track> = response
                .items
                .iter()
                .filter(|item| item.item_type == "Audio")
                .map(|item| Self::item_to_track(&base_url, api_key, &user.id, item))
                .collect();
            let fetched_count = tracks.len();
            all_tracks.extend(tracks);

            if fetched_count == 0
                || fetched_count < limit
                || all_tracks.len() >= response.total_record_count as usize
            {
                break;
            }
            start_index += limit;
        }

        let metadata_response = self
            .client
            .get(format!("{}/Playlists/{}", base_url, id))
            .headers(Self::build_headers(api_key)?)
            .send()
            .await
            .map_err(|error| {
                ProviderError(format!(
                    "Failed to fetch Jellyfin playlist metadata: {}",
                    error
                ))
            })?;

        if metadata_response.status().is_success() {
            let item = metadata_response
                .json::<JellyfinItem>()
                .await
                .map_err(|error| {
                    ProviderError(format!(
                        "Failed to parse Jellyfin playlist metadata response: {}",
                        error
                    ))
                })?;

            let mut playlist = Self::item_to_playlist(&base_url, api_key, &item);
            playlist.track_count = all_tracks.len();
            playlist.tracks = all_tracks;
            return Ok(playlist);
        }

        Ok(Self::create_fallback_playlist(id, all_tracks))
    }

    async fn get_track(
        &self,
        session: &ProviderAuthRequest,
        id: &str,
    ) -> Result<Track, ProviderError> {
        let base_url = Self::base_url(session)?;
        let api_key = Self::api_key(session)?;
        let user = self.resolve_user(session, &base_url, api_key).await?;

        let params = vec![("Fields".to_string(), "AudioInfo,MediaSources".to_string())];
        let item: JellyfinItem = self
            .send_json(
                self.client
                    .get(format!("{}/Users/{}/Items/{}", base_url, user.id, id))
                    .headers(Self::build_headers(api_key)?)
                    .query(&params),
                "fetch Jellyfin track",
            )
            .await?;

        Ok(Self::item_to_track(&base_url, api_key, &user.id, &item))
    }

    async fn search_tracks(
        &self,
        session: &ProviderAuthRequest,
        query: &str,
    ) -> Result<Vec<Track>, ProviderError> {
        let normalized_query = query.trim();
        if normalized_query.is_empty() {
            return Ok(Vec::new());
        }

        let base_url = Self::base_url(session)?;
        let api_key = Self::api_key(session)?;
        let user = self.resolve_user(session, &base_url, api_key).await?;

        let response: JellyfinItemsResponse = self
            .send_json(
                self.client
                    .get(format!("{}/Users/{}/Items", base_url, user.id))
                    .headers(Self::build_headers(api_key)?)
                    .query(&[
                        ("searchTerm", normalized_query),
                        ("IncludeItemTypes", "Audio"),
                        ("Recursive", "true"),
                        ("Fields", "AudioInfo,MediaSources"),
                    ]),
                "search Jellyfin tracks",
            )
            .await?;

        Ok(response
            .items
            .iter()
            .map(|item| Self::item_to_track(&base_url, api_key, &user.id, item))
            .collect())
    }

    async fn search_playlists(
        &self,
        session: &ProviderAuthRequest,
        query: &str,
    ) -> Result<Vec<Playlist>, ProviderError> {
        let normalized_query = query.trim();
        if normalized_query.is_empty() {
            return Ok(Vec::new());
        }

        let base_url = Self::base_url(session)?;
        let api_key = Self::api_key(session)?;
        let user = self.resolve_user(session, &base_url, api_key).await?;

        let response: JellyfinItemsResponse = self
            .send_json(
                self.client
                    .get(format!("{}/Users/{}/Items", base_url, user.id))
                    .headers(Self::build_headers(api_key)?)
                    .query(&[
                        ("searchTerm", normalized_query),
                        ("IncludeItemTypes", "Playlist"),
                        ("Recursive", "true"),
                    ]),
                "search Jellyfin playlists",
            )
            .await?;

        Ok(response
            .items
            .iter()
            .map(|item| Self::item_to_playlist(&base_url, api_key, item))
            .collect())
    }

    async fn get_stream_url(
        &self,
        session: &ProviderAuthRequest,
        track_id: &str,
    ) -> Result<String, ProviderError> {
        let base_url = Self::base_url(session)?;
        let api_key = Self::api_key(session)?;
        let user = self.resolve_user(session, &base_url, api_key).await?;

        Ok(format!(
            "{}/Audio/{}/universal?UserId={}&Container={}&AudioCodec={}",
            base_url, track_id, user.id, JELLYFIN_STREAM_CONTAINERS, JELLYFIN_STREAM_CODECS
        ))
    }
}
