use serde::{Deserialize, Serialize};

pub const CONFIG_EXPORT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportConfigPayload {
    pub export_version: u32,
    pub provider_configs: ExportProviderConfigs,
    pub custom_playlists: Vec<ExportCustomPlaylist>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportProviderConfigs {
    pub spotify: ExportSpotifyConfig,
    pub jellyfin: ExportServerConfig,
    pub plex: ExportServerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSpotifyConfig {
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportServerConfig {
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportCustomPlaylist {
    pub playlist: ExportPlaylist,
    pub tracks: Vec<ExportPlaylistTrack>,
    pub union_sources: Vec<ExportUnionPlaylistSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportPlaylist {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub image_url: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub track_count: i64,
    pub playlist_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportPlaylistTrack {
    pub id: i64,
    pub playlist_id: String,
    pub track_source: String,
    pub track_id: String,
    pub position: i64,
    pub added_at: i64,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: Option<i64>,
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportUnionPlaylistSource {
    pub id: i64,
    pub union_playlist_id: String,
    pub source_type: String,
    pub source_playlist_id: String,
    pub position: i64,
    pub added_at: i64,
}
