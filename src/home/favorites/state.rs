use std::ops::{Index, IndexMut};

use gpui::ScrollHandle;

use crate::emby::{UserItem, VideoItemType};

use super::super::{carousel::CarouselState, paged_items::PagedItemsState};

pub(crate) const FAVORITE_ITEM_TYPES: [VideoItemType; 3] = [
    VideoItemType::Movie,
    VideoItemType::Series,
    VideoItemType::Episode,
];
pub(crate) const FAVORITES_PAGE_LIMIT: u32 = 30;

pub(crate) fn favorite_section_title(item_type: VideoItemType) -> &'static str {
    match item_type {
        VideoItemType::Movie => "电影",
        VideoItemType::Series => "剧集",
        VideoItemType::Episode => "集",
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FavoriteSection {
    pub(crate) paged: PagedItemsState,
    pub(crate) carousel: CarouselState,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FavoritesState {
    sections: [FavoriteSection; 3],
    pub(crate) scroll_handle: ScrollHandle,
}

impl Index<VideoItemType> for FavoritesState {
    type Output = FavoriteSection;

    fn index(&self, item_type: VideoItemType) -> &Self::Output {
        &self.sections[section_index(item_type)]
    }
}

impl IndexMut<VideoItemType> for FavoritesState {
    fn index_mut(&mut self, item_type: VideoItemType) -> &mut Self::Output {
        &mut self.sections[section_index(item_type)]
    }
}

fn section_index(item_type: VideoItemType) -> usize {
    match item_type {
        VideoItemType::Movie => 0,
        VideoItemType::Series => 1,
        VideoItemType::Episode => 2,
    }
}

impl FavoritesState {
    pub(crate) fn items(&self) -> impl Iterator<Item = &UserItem> {
        self.sections
            .iter()
            .flat_map(|section| &section.paged.items)
    }

    pub(crate) fn has_items(&self) -> bool {
        self.items().next().is_some()
    }

    pub(crate) fn mark_dirty(&mut self) {
        for section in &mut self.sections {
            section.paged.mark_dirty();
        }
    }

    pub(crate) fn sync_previous_offsets(&mut self) {
        for section in &mut self.sections {
            section.carousel.sync_previous_offset();
        }
    }

    pub(crate) fn remove_item(
        &mut self,
        item_id: &str,
    ) -> Option<(VideoItemType, usize, UserItem)> {
        FAVORITE_ITEM_TYPES.into_iter().find_map(|item_type| {
            self[item_type]
                .paged
                .remove_item(item_id)
                .map(|(index, item)| (item_type, index, item))
        })
    }

    pub(crate) fn restore_item(&mut self, item_type: VideoItemType, index: usize, item: UserItem) {
        self[item_type].paged.restore_item(index, item);
    }
}
