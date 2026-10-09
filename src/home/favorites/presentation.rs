use std::ops::{Index, IndexMut};

use gpui::ScrollHandle;

use crate::home::model::favorites::FavoriteItemType;

use super::super::carousel::CarouselState;
use super::controller::{FAVORITE_ITEM_TYPES, FavoritesSource, section_index};

#[derive(Debug)]
pub(crate) struct FavoriteSection {
    // Overview rows and the sorted More page retain independent continuations.
    pub(crate) effect: crate::effects::EffectHandle<gpui::Task<()>>,
    overview_effect: crate::effects::EffectHandle<gpui::Task<()>>,
    pub(crate) presentation: super::super::presentation::GridPresentation,
    pub(crate) carousel: CarouselState,
    pub(in crate::home) sort_menu_open: bool,
}

impl FavoriteSection {
    pub(crate) fn effect_mut(
        &mut self,
        source: FavoritesSource,
    ) -> &mut crate::effects::EffectHandle<gpui::Task<()>> {
        match source {
            FavoritesSource::Overview => &mut self.overview_effect,
            FavoritesSource::Items => &mut self.effect,
        }
    }
}

#[derive(Debug)]
pub(crate) struct FavoritesPresentation {
    sections: [FavoriteSection; 4],
    pub(crate) scroll_handle: ScrollHandle,
}

impl Index<FavoriteItemType> for FavoritesPresentation {
    type Output = FavoriteSection;

    fn index(&self, item_type: FavoriteItemType) -> &Self::Output {
        &self.sections[section_index(item_type)]
    }
}

impl IndexMut<FavoriteItemType> for FavoritesPresentation {
    fn index_mut(&mut self, item_type: FavoriteItemType) -> &mut Self::Output {
        &mut self.sections[section_index(item_type)]
    }
}

impl FavoritesPresentation {
    pub(crate) fn new() -> Self {
        Self {
            sections: FAVORITE_ITEM_TYPES.map(|_| FavoriteSection {
                effect: Default::default(),
                overview_effect: Default::default(),
                presentation: Default::default(),
                carousel: Default::default(),
                sort_menu_open: false,
            }),
            scroll_handle: ScrollHandle::new(),
        }
    }

    pub(crate) fn cancel_effects(&mut self) {
        for section in &mut self.sections {
            section.effect.cancel();
            section.overview_effect.cancel();
        }
    }

    pub(crate) fn sync_previous_offsets(&mut self) {
        for section in &mut self.sections {
            section.carousel.sync_previous_offset();
        }
    }
}
