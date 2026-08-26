use super::required_session_param;
use crate::models::{Playlist, Source, Track};
use crate::provider_api::{ProviderApi, ProviderConnectionCheck};
use crate::providers::{ProviderAuthRequest, ProviderError};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;

const SPOTIFY_API_BASE_URL: &str = "https://api.spotify.com/v1";
const SPOTIFY_PLAYLIST_PAGE_SIZE: usize = 50;

fn next_playlist_offset(offset: usize, page_len: usize, total: Option<usize>) -> Option<usize> {
    if page_len == 0 {
        return None;
    }

    let next_offset = offset.checked_add(page_len)?;
    match total {
        Some(total) => (next_offset < total).then_some(next_offset),
        None => (page_len == SPOTIFY_PLAYLIST_PAGE_SIZE).then_some(next_offset),
    }
}

pub struct SpotifyApiClient {
    client: Client,
    api_base_url: String,
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
            api_base_url: SPOTIFY_API_BASE_URL.to_string(),
        }
    }

    pub fn with_client(client: Client) -> Self {
        Self {
            client,
            api_base_url: SPOTIFY_API_BASE_URL.to_string(),
        }
    }

    #[cfg(test)]
    fn with_client_and_base_url(client: Client, api_base_url: String) -> Self {
        Self {
            client,
            api_base_url: api_base_url.trim_end_matches('/').to_string(),
        }
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
        let url = format!("{}/{}", self.api_base_url, endpoint);
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
            .map(|artists| {
                artists
                    .iter()
                    .filter_map(|artist| artist.get("name").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|joined| !joined.is_empty())
            .unwrap_or_else(|| "Unknown Artist".to_string());

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
        let mut playlists = Vec::new();
        let mut offset = 0;

        loop {
            let response = self
                .execute_json(
                    "me/playlists",
                    token,
                    &[
                        ("limit".to_string(), SPOTIFY_PLAYLIST_PAGE_SIZE.to_string()),
                        ("offset".to_string(), offset.to_string()),
                    ],
                )
                .await?;
            let items = response
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let page_len = items.len();
            playlists.extend(items.iter().filter_map(Self::parse_playlist));

            let total = response
                .get("total")
                .and_then(Value::as_u64)
                .and_then(|total| usize::try_from(total).ok());
            let Some(next_offset) = next_playlist_offset(offset, page_len, total) else {
                break;
            };
            offset = next_offset;
        }

        Ok(playlists)
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
        // Fetch playlist metadata first
        let metadata = self
            .execute_json(&format!("playlists/{}", playlist_id), token, &[])
            .await?;

        // Determine per-request page size requested by the caller (provided via session)
        let requested_page_size = session
            .get("page_size")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(100usize)
            .clamp(1, 1_000);

        // Spotify limits per-request page size to 100. Fetch in pages until we
        // reach the provider's total (or receive an empty page).
        let mut all_tracks: Vec<Track> = Vec::new();
        let mut current_offset: usize = 0;
        let per_request_limit: usize = std::cmp::min(requested_page_size, 100);
        let mut total_opt: Option<usize> = None;

        loop {
            let query = vec![
                ("market".to_string(), "from_token".to_string()),
                ("offset".to_string(), current_offset.to_string()),
                ("limit".to_string(), per_request_limit.to_string()),
            ];

            let tracks_response = self
                .execute_json(&format!("playlists/{}/tracks", playlist_id), token, &query)
                .await?;

            let items = tracks_response
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            // Parse track entries (items may contain wrapper objects with "track")
            let mut parsed_count = 0usize;
            for item in &items {
                if let Some(track_val) = item.get("track") {
                    if let Some(track) = Self::parse_track(track_val) {
                        all_tracks.push(track);
                    }
                } else if let Some(track) = Self::parse_track(item) {
                    all_tracks.push(track);
                }
                parsed_count += 1;
            }

            // Capture total reported by Spotify on first page
            if total_opt.is_none() {
                total_opt = tracks_response
                    .get("total")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize);
            }

            // Stop conditions
            if parsed_count == 0 {
                break;
            }
            if let Some(total) = total_opt
                && current_offset + parsed_count >= total
            {
                break;
            }

            // Advance offset by the number of items returned (not parsed count),
            // this aligns with Spotify's paging semantics.
            current_offset += parsed_count;
        }

        let mut playlist = Self::parse_playlist(&metadata).ok_or_else(|| {
            ProviderError("Failed to parse Spotify playlist metadata".to_string())
        })?;

        if let Some(total) = total_opt {
            playlist.track_count = total;
        } else {
            playlist.track_count = all_tracks.len();
        }
        playlist.tracks = all_tracks;
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

#[cfg(test)]
mod tests {
    use super::{SpotifyApiClient, next_playlist_offset};
    use crate::provider_api::ProviderApi;
    use crate::providers::ProviderAuthRequest;
    use reqwest::Client;
    use serde_json::json;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn playlist_page(offset: usize, count: usize, total: usize) -> String {
        let items = (offset..offset + count)
            .map(|index| {
                json!({
                    "id": format!("playlist-{index}"),
                    "name": format!("Playlist {index}"),
                    "owner": { "display_name": "Test owner" },
                    "tracks": { "total": 0 },
                })
            })
            .collect::<Vec<_>>();
        json!({ "items": items, "total": total }).to_string()
    }

    fn start_playlist_test_server(
        responses: Vec<String>,
    ) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base_url = format!(
            "http://{}/v1",
            listener.local_addr().expect("server address")
        );
        let server = thread::spawn(move || {
            responses
                .into_iter()
                .map(|body| {
                    let (mut stream, _) = listener.accept().expect("accept request");
                    let mut request = Vec::new();
                    loop {
                        let mut buffer = [0; 1024];
                        let read = stream.read(&mut buffer).expect("read request");
                        if read == 0 {
                            break;
                        }
                        request.extend_from_slice(&buffer[..read]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let request_line = String::from_utf8_lossy(&request)
                        .lines()
                        .next()
                        .expect("request line")
                        .to_string();
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .expect("write response");
                    request_line
                })
                .collect()
        });
        (base_url, server)
    }

    #[test]
    fn playlist_pagination_advances_until_the_reported_total() {
        assert_eq!(next_playlist_offset(0, 50, Some(120)), Some(50));
        assert_eq!(next_playlist_offset(50, 50, Some(120)), Some(100));
        assert_eq!(next_playlist_offset(100, 10, Some(120)), Some(110));
        assert_eq!(next_playlist_offset(100, 20, Some(120)), None);
    }

    #[test]
    fn playlist_pagination_stops_on_a_short_or_empty_page_without_total() {
        assert_eq!(next_playlist_offset(0, 49, None), None);
        assert_eq!(next_playlist_offset(0, 0, None), None);
        assert_eq!(next_playlist_offset(0, 50, None), Some(50));
    }

    #[test]
    fn get_playlists_fetches_and_aggregates_every_spotify_page() {
        let (base_url, server) = start_playlist_test_server(vec![
            playlist_page(0, 50, 101),
            playlist_page(50, 50, 101),
            playlist_page(100, 1, 101),
        ]);
        let client = SpotifyApiClient::with_client_and_base_url(Client::new(), base_url);
        let session = ProviderAuthRequest::new(HashMap::from([(
            "access_token".to_string(),
            "test-token".to_string(),
        )]));
        let runtime = tokio::runtime::Runtime::new().expect("create runtime");

        let playlists = runtime
            .block_on(client.get_playlists(&session))
            .expect("fetch playlists");

        assert_eq!(playlists.len(), 101);
        assert_eq!(
            playlists.first().map(|playlist| playlist.id.as_str()),
            Some("playlist-0")
        );
        assert_eq!(
            playlists.last().map(|playlist| playlist.id.as_str()),
            Some("playlist-100")
        );

        let paths = server.join().expect("test server should finish");
        assert_eq!(paths.len(), 3);
        for (index, offset) in [0, 50, 100].into_iter().enumerate() {
            assert!(paths[index].starts_with("GET /v1/me/playlists?"));
            assert!(paths[index].contains("limit=50"));
            assert!(paths[index].contains(&format!("offset={offset}")));
        }
    }
}
