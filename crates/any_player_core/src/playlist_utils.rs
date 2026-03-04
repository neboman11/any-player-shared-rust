//! Dedup contract types and algorithm for Any Player playlists.
//!
//! These types define the shared dedup output shape used by downstream plans.
//! Algorithm: case-insensitive + trimmed title+artist comparison.

use crate::models::Track;
use serde::{Deserialize, Serialize};

/// Returns the normalized dedup key for a title+artist pair.
///
/// Format: `"{lowercase_trimmed_title}|{lowercase_trimmed_artist}"`
pub fn duplicate_key(title: &str, artist: &str) -> String {
    format!(
        "{}|{}",
        title.trim().to_lowercase(),
        artist.trim().to_lowercase()
    )
}

/// A single occurrence of a duplicate track within the original input list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DuplicateOccurrence {
    /// Zero-based index of this track in the original input slice.
    pub index: usize,
    /// Track ID of this occurrence.
    pub track_id: String,
}

/// A group of tracks that share the same normalized dedup key.
///
/// The first occurrence is kept in `DeduplicateResult::tracks`; all subsequent
/// occurrences are recorded here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DuplicateGroup {
    /// Normalized dedup key: `"{title_lower_trim}|{artist_lower_trim}"`.
    pub key: String,
    /// Zero-based index of the first (kept) occurrence in the original input.
    pub first_occurrence_index: usize,
    /// All duplicate occurrences (does NOT include the first occurrence).
    pub occurrences: Vec<DuplicateOccurrence>,
}

/// Result of running deduplication over a track list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeduplicateResult {
    /// Tracks to keep: first occurrence of each unique title+artist key, in original order.
    pub tracks: Vec<Track>,
    /// Groups of duplicates found. Empty when no duplicates exist.
    pub duplicate_groups: Vec<DuplicateGroup>,
}

/// Deduplicate a slice of tracks by title+artist (case-insensitive, whitespace-trimmed).
///
/// Returns the first occurrence of each unique title+artist combination.
/// The relative order of kept tracks mirrors the original input order.
pub fn deduplicate_tracks(tracks: &[Track]) -> DeduplicateResult {
    use std::collections::HashMap;

    // key -> first_occurrence_index in original slice
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut result_tracks: Vec<Track> = Vec::new();
    let mut duplicate_groups: Vec<DuplicateGroup> = Vec::new();
    // key -> index in duplicate_groups vec
    let mut group_index: HashMap<String, usize> = HashMap::new();

    for (i, track) in tracks.iter().enumerate() {
        let key = duplicate_key(&track.title, &track.artist);

        if let Some(&first_idx) = seen.get(&key) {
            let occurrence = DuplicateOccurrence {
                index: i,
                track_id: track.id.clone(),
            };

            if let Some(&gi) = group_index.get(&key) {
                // Group already exists — append this occurrence.
                duplicate_groups[gi].occurrences.push(occurrence);
            } else {
                // First duplicate of this key — create a new group.
                let gi = duplicate_groups.len();
                duplicate_groups.push(DuplicateGroup {
                    key: key.clone(),
                    first_occurrence_index: first_idx,
                    occurrences: vec![occurrence],
                });
                group_index.insert(key, gi);
            }
        } else {
            seen.insert(key, i);
            result_tracks.push(track.clone());
        }
    }

    DeduplicateResult {
        tracks: result_tracks,
        duplicate_groups,
    }
}
