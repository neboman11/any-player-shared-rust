use super::required_session_param;
use crate::models::{Playlist, Source, Track};
use crate::provider_api::{ProviderApi, ProviderConnectionCheck};
use crate::providers::{ProviderAuthRequest, ProviderError};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{Level, debug, info};

const PLEX_TYPE_PLAYLIST: &str = "15";
const PLEX_MAX_PLAYLIST_PAGES: usize = 1000;
const PLEX_TCP_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

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
    #[serde(default, rename = "totalSize")]
    total_size: Option<usize>,
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
        // Create a client with conservative connect/request timeouts so
        // unreachable Plex servers fail fast and provide actionable errors
        // to the caller instead of hanging until the overall dispatch
        // timeout.
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(6))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { client }
    }

    pub fn with_client(client: Client) -> Self {
        Self { client }
    }

    fn session_base_url(session: &ProviderAuthRequest) -> Result<String, ProviderError> {
        let url = required_session_param(session, "Plex", "url")?;
        let trimmed = url.trim_end_matches('/');
        let scheme = trimmed.split("://").next().unwrap_or("").to_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(ProviderError(format!(
                "Plex URL must use http or https scheme, got: {}",
                trimmed
            )));
        }
        Ok(trimmed.to_string())
    }

    fn session_token(session: &ProviderAuthRequest) -> Result<&str, ProviderError> {
        required_session_param(session, "Plex", "token")
    }

    fn authed_url(base_url: &str, token: &str, path_with_query: &str) -> String {
        let path = path_with_query.trim_start_matches('/');
        if path.contains('?') {
            format!("{}/{}&X-Plex-Token={}", base_url, path, token)
        } else {
            format!("{}/{}?X-Plex-Token={}", base_url, path, token)
        }
    }

    fn request_url(base_url: &str, path_with_query: &str) -> String {
        let path = path_with_query.trim_start_matches('/');
        format!("{}/{}", base_url, path)
    }

    async fn probe_tcp_connect(url: &str) -> Result<(), ProviderError> {
        if let Ok(parsed) = url::Url::parse(url)
            && let Some(host) = parsed.host_str()
        {
            let port = parsed.port_or_known_default().unwrap_or(80);
            let addrs = tokio::net::lookup_host((host, port))
                .await
                .map_err(|error| {
                    ProviderError(format!(
                        "Failed to resolve Plex server host {}:{}: {}",
                        host, port, error
                    ))
                })?;

            let mut attempted = false;
            let mut last_connect_error: Option<String> = None;

            for addr in addrs {
                attempted = true;
                info!("Plex TCP probe: connecting to {}", addr);
                match timeout(PLEX_TCP_PROBE_TIMEOUT, TcpStream::connect(addr)).await {
                    Err(_) => {
                        last_connect_error = Some(format!(
                            "{}: timed out after {}s",
                            addr,
                            PLEX_TCP_PROBE_TIMEOUT.as_secs()
                        ));
                    }
                    Ok(Ok(_)) => return Ok(()),
                    Ok(Err(error)) => {
                        last_connect_error = Some(format!("{}: {}", addr, error));
                    }
                }
            }

            if !attempted {
                return Err(ProviderError(format!(
                    "No network addresses found for Plex server host {}:{}",
                    host, port
                )));
            }

            if let Some(error) = last_connect_error {
                return Err(ProviderError(format!(
                    "Failed to connect to Plex server at {}:{} ({})",
                    host, port, error
                )));
            }
        }

        Ok(())
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
            .bytes()
            .await
            .map_err(|error| ProviderError(format!("Failed to read {} body: {}", action, error)))?;

        // Log response diagnostics only when debug logging is enabled.
        let len = body.len();
        if tracing::enabled!(Level::DEBUG) {
            debug!("Plex {} response length={} bytes", action, len);
            let excerpt = String::from_utf8_lossy(&body);
            let head: String = excerpt.chars().take(1024).collect();
            debug!("Plex {} response head: {}", action, head);
        }

        serde_json::from_slice::<T>(&body).map_err(|error| {
            ProviderError(format!("Failed to parse {} response: {}", action, error))
        })
    }

    async fn get_tracks_from_endpoint(
        &self,
        base_url: &str,
        token: &str,
        endpoint: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<Track>, usize, Option<usize>), ProviderError> {
        let separator = if endpoint.contains('?') { '&' } else { '?' };
        // Use Plex's correct pagination params: containerStart and containerSize
        let paginated_endpoint = format!(
            "{}{}containerStart={}&containerSize={}",
            endpoint, separator, offset, limit
        );

        info!(
            "Plex get_tracks_from_endpoint: base_url='{}' endpoint='{}' offset={} limit={}",
            base_url, paginated_endpoint, offset, limit
        );

        let url = Self::authed_url(base_url, token, &paginated_endpoint);
        info!(
            "Plex sending HTTP request: {}",
            Self::request_url(base_url, &paginated_endpoint)
        );

        // Quick TCP probe to fail fast when the Plex server is unreachable
        Self::probe_tcp_connect(&url).await?;

        // Wrap the request in a tokio timeout so a stalled connect/read fails
        // quickly instead of blocking until the overall provider dispatch
        // timeout elapses.
        let send_future = self
            .client
            .get(&url)
            .header("Accept", "application/json")
            .send();
        let response = timeout(Duration::from_secs(20), send_future)
            .await
            .map_err(|_| {
                ProviderError(format!(
                    "Plex HTTP request timed out after 20s (url={}, offset={}, limit={})",
                    Self::request_url(base_url, &paginated_endpoint),
                    offset,
                    limit
                ))
            })?
            .map_err(|error| ProviderError(format!("Failed to fetch Plex tracks: {}", error)))?;

        debug!(
            "Plex get_tracks_from_endpoint response: offset={} limit={} status={}",
            offset,
            limit,
            response.status()
        );

        if !response.status().is_success() {
            return Err(ProviderError(format!(
                "Plex track request failed: HTTP {}",
                response.status()
            )));
        }

        let parsed = Self::parse_json_response::<PlexResponse>(response, "Plex track").await?;
        let container = parsed.media_container;
        let total_size = container.total_size;
        let raw_count = container.metadata.len();
        let tracks = container
            .metadata
            .into_iter()
            .filter_map(|item| Self::track_from_metadata(base_url, token, item))
            .collect();

        debug!(
            "Plex parsed page: offset={} limit={} raw_count={} total_size={:?}",
            offset, limit, raw_count, total_size
        );

        Ok((tracks, raw_count, total_size))
    }

    async fn get_all_tracks_from_endpoint(
        &self,
        base_url: &str,
        token: &str,
        endpoint: &str,
        page_size: usize,
    ) -> Result<Vec<Track>, ProviderError> {
        info!("Plex get_all_tracks_from_endpoint: page_size={}", page_size);
        let mut all_tracks = Vec::new();
        let mut offset = 0;
        let mut page_count = 0;
        let mut total_size: Option<usize> = None;

        for _ in 0..PLEX_MAX_PLAYLIST_PAGES {
            page_count += 1;
            debug!(
                "Plex fetching page {}: offset={} page_size={}",
                page_count, offset, page_size
            );
            let (page_tracks, raw_count, page_total) = self
                .get_tracks_from_endpoint(base_url, token, endpoint, offset, page_size)
                .await?;

            // Store the total_size from the first response
            if total_size.is_none() {
                total_size = page_total;
            }

            debug!("Plex page {} returned {} tracks", page_count, raw_count);
            all_tracks.extend(page_tracks);
            offset += raw_count;

            // If the server reported a total, stop once we have collected all items
            if let Some(total) = total_size
                && all_tracks.len() >= total
            {
                debug!("Plex reached total_size={}, stopping pagination", total);
                break;
            }

            // If we got no tracks at all, there's nothing more to fetch
            if raw_count == 0 {
                debug!("Plex got 0 tracks, stopping pagination");
                break;
            }
        }

        info!(
            "Plex get_all_tracks_from_endpoint completed: {} tracks across {} pages",
            all_tracks.len(),
            page_count
        );
        Ok(all_tracks)
    }

    async fn get_playlists_from_endpoint(
        &self,
        base_url: &str,
        token: &str,
        endpoint: &str,
        type_filter: Option<&str>,
    ) -> Result<Vec<Playlist>, ProviderError> {
        // Only append a limit parameter if one is not already present in the endpoint
        let endpoint_with_limit = if endpoint.contains("limit=") {
            endpoint.to_string()
        } else {
            let separator = if endpoint.contains('?') { '&' } else { '?' };
            format!("{}{}limit=50", endpoint, separator)
        };
        let url = Self::authed_url(base_url, token, &endpoint_with_limit);
        info!(
            "Plex sending HTTP request: {}",
            Self::request_url(base_url, &endpoint_with_limit)
        );

        // Quick TCP probe to fail fast when the Plex server is unreachable
        Self::probe_tcp_connect(&url).await?;

        let send_future = self
            .client
            .get(&url)
            .header("Accept", "application/json")
            .send();
        let response = timeout(Duration::from_secs(20), send_future)
            .await
            .map_err(|_| {
                ProviderError(format!(
                    "Plex HTTP request timed out after 20s (url={})",
                    Self::request_url(base_url, &endpoint_with_limit)
                ))
            })?
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
        let endpoint = "identity";
        let url = Self::authed_url(&base_url, token, endpoint);
        info!(
            "Plex validate_connection: requesting {}",
            Self::request_url(&base_url, endpoint)
        );

        // Quick TCP probe to fail fast when the Plex server is unreachable
        if let Err(error) = Self::probe_tcp_connect(&url).await {
            return Ok(ProviderConnectionCheck::Failed(format!(
                "Plex connection test failed: {}",
                error.0
            )));
        }

        let send_future = self
            .client
            .get(&url)
            .header("Accept", "application/json")
            .send();
        let response = timeout(Duration::from_secs(10), send_future)
            .await
            .map_err(|_| {
                ProviderError(
                    "Connection validation timed out after 10 seconds. Provider server is not responding."
                        .to_string(),
                )
            })?
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
        debug!("PlexApiClient::get_playlist called: id={}", id);
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

        // Get page size from session, default to 300, clamped to a reasonable range
        let page_size = session
            .get("page_size")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(300)
            .clamp(1, 1000);

        info!("Plex get_playlist: id={} page_size={}", id, page_size);

        // Fetch all tracks using pagination to avoid truncation and huge single responses
        info!(
            "Plex fetching all tracks for playlist {} with pagination",
            id
        );
        let all_tracks = self
            .get_all_tracks_from_endpoint(
                &base_url,
                token,
                &format!("playlists/{}/items", id),
                page_size,
            )
            .await?;

        debug!(
            "  fetched {} tracks total for playlist {}",
            all_tracks.len(),
            id
        );
        playlist.track_count = all_tracks.len();
        playlist.tracks = all_tracks;
        Ok(playlist)
    }

    async fn get_track(
        &self,
        session: &ProviderAuthRequest,
        id: &str,
    ) -> Result<Track, ProviderError> {
        let base_url = Self::session_base_url(session)?;
        let token = Self::session_token(session)?;
        let (tracks, _, _) = self
            .get_tracks_from_endpoint(&base_url, token, &format!("library/metadata/{}", id), 0, 1)
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
        let (tracks, _, _) = self
            .get_tracks_from_endpoint(
                &base_url,
                token,
                &format!("search?query={}", encoded_query),
                0,
                50,
            )
            .await?;
        Ok(tracks)
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

    async fn get_recently_played(
        &self,
        session: &ProviderAuthRequest,
        limit: usize,
    ) -> Result<Vec<Track>, ProviderError> {
        let base_url = Self::session_base_url(session)?;
        let token = Self::session_token(session)?;

        // Get page size from session, default to 300, clamped to a sane range
        let page_size = session
            .get("page_size")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(300)
            .clamp(1, 1000);

        // For small limits, avoid paginating through the entire recently played list.
        // The endpoint is ordered by recency, so a single page is sufficient.
        let mut tracks = if limit <= page_size {
            let (page_tracks, _, _) = self
                .get_tracks_from_endpoint(
                    &base_url,
                    token,
                    "hubs/home/recentlyPlayed?type=10",
                    0,
                    limit,
                )
                .await?;
            page_tracks
        } else {
            self.get_all_tracks_from_endpoint(
                &base_url,
                token,
                "hubs/home/recentlyPlayed?type=10",
                page_size,
            )
            .await?
        };

        if tracks.len() > limit {
            tracks.truncate(limit);
        }
        Ok(tracks)
    }
}
