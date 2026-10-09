use crate::emby::VideoItemType;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FavoriteItemType {
    Movie,
    Series,
    Episode,
    Person,
}

impl FavoriteItemType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "Movie",
            Self::Series => "Series",
            Self::Episode => "Episode",
            Self::Person => "Person",
        }
    }

    pub(crate) fn video_types(self) -> &'static [VideoItemType] {
        match self {
            Self::Movie => &[VideoItemType::Movie],
            Self::Series => &[VideoItemType::Series],
            Self::Episode => &[VideoItemType::Episode],
            Self::Person => &[],
        }
    }
}

impl From<VideoItemType> for FavoriteItemType {
    fn from(item_type: VideoItemType) -> Self {
        match item_type {
            VideoItemType::Movie => Self::Movie,
            VideoItemType::Series => Self::Series,
            VideoItemType::Episode => Self::Episode,
        }
    }
}
