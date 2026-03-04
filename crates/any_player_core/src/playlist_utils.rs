//! Dedup contract types and algorithm for Any Player playlists.
//!
//! These types define the shared dedup output shape used by downstream plans.
//! Algorithm: case-insensitive + trimmed title+artist comparison.

use crate::models::Track;
use serde::{Deserialize, Serialize};

/// Returns the normalized dedup key for a title+artist pair as a tuple.
///
/// Returns `(lowercase_trimmed_title, lowercase_trimmed_artist)`.
/// Using a tuple avoids false collisions that would arise from embedding both
/// strings into a single delimited string (e.g. when a title or artist
/// contains the delimiter character).
pub fn duplicate_key(title: &str, artist: &str) -> (String, String) {
    (title.trim().to_lowercase(), artist.trim().to_lowercase())
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
    /// Normalized dedup key as `(lowercase_trimmed_title, lowercase_trimmed_artist)`.
    pub key: (String, String),
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
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    let mut result_tracks: Vec<Track> = Vec::new();
    let mut duplicate_groups: Vec<DuplicateGroup> = Vec::new();
    // key -> index in duplicate_groups vec
    let mut group_index: HashMap<(String, String), usize> = HashMap::new();

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Source;
    use serde::Deserialize;

    // ---------------------------------------------------------------------------
    // Fixture deserialization helpers
    // ---------------------------------------------------------------------------

    /// Minimal track descriptor as it appears in `dedup_spec.json` input arrays.
    #[derive(Debug, Deserialize)]
    struct FixtureTrack {
        id: String,
        title: String,
        artist: String,
    }

    /// Expected duplicate group as serialised in the fixture.
    #[derive(Debug, Deserialize)]
    struct ExpectedGroup {
        key: (String, String),
        first_occurrence_id: String,
        first_occurrence_index: usize,
        duplicate_ids: Vec<String>,
    }

    /// One test case from the shared fixture.
    #[derive(Debug, Deserialize)]
    struct TestCase {
        id: String,
        description: String,
        input: Vec<FixtureTrack>,
        expected_deduped_count: usize,
        expected_duplicate_groups: Vec<ExpectedGroup>,
    }

    /// Top-level fixture document.
    #[derive(Debug, Deserialize)]
    struct FixtureSpec {
        test_cases: Vec<TestCase>,
    }

    /// Build a full `Track` from a fixture row, filling required fields with
    /// neutral defaults (empty album, 0 duration, Custom source).
    fn make_track(ft: &FixtureTrack) -> Track {
        Track {
            id: ft.id.clone(),
            title: ft.title.clone(),
            artist: ft.artist.clone(),
            album: String::new(),
            duration_ms: 0,
            image_url: None,
            source: Source::Custom,
            url: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            auth_headers: None,
            enriched: false,
        }
    }

    // ---------------------------------------------------------------------------
    // Fixture-driven regression suite
    // ---------------------------------------------------------------------------

    /// Runs every case in `test-fixtures/dedup_spec.json` and asserts:
    ///   - deduped track count matches `expected_deduped_count`
    ///   - number of duplicate groups matches
    ///   - each expected group is found by key, with correct `first_occurrence_index`
    ///   - the kept track (first_occurrence_id) is present in `deduped_tracks`
    ///   - each expected duplicate ID appears in the group's occurrences
    #[test]
    fn fixture_all_cases() {
        let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-fixtures")
            .join("dedup_spec.json");
        let fixture_json =
            std::fs::read_to_string(&fixture_path).expect("Failed to read dedup_spec.json fixture");
        let spec: FixtureSpec =
            serde_json::from_str(&fixture_json).expect("Failed to parse dedup_spec.json");

        for case in &spec.test_cases {
            println!("\n[{}] {}", case.id, case.description);

            let tracks: Vec<Track> = case.input.iter().map(make_track).collect();
            let result = deduplicate_tracks(&tracks);

            // --- deduped count ---
            assert_eq!(
                result.tracks.len(),
                case.expected_deduped_count,
                "[{}] deduped track count mismatch",
                case.id
            );

            // --- group count ---
            assert_eq!(
                result.duplicate_groups.len(),
                case.expected_duplicate_groups.len(),
                "[{}] duplicate group count mismatch",
                case.id
            );

            // --- per-group assertions ---
            for expected in &case.expected_duplicate_groups {
                let actual = result
                    .duplicate_groups
                    .iter()
                    .find(|g| g.key == expected.key)
                    .unwrap_or_else(|| {
                        panic!(
                            "[{}] expected group with key {:?} not found in result",
                            case.id, expected.key
                        )
                    });

                // first_occurrence_index
                assert_eq!(
                    actual.first_occurrence_index, expected.first_occurrence_index,
                    "[{}] first_occurrence_index mismatch for key {:?}",
                    case.id, expected.key
                );

                // first occurrence track must be in deduped list
                assert!(
                    result
                        .tracks
                        .iter()
                        .any(|t| t.id == expected.first_occurrence_id),
                    "[{}] first occurrence '{}' not found in deduped tracks",
                    case.id,
                    expected.first_occurrence_id
                );

                // duplicate occurrence ids
                assert_eq!(
                    actual.occurrences.len(),
                    expected.duplicate_ids.len(),
                    "[{}] occurrence count mismatch for key {:?}",
                    case.id,
                    expected.key
                );

                for dup_id in &expected.duplicate_ids {
                    assert!(
                        actual.occurrences.iter().any(|o| &o.track_id == dup_id),
                        "[{}] expected duplicate id '{}' not found in occurrences for key {:?}",
                        case.id,
                        dup_id,
                        expected.key
                    );
                }
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Focused edge-case tests (supplement the fixture)
    // ---------------------------------------------------------------------------

    #[test]
    fn duplicate_key_empty_artist() {
        // Empty artist is a valid key component — must not panic or produce wrong key
        let (title_key, artist_key) = duplicate_key("Unknown Track", "");
        assert_eq!(title_key, "unknown track");
        assert_eq!(artist_key, "");
    }

    #[test]
    fn duplicate_key_lowercase_unicode() {
        // Rust's .to_lowercase() handles Unicode; ü stays ü, CJK is unchanged
        let (title_key, artist_key) = duplicate_key("Für Elise", "Beethoven");
        assert_eq!(title_key, "für elise");
        assert_eq!(artist_key, "beethoven");

        let (title_key_cjk, artist_key_cjk) = duplicate_key("東京の夜", "坂本龍一");
        assert_eq!(title_key_cjk, "東京の夜");
        assert_eq!(artist_key_cjk, "坂本龍一");
    }

    #[test]
    fn duplicate_key_whitespace_and_tabs() {
        let k1 = duplicate_key("  Song  ", "  Artist  ");
        let k2 = duplicate_key("\tSong\t", "\tArtist\t");
        assert_eq!(k1, k2, "space-trimmed and tab-trimmed keys must be equal");
        assert_eq!(k1, ("song".to_string(), "artist".to_string()));
    }

    #[test]
    fn deduplicate_tracks_preserves_first_occurrence_order() {
        let tracks: Vec<Track> = vec![
            make_track(&FixtureTrack {
                id: "a".into(),
                title: "Z Song".into(),
                artist: "B".into(),
            }),
            make_track(&FixtureTrack {
                id: "b".into(),
                title: "A Song".into(),
                artist: "B".into(),
            }),
            make_track(&FixtureTrack {
                id: "c".into(),
                title: "Z Song".into(),
                artist: "B".into(),
            }),
        ];
        let result = deduplicate_tracks(&tracks);
        assert_eq!(result.tracks.len(), 2);
        assert_eq!(
            result.tracks[0].id, "a",
            "first occurrence of Z Song must be kept"
        );
        assert_eq!(
            result.tracks[1].id, "b",
            "A Song must follow in original order"
        );
        assert_eq!(result.duplicate_groups.len(), 1);
        assert_eq!(result.duplicate_groups[0].first_occurrence_index, 0);
    }
}
