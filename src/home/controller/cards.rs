use super::HomeController;
use crate::{
    emby::{MediaItem, ResumeItem, UserItem},
    home::model::cards::{EpisodeCardVm, ResumeCardVm, UserEpisodeCardVm, UserItemCardVm},
};

impl HomeController {
    pub(in crate::home) fn resume_card_vm(&self, item: &ResumeItem) -> ResumeCardVm {
        ResumeCardVm::new(
            item,
            self.effective_user_data(&item.id, item.user_data.as_ref()),
        )
    }

    pub(in crate::home) fn user_item_card_vm(
        &self,
        item: &UserItem,
        show_favorite: bool,
    ) -> UserItemCardVm {
        UserItemCardVm::new(
            item,
            self.effective_user_data(&item.id, item.user_data.as_ref()),
            show_favorite,
        )
    }

    pub(in crate::home) fn user_episode_card_vm(&self, item: &UserItem) -> UserEpisodeCardVm {
        UserEpisodeCardVm::new(
            item,
            self.effective_user_data(&item.id, item.user_data.as_ref()),
        )
    }

    pub(in crate::home) fn episode_card_vm(
        &self,
        item: &MediaItem,
        selected: bool,
    ) -> EpisodeCardVm {
        EpisodeCardVm::new(
            item,
            self.effective_user_data(&item.id, item.user_data.as_ref()),
            selected,
        )
    }
}
