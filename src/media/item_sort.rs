use crate::emby::{SortOrder, UserItemsSort, VideoItemType};
use serde::{Deserialize, Serialize};

pub(crate) const ITEM_SORT_OPTIONS: [UserItemsSort; 11] = [
    UserItemsSort::SortName,
    UserItemsSort::DateCreated,
    UserItemsSort::PremiereDate,
    UserItemsSort::ProductionYear,
    UserItemsSort::CommunityRating,
    UserItemsSort::CriticRating,
    UserItemsSort::DatePlayed,
    UserItemsSort::DateLastContentAdded,
    UserItemsSort::PlayCount,
    UserItemsSort::Random,
    UserItemsSort::OfficialRating,
];

#[derive(Clone, Copy, Debug)]
pub(crate) struct ItemSortOptions {
    include_last_content_added: bool,
}

impl ItemSortOptions {
    pub(crate) const ALL: Self = Self {
        include_last_content_added: true,
    };

    pub(crate) fn for_item_types(item_types: &[VideoItemType]) -> Self {
        Self {
            include_last_content_added: matches!(item_types, [VideoItemType::Series]),
        }
    }

    pub(crate) fn contains(self, sort_by: UserItemsSort) -> bool {
        sort_by != UserItemsSort::DateLastContentAdded || self.include_last_content_added
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = UserItemsSort> {
        ITEM_SORT_OPTIONS
            .into_iter()
            .filter(move |sort_by| self.contains(*sort_by))
    }
}

pub(crate) fn item_sort_is_available(sort_by: UserItemsSort, item_types: &[VideoItemType]) -> bool {
    ItemSortOptions::for_item_types(item_types).contains(sort_by)
}

/// One browsing sort shared across item lists and server accounts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ItemSortPreferences {
    pub sort_by: UserItemsSort,
    pub sort_order: SortOrder,
}

impl Default for ItemSortPreferences {
    fn default() -> Self {
        Self {
            sort_by: UserItemsSort::SortName,
            sort_order: SortOrder::Ascending,
        }
    }
}

impl ItemSortPreferences {
    pub(crate) fn for_item_types(self, item_types: &[VideoItemType]) -> Self {
        self.for_options(ItemSortOptions::for_item_types(item_types))
    }

    pub(crate) fn for_options(self, options: ItemSortOptions) -> Self {
        if options.contains(self.sort_by) {
            self
        } else {
            Self {
                sort_by: UserItemsSort::SortName,
                ..self
            }
        }
    }
}
