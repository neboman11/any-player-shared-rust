/// Internal loudness target used when normalizing audio, expressed as a
/// percentage (0–100). Callers may not override this value at runtime.
pub const INTERNAL_NORMALIZATION_TARGET: u32 = 85;

/// Identifies the content source for audio normalization calculations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioNormalizationSource {
    /// Audio from Spotify.
    Spotify,
    /// Audio from any other provider (e.g. Jellyfin, Plex).
    Other,
}

/// User-facing settings that control the audio normalization pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioNormalizationSettings {
    /// Whether audio normalization is active.
    pub enabled: bool,
    /// Target loudness level (0–100). Clamped to 100 at runtime.
    pub target: u32,
}

impl Default for AudioNormalizationSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            target: INTERNAL_NORMALIZATION_TARGET,
        }
    }
}

/// Clamps `target` to the valid loudness range (0–100).
pub fn clamp_target(target: u32) -> u32 {
    target.min(100)
}

/// Converts a loudness target percentage into a linear RMS scale factor.
///
/// The factor is always in `[0.1, 1.0]`, with higher targets producing values
/// closer to 1.0 (full volume).
pub fn normalization_target_runtime_factor(target: u32) -> f32 {
    let clamped = clamp_target(target);
    let normalized = (clamped as f32) / 100.0;
    let target_rms = 0.04 + (normalized * 0.18);
    let max_target_rms = 0.04 + 0.18;
    (target_rms / max_target_rms).clamp(0.1, 1.0)
}

/// Computes effective output volume after applying normalization.
///
/// When normalization is disabled, `base_volume` is returned unchanged.
/// Otherwise the volume is scaled by the target factor appropriate to the
/// content source. The returned value is always in `[0, 100]`.
pub fn effective_output_volume(
    base_volume: u32,
    source: AudioNormalizationSource,
    settings: &AudioNormalizationSettings,
) -> u32 {
    let base = base_volume.min(100);

    if !settings.enabled {
        return base;
    }

    let target = clamp_target(settings.target);
    match source {
        AudioNormalizationSource::Spotify => ((base.saturating_mul(target) + 50) / 100).min(100),
        AudioNormalizationSource::Other => {
            let runtime_factor = normalization_target_runtime_factor(target);
            ((base as f32) * runtime_factor).round().clamp(0.0, 100.0) as u32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AudioNormalizationSettings, AudioNormalizationSource, effective_output_volume,
        normalization_target_runtime_factor,
    };

    #[test]
    fn runtime_factor_is_bounded() {
        let low = normalization_target_runtime_factor(0);
        let high = normalization_target_runtime_factor(100);

        assert!(low >= 0.1);
        assert!(high <= 1.0);
        assert!(high > low);
    }

    #[test]
    fn disabled_normalization_keeps_base_volume() {
        let settings = AudioNormalizationSettings {
            enabled: false,
            target: 25,
        };

        let volume = effective_output_volume(77, AudioNormalizationSource::Spotify, &settings);
        assert_eq!(volume, 77);
    }
}
