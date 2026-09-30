use super::{
    SettingValidation,
    model::*,
    values::{BYTES_PER_MIB, parse_seconds},
};
use crate::{
    player::{PlaybackCacheConfig, PlaybackLanguagePreferences},
    theme::ColorTheme,
};
use std::collections::HashSet;

/// The one editable settings snapshot for the open window. Edits enter through
/// intents. Editors/dropdowns/scrolling belong to the chosen GPUI presentation;
/// closing ends edits without rolling back changes already sent to persistence.
pub(crate) struct SettingsController {
    snapshot: SettingsSnapshot,
    mode: SettingsMode,
    category: SettingsCategory,
    query: String,
    invalid_fields: HashSet<NumericSetting>,
    closed: bool,
}

pub(crate) struct SettingsVm<'a> {
    pub(crate) config: &'a PlaybackCacheConfig,
    pub(crate) color_theme: ColorTheme,
    pub(crate) track_languages: PlaybackLanguagePreferences,
    pub(crate) category: SettingsCategory,
    pub(crate) query: &'a str,
    pub(crate) editable: bool,
    invalid_fields: &'a HashSet<NumericSetting>,
}

impl SettingsVm<'_> {
    /// A rejected draft stays in the editor; the saved value above remains the
    /// last accepted value. The view model exposes validation without copying
    /// drafts into the persisted snapshot or introducing new error UI.
    pub(crate) fn validation(&self, field: NumericSetting) -> SettingValidation {
        if self.invalid_fields.contains(&field) {
            SettingValidation::Invalid
        } else {
            SettingValidation::Valid
        }
    }

    pub(crate) fn includes(&self, item: &SettingDescriptor) -> bool {
        if self.query.is_empty() {
            item.category == self.category
        } else {
            super::values::matches_search(
                self.query,
                &[
                    item.category.title(),
                    item.section,
                    item.title,
                    item.description,
                    item.keywords,
                ],
            )
        }
    }
}

impl SettingsController {
    pub(crate) fn new(
        config: &PlaybackCacheConfig,
        mode: SettingsMode,
        color_theme: ColorTheme,
        track_languages: PlaybackLanguagePreferences,
    ) -> Self {
        Self {
            snapshot: SettingsSnapshot {
                playback: config.clone().normalized(),
                color_theme,
                track_languages,
            },
            mode,
            category: SettingsCategory::default(),
            query: String::new(),
            invalid_fields: HashSet::new(),
            closed: false,
        }
    }

    pub(crate) fn view_model(&self) -> SettingsVm<'_> {
        SettingsVm {
            config: &self.snapshot.playback,
            color_theme: self.snapshot.color_theme,
            track_languages: self.snapshot.track_languages,
            category: self.category,
            query: &self.query,
            editable: !self.closed,
            invalid_fields: &self.invalid_fields,
        }
    }

    pub(crate) fn snapshot(&self) -> SettingsSnapshot {
        self.snapshot.clone()
    }

    /// Theme application is synchronous, but can resolve to the built-in
    /// fallback. Persist what the adapter actually applied, as before.
    pub(crate) fn finish_theme_selection(&mut self, applied: ColorTheme) {
        if !self.closed {
            self.snapshot.color_theme = applied;
        }
    }

    pub(crate) fn dispatch(&mut self, intent: SettingsIntent) -> SettingsChange {
        if !self.view_model().editable {
            return SettingsChange::default();
        }
        match intent {
            SettingsIntent::Close => {
                self.closed = true;
                return SettingsChange::default();
            }
            SettingsIntent::Search(query) => {
                let query = query.trim().to_owned();
                let changed = query != self.query;
                self.query = query;
                return SettingsChange {
                    view_changed: changed,
                    ..Default::default()
                };
            }
            SettingsIntent::Category(category) => {
                if self.mode == SettingsMode::User
                    && matches!(
                        category,
                        SettingsCategory::General | SettingsCategory::Readahead
                    )
                {
                    return SettingsChange::default();
                }
                let changed = self.category != category || !self.query.is_empty();
                self.category = category;
                self.query.clear();
                return SettingsChange {
                    view_changed: changed,
                    ..Default::default()
                };
            }
            _ => {}
        }
        let previous = self.snapshot.clone();
        let previous_validation = match &intent {
            SettingsIntent::EditNumber { field, .. } => {
                Some((*field, self.view_model().validation(*field)))
            }
            _ => None,
        };
        let config = &mut self.snapshot.playback;
        match intent {
            SettingsIntent::Theme(theme) => self.snapshot.color_theme = theme,
            SettingsIntent::Language { language, audio } => {
                if audio {
                    self.snapshot.track_languages.audio = language
                } else {
                    self.snapshot.track_languages.subtitle = language
                }
            }
            SettingsIntent::HardwareDecode(mode) => config.hardware_decode = mode,
            SettingsIntent::MemoryBudget(Some(budget)) => *config = budget.apply(config),
            SettingsIntent::MemoryBudget(None) => {}
            SettingsIntent::Mode(mode) => config.mode = mode,
            SettingsIntent::SeekableCache(mode) => config.seekable_cache = mode,
            SettingsIntent::UnlinkFiles(policy) => config.unlink_files = policy,
            SettingsIntent::Toggle(setting) => {
                let value = match setting {
                    ToggleSetting::DecoderFramedrop => &mut config.decoder_framedrop,
                    ToggleSetting::DiskCache => &mut config.disk_cache,
                    ToggleSetting::CachePause => &mut config.cache_pause,
                    ToggleSetting::CachePauseInitial => &mut config.cache_pause_initial,
                    ToggleSetting::DonateBuffer => &mut config.demuxer_donate_buffer,
                    ToggleSetting::AdaptiveReadahead => &mut config.adaptive_readahead,
                    ToggleSetting::AutomaticHysteresis => &mut config.automatic_hysteresis,
                    ToggleSetting::DemuxerCacheWait => &mut config.demuxer_cache_wait,
                };
                *value = !*value;
            }
            SettingsIntent::EditNumber { field, input } => {
                if let Some(next) = field.edited(config, &input) {
                    *config = next;
                    self.invalid_fields.remove(&field);
                } else {
                    self.invalid_fields.insert(field);
                }
            }
            SettingsIntent::Category(_) | SettingsIntent::Search(_) | SettingsIntent::Close => {
                unreachable!()
            }
        }
        let changed = self.snapshot != previous;
        // Validation is a visible model transition even when the accepted
        // configuration is unchanged. Repeated invalid edits do not request
        // another parent render; the editor itself owns its text repaint.
        let validation_changed = previous_validation
            .is_some_and(|(field, previous)| self.view_model().validation(field) != previous);
        SettingsChange {
            persist: changed,
            view_changed: changed || validation_changed,
            theme: (self.snapshot.color_theme != previous.color_theme)
                .then_some(self.snapshot.color_theme),
            languages: (self.snapshot.track_languages != previous.track_languages)
                .then_some(self.snapshot.track_languages),
        }
    }
}

impl NumericSetting {
    fn edited(self, config: &PlaybackCacheConfig, input: &str) -> Option<PlaybackCacheConfig> {
        let mut next = config.clone();
        match self {
            Self::CacheSecs
            | Self::ReadaheadSecs
            | Self::PacketReadaheadSecs
            | Self::HysteresisSecs
            | Self::CachePauseWaitSecs => {
                let value = parse_seconds(input)?;
                match self {
                    Self::CacheSecs => next.cache_secs = value,
                    Self::ReadaheadSecs => next.demuxer_readahead_secs = value,
                    Self::PacketReadaheadSecs => next.demuxer_packet_max_readahead_secs = value,
                    Self::HysteresisSecs => next.demuxer_hysteresis_secs = value,
                    Self::CachePauseWaitSecs => next.cache_pause_wait = value,
                    _ => unreachable!(),
                }
                Some(next.normalized())
            }
            Self::DiskCacheGib => {
                next.disk_cache_max_bytes = input
                    .trim()
                    .parse::<u64>()
                    .ok()?
                    .checked_mul(1024 * BYTES_PER_MIB)
                    .filter(|bytes| *bytes > 0)?;
                Some(next)
            }
            _ => {
                let value = input.trim().parse::<u64>().ok()?;
                self.set_integer(&mut next, value);
                let normalized = next.normalized();
                let mut requested = normalized.clone();
                self.set_integer(&mut requested, value);
                // Dependent fields may normalize, but the edited value may not
                // silently turn into a different value on persistence.
                (requested == normalized).then_some(normalized)
            }
        }
    }

    fn set_integer(self, config: &mut PlaybackCacheConfig, value: u64) {
        let bytes = value.saturating_mul(BYTES_PER_MIB);
        match self {
            Self::TotalCacheMib => config.total_cache_max_bytes = bytes,
            Self::HttpCacheMib => config.http_cache_max_bytes = bytes,
            Self::HttpCacheChunkMib => config.http_cache_chunk_bytes = bytes,
            Self::DemuxerForwardMib => config.demuxer_max_bytes = bytes,
            Self::DemuxerBackMib => config.demuxer_max_back_bytes = bytes,
            Self::RangeRequestMib => config.http_cache_range_request_bytes = bytes,
            Self::MaxRanges => {
                config.demuxer_max_ranges = usize::try_from(value).unwrap_or(usize::MAX)
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
