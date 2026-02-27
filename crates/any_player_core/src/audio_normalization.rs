use std::collections::{HashMap, VecDeque};

pub const INTERNAL_NORMALIZATION_TARGET: u32 = 85;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioNormalizationSource {
    Spotify,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioNormalizationSettings {
    pub enabled: bool,
    pub target: u32,
    pub strict_mode: bool,
}

impl Default for AudioNormalizationSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            target: INTERNAL_NORMALIZATION_TARGET,
            strict_mode: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AdaptiveNormalizationState {
    global_history: VecDeque<f32>,
    source_history: HashMap<String, VecDeque<f32>>,
}

impl Default for AdaptiveNormalizationState {
    fn default() -> Self {
        Self {
            global_history: VecDeque::with_capacity(64),
            source_history: HashMap::new(),
        }
    }
}

impl AdaptiveNormalizationState {
    const MAX_GLOBAL_SAMPLES: usize = 64;
    const MAX_SOURCE_SAMPLES: usize = 24;

    pub fn push_gain(&mut self, source: &str, gain: f32) {
        let clamped_gain = gain.clamp(0.25, 3.0);

        self.global_history.push_back(clamped_gain);
        while self.global_history.len() > Self::MAX_GLOBAL_SAMPLES {
            self.global_history.pop_front();
        }

        let source_history = self
            .source_history
            .entry(source.to_string())
            .or_insert_with(|| VecDeque::with_capacity(Self::MAX_SOURCE_SAMPLES));
        source_history.push_back(clamped_gain);
        while source_history.len() > Self::MAX_SOURCE_SAMPLES {
            source_history.pop_front();
        }
    }

    fn avg_db(history: &VecDeque<f32>) -> Option<f32> {
        if history.is_empty() {
            return None;
        }

        let sum_db: f32 = history.iter().map(|gain| 20.0 * gain.log10()).sum();
        Some(sum_db / (history.len() as f32))
    }

    pub fn strict_compensation_gain(&self, source: &str) -> f32 {
        if self.global_history.len() < 6 {
            return 1.0;
        }

        let global_avg_db = match Self::avg_db(&self.global_history) {
            Some(value) => value,
            None => return 1.0,
        };

        let source_history = self.source_history.get(source);

        if source_history.is_none() {
            return (10.0_f32.powf(global_avg_db / 20.0)).clamp(0.7, 1.3);
        }

        let source_history = source_history.expect("checked above");
        if source_history.len() < 4 {
            return (10.0_f32.powf(global_avg_db / 20.0)).clamp(0.7, 1.3);
        }

        let source_avg_db = match Self::avg_db(source_history) {
            Some(value) => value,
            None => return 1.0,
        };

        let compensation_db = source_avg_db - global_avg_db;
        (10.0_f32.powf(compensation_db / 20.0)).clamp(0.7, 1.3)
    }
}

pub fn clamp_target(target: u32) -> u32 {
    target.min(100)
}

pub fn normalization_target_runtime_factor(target: u32) -> f32 {
    let clamped = clamp_target(target);
    let normalized = (clamped as f32) / 100.0;
    let target_rms = 0.04 + (normalized * 0.18);
    let max_target_rms = 0.04 + 0.18;
    (target_rms / max_target_rms).clamp(0.1, 1.0)
}

pub fn effective_output_volume(
    base_volume: u32,
    source: AudioNormalizationSource,
    settings: &AudioNormalizationSettings,
    strict_gain: f32,
) -> u32 {
    let base = base_volume.min(100);

    if !settings.enabled {
        return base;
    }

    let target = clamp_target(settings.target);
    let source_adjusted = match source {
        AudioNormalizationSource::Spotify => ((base.saturating_mul(target) + 50) / 100).min(100),
        AudioNormalizationSource::Other => {
            let runtime_factor = normalization_target_runtime_factor(target);
            ((base as f32) * runtime_factor).round().clamp(0.0, 100.0) as u32
        }
    };

    ((source_adjusted as f32) * strict_gain)
        .round()
        .clamp(0.0, 100.0) as u32
}

#[cfg(test)]
mod tests {
    use super::{
        AdaptiveNormalizationState, AudioNormalizationSettings, AudioNormalizationSource,
        effective_output_volume, normalization_target_runtime_factor,
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
            strict_mode: false,
        };
        let volume = effective_output_volume(77, AudioNormalizationSource::Spotify, &settings, 1.0);
        assert_eq!(volume, 77);
    }

    #[test]
    fn strict_compensation_uses_history() {
        let mut state = AdaptiveNormalizationState::default();
        for _ in 0..10 {
            state.push_gain("spotify", 0.9);
            state.push_gain("jellyfin", 1.2);
        }

        let gain = state.strict_compensation_gain("spotify");
        assert!((0.7..=1.3).contains(&gain));
        assert!(gain > 0.0);
    }
}
