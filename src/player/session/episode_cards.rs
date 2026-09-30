use super::PlaybackSessionController;
use crate::player::model::episode_card::EpisodeCardVm;

impl PlaybackSessionController {
    pub(in crate::player) fn episode_card_vm(
        &self,
        index: usize,
        resolved_source_id: &str,
    ) -> Option<EpisodeCardVm<'_>> {
        let queue = self.queue.queue();
        let item = queue.items.get(index)?;
        Some(EpisodeCardVm::new(
            item,
            self.episode_file_size(index, resolved_source_id),
            index == queue.current_index,
        ))
    }

    pub(in crate::player) fn episode_file_size(
        &self,
        index: usize,
        resolved_source_id: &str,
    ) -> Option<u64> {
        let item = self.queue.queue().items.get(index)?;
        if index == self.queue.queue().current_index {
            return self
                .source_view()
                .content_length
                .filter(|size| *size > 0)
                .or_else(|| {
                    [
                        resolved_source_id,
                        self.source_view()
                            .track_preference_key
                            .media_source_id
                            .as_str(),
                    ]
                    .into_iter()
                    .find_map(|source_id| {
                        item.media_sources
                            .iter()
                            .find(|source| source.id.as_deref() == Some(source_id))
                            .and_then(|source| source.size)
                            .filter(|size| *size > 0)
                    })
                });
        }
        crate::player::preferred_playback_media_source(&item.media_sources)
            .and_then(|source| source.size)
            .filter(|size| *size > 0)
    }
}
