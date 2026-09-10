use crate::player::PlaybackCacheConfig;

const MIB: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MemoryBudget {
    Mib128,
    Mib256,
    Mib512,
    Gib1,
    Gib2,
}

impl MemoryBudget {
    pub(super) const ALL: [Self; 5] = [
        Self::Mib128,
        Self::Mib256,
        Self::Mib512,
        Self::Gib1,
        Self::Gib2,
    ];

    pub(super) fn id(self) -> &'static str {
        match self {
            Self::Mib128 => "memory-budget-128-mib",
            Self::Mib256 => "memory-budget-256-mib",
            Self::Mib512 => "memory-budget-512-mib",
            Self::Gib1 => "memory-budget-1-gib",
            Self::Gib2 => "memory-budget-2-gib",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Mib128 => "128 MiB",
            Self::Mib256 => "256 MiB",
            Self::Mib512 => "512 MiB",
            Self::Gib1 => "1 GiB",
            Self::Gib2 => "2 GiB",
        }
    }

    fn bytes(self) -> u64 {
        match self {
            Self::Mib128 => 128 * MIB,
            Self::Mib256 => 256 * MIB,
            Self::Mib512 => 512 * MIB,
            Self::Gib1 => 1024 * MIB,
            Self::Gib2 => 2048 * MIB,
        }
    }

    pub(super) fn current(config: &PlaybackCacheConfig) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|budget| budget.bytes() == config.total_cache_max_bytes)
    }

    pub(super) fn apply(self, config: &PlaybackCacheConfig) -> PlaybackCacheConfig {
        let mut result = config.clone();
        if config.total_cache_max_bytes == self.bytes() {
            return result;
        }
        // A capacity choice only changes byte budgets. Preserve the user's
        // refill policy, storage preferences and opt-in decoder dropping.
        let defaults = PlaybackCacheConfig::default();
        let scale = |bytes| bytes * self.bytes() / defaults.total_cache_max_bytes;
        result.total_cache_max_bytes = self.bytes();
        result.http_cache_max_bytes = scale(defaults.http_cache_max_bytes);
        result.demuxer_max_bytes = scale(defaults.demuxer_max_bytes);
        result.demuxer_max_back_bytes = scale(defaults.demuxer_max_back_bytes);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::{CacheUnlinkPolicy, PlaybackCacheMode};

    #[test]
    fn capacity_options_keep_forward_priority_within_the_shared_budget() {
        for (budget, mib) in MemoryBudget::ALL
            .into_iter()
            .zip([128, 256, 512, 1024, 2048])
        {
            let config = budget.apply(&PlaybackCacheConfig::default());
            assert_eq!(config.total_cache_max_bytes, mib * MIB);
            let (http, forward, back) = config.effective_cache_budgets();
            assert_eq!(forward, 3 * back);
            assert!(http + forward + back <= config.total_cache_max_bytes);
            assert_eq!(MemoryBudget::current(&config), Some(budget));
            assert_eq!(config.clone().normalized(), config);
            assert!(!config.decoder_framedrop);
        }
    }

    #[test]
    fn capacity_changes_preserve_timing_disk_and_decoder_preferences() {
        let config = PlaybackCacheConfig {
            mode: PlaybackCacheMode::Disabled,
            cache_pause: false,
            cache_secs: 7.0,
            demuxer_packet_max_readahead_secs: 2.0,
            total_cache_max_bytes: 192 * MIB,
            disk_cache: true,
            disk_cache_max_bytes: 3 * 1024 * MIB + 123,
            cache_dir: Some("/custom/cache".into()),
            unlink_files: CacheUnlinkPolicy::Never,
            decoder_framedrop: true,
            ..PlaybackCacheConfig::default()
        };
        assert_eq!(MemoryBudget::current(&config), None);
        for budget in MemoryBudget::ALL {
            let updated = budget.apply(&config);
            let mut expected = config.clone();
            expected.total_cache_max_bytes = updated.total_cache_max_bytes;
            expected.http_cache_max_bytes = updated.http_cache_max_bytes;
            expected.demuxer_max_bytes = updated.demuxer_max_bytes;
            expected.demuxer_max_back_bytes = updated.demuxer_max_back_bytes;
            assert_eq!(updated, expected);
        }
    }

    #[test]
    fn reselecting_the_current_capacity_preserves_custom_layer_budgets() {
        let config = PlaybackCacheConfig {
            http_cache_max_bytes: 33 * MIB + 123,
            demuxer_max_bytes: 100 * MIB,
            demuxer_max_back_bytes: 0,
            ..PlaybackCacheConfig::default()
        };
        assert_eq!(MemoryBudget::current(&config), Some(MemoryBudget::Mib256));
        assert_eq!(MemoryBudget::Mib256.apply(&config), config);
    }

    #[test]
    fn legacy_independent_budgets_remain_custom_until_capacity_is_selected() {
        let config = PlaybackCacheConfig {
            total_cache_max_bytes: 0,
            ..PlaybackCacheConfig::default()
        };
        assert_eq!(MemoryBudget::current(&config), None);
        let updated = MemoryBudget::Mib256.apply(&config);
        assert_eq!(updated, PlaybackCacheConfig::default());
    }
}
