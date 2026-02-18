use serde::{Deserialize, Serialize};
use std::fmt;

/// Source provider type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Source {
    Spotify,
    Jellyfin,
    Plex,
    Custom,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Spotify => write!(f, "spotify"),
            Source::Jellyfin => write!(f, "jellyfin"),
            Source::Plex => write!(f, "plex"),
            Source::Custom => write!(f, "custom"),
        }
    }
}

/// A music track from any source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Unique ID within the source provider.
    pub id: String,
    /// Display title.
    pub title: String,
    /// Artist name(s).
    pub artist: String,
    /// Album name.
    pub album: String,
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// Cover art URL (if available).
    pub image_url: Option<String>,
    /// Source provider.
    pub source: Source,
    /// External URL or stream URL.
    pub url: Option<String>,
    /// Audio bitrate in kbps (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    /// Audio sample rate in Hz (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate_hz: Option<u32>,
    /// HTTP headers for authentication (e.g., API keys).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_headers: Option<Vec<(String, String)>>,
    /// Whether enrichment has been attempted for this track.
    #[serde(default)]
    pub enriched: bool,
}

impl fmt::Display for Track {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} - {} ({})", self.title, self.artist, self.source)
    }
}

/// A playlist containing multiple tracks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    /// Unique ID within the source provider.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Owner/creator name.
    pub owner: String,
    /// Cover art URL (if available).
    pub image_url: Option<String>,
    /// Number of tracks in the playlist.
    pub track_count: usize,
    /// Tracks in this playlist.
    pub tracks: Vec<Track>,
    /// Source provider.
    pub source: Source,
}

impl fmt::Display for Playlist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} by {} ({})", self.name, self.owner, self.source)
    }
}

/// Playback state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
}

impl fmt::Display for PlaybackState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlaybackState::Playing => write!(f, "Playing"),
            PlaybackState::Paused => write!(f, "Paused"),
            PlaybackState::Stopped => write!(f, "Stopped"),
        }
    }
}

/// Repeat mode for playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepeatMode {
    Off,
    One,
    All,
}

/// Current playback information.
#[derive(Debug, Clone)]
pub struct PlaybackInfo {
    /// Currently playing track (if any).
    pub current_track: Option<Track>,
    /// Current playback state.
    pub state: PlaybackState,
    /// Current position in milliseconds.
    pub position_ms: u64,
    /// Is shuffle enabled.
    pub shuffle: bool,
    /// Repeat mode.
    pub repeat_mode: RepeatMode,
    /// Volume (0-100).
    pub volume: u32,
    /// Queue of tracks.
    pub queue: Vec<Track>,
    /// Current index in queue.
    pub current_index: usize,
    /// Shuffle order: maps shuffle position to original queue index.
    /// When shuffle is enabled, this array defines the play order.
    pub shuffle_order: Vec<usize>,
}

impl Default for PlaybackInfo {
    fn default() -> Self {
        Self {
            current_track: None,
            state: PlaybackState::Stopped,
            position_ms: 0,
            shuffle: false,
            repeat_mode: RepeatMode::Off,
            volume: 50,
            queue: Vec::new(),
            current_index: 0,
            shuffle_order: Vec::new(),
        }
    }
}
