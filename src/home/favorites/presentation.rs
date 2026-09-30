use std::ops::{Index, IndexMut};

use gpui::ScrollHandle;

use crate::emby::VideoItemType;

use super::super::carousel::CarouselState;
use super::controller::{FAVORITE_ITEM_TYPES, section_index};

#[derive(Debug)]
pub(crate) struct FavoriteSection {
    // One continuation per category; replacement/release cancels delivery.
    pub(crate) effect: crate::effects::EffectHandle<gpui::Task<()>>,
    pub(crate) presentation: super::super::presentation::GridPresentation,
    pub(crate) carousel: CarouselState,
}

#[derive(Debug)]
pub(crate) struct FavoritesPresentation {
    sections: [FavoriteSection; 3],
    pub(crate) scroll_handle: ScrollHandle,
}

impl Index<VideoItemType> for FavoritesPresentation {
    type Output = FavoriteSection;

    fn index(&self, item_type: VideoItemType) -> &Self::Output {
        &self.sections[section_index(item_type)]
    }
}

impl IndexMut<VideoItemType> for FavoritesPresentation {
    fn index_mut(&mut self, item_type: VideoItemType) -> &mut Self::Output {
        &mut self.sections[section_index(item_type)]
    }
}

impl FavoritesPresentation {
    pub(crate) fn new() -> Self {
        Self {
            sections: FAVORITE_ITEM_TYPES.map(|_| FavoriteSection {
                effect: Default::default(),
                presentation: Default::default(),
                carousel: Default::default(),
            }),
            scroll_handle: ScrollHandle::new(),
        }
    }

    pub(crate) fn cancel_effects(&mut self) {
        for section in &mut self.sections {
            section.effect.cancel();
        }
    }

    pub(crate) fn sync_previous_offsets(&mut self) {
        for section in &mut self.sections {
            section.carousel.sync_previous_offset();
        }
    }
}
