use serde::{Deserialize, Serialize};

/// Schema version for exported configuration payloads.
///
/// This value is serialized as part of [`ExportConfigPayload`] and can be
/// used to handle backwards-compatible changes to the export format.
pub const CONFIG_EXPORT_VERSION: u32 = 1;

/// Root payload for exporting and importing application configuration.
///
/// This includes provider-specific configuration as well as any custom
/// playlists created by the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportConfigPayload {
    /// Export format version, typically set to [`CONFIG_EXPORT_VERSION`].
    pub export_version: u32,
    /// Configuration for all supported content providers.
    pub provider_configs: ExportProviderConfigs,
    /// Custom playlists and their associated metadata and tracks.
    pub custom_playlists: Vec<ExportCustomPlaylist>,
}

/// Collection of provider configuration sections included in an export.
///
/// Each field holds the minimal configuration needed to reconnect to or
/// re-identify a provider when importing a configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportProviderConfigs {
    /// Exported configuration for the Spotify provider.
    pub spotify: ExportSpotifyConfig,
    /// Exported configuration for a Jellyfin server.
    pub jellyfin: ExportServerConfig,
    /// Exported configuration for a Plex server.
    pub plex: ExportServerConfig,
}

/// Minimal Spotify configuration needed for export/import.
///
/// This typically contains identifiers and redirect URIs required to
/// perform authentication when restoring a configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSpotifyConfig {
    /// Client ID used to authenticate with Spotify.
    pub client_id: Option<String>,
    /// Redirect URI used during the Spotify authentication flow.
    pub redirect_uri: Option<String>,
}

/// Minimal server configuration shared by Jellyfin and Plex exports.
///
/// This currently captures the base URL required to reach the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportServerConfig {
    /// Base URL of the media server instance.
    pub base_url: Option<String>,
}

/// Export representation of a custom playlist and its content.
///
/// Includes the playlist metadata itself, its tracks, and any union
/// playlist sources that contribute to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportCustomPlaylist {
    /// Metadata for the exported playlist.
    pub playlist: ExportPlaylist,
    /// Tracks that belong to the exported playlist.
    pub tracks: Vec<ExportPlaylistTrack>,
    /// Sources used when the playlist is a union of other playlists.
    pub union_sources: Vec<ExportUnionPlaylistSource>,
}

/// Exported metadata for a single playlist.
///
/// This includes identifying information, timestamps, and type
/// information needed to recreate the playlist on import.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportPlaylist {
    /// Stable identifier for the playlist within the application.
    pub id: String,
    /// Human-readable playlist name.
    pub name: String,
    /// Optional playlist description.
    pub description: Option<String>,
    /// Optional URL to artwork associated with the playlist.
    pub image_url: Option<String>,
    /// Creation timestamp (Unix epoch milliseconds).
    pub created_at: i64,
    /// Last update timestamp (Unix epoch milliseconds).
    pub updated_at: i64,
    /// Number of tracks contained in the playlist.
    pub track_count: i64,
    /// Logical type of the playlist (e.g., manual, union, smart).
    pub playlist_type: String,
}

/// Exported representation of a single playlist track.
///
/// Contains enough information to identify and recreate the track
/// entry when importing the playlist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportPlaylistTrack {
    /// Stable identifier for the playlist track row.
    pub id: i64,
    /// Identifier of the playlist this track belongs to.
    pub playlist_id: String,
    /// Provider or source type for the track (e.g., Spotify, Jellyfin).
    pub track_source: String,
    /// Provider-specific identifier for the track.
    pub track_id: String,
    /// Position of the track within the playlist.
    pub position: i64,
    /// Timestamp when the track was added (Unix epoch milliseconds).
    pub added_at: i64,
    /// Track title.
    pub title: String,
    /// Primary artist name.
    pub artist: String,
    /// Optional album title.
    pub album: Option<String>,
    /// Optional track duration in milliseconds.
    pub duration_ms: Option<i64>,
    /// Optional URL to artwork associated with the track.
    pub image_url: Option<String>,
}

/// Exported representation of a union playlist source.
///
/// Describes a single source playlist that contributes tracks to a
/// union playlist, preserving ordering and creation metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportUnionPlaylistSource {
    /// Stable identifier for the union source row.
    pub id: i64,
    /// Identifier of the union playlist that consumes this source.
    pub union_playlist_id: String,
    /// Type of the source (e.g., another playlist type or provider).
    pub source_type: String,
    /// Identifier of the source playlist.
    pub source_playlist_id: String,
    /// Position of this source within the union playlist definition.
    pub position: i64,
    /// Timestamp when the source was added (Unix epoch milliseconds).
    pub added_at: i64,
}
