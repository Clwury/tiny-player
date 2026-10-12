use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AppLanguage {
    #[serde(rename = "zh-Hant")]
    ChineseTraditional,
    #[serde(rename = "en")]
    English,
    #[default]
    #[serde(rename = "zh-Hans", other)]
    ChineseSimplified,
}

impl AppLanguage {
    pub(crate) const ALL: [Self; 3] = [
        Self::ChineseSimplified,
        Self::ChineseTraditional,
        Self::English,
    ];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::ChineseSimplified => "app-language-chinese-simplified",
            Self::ChineseTraditional => "app-language-chinese-traditional",
            Self::English => "app-language-english",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ChineseSimplified => "简体中文",
            Self::ChineseTraditional => "繁體中文",
            Self::English => "English",
        }
    }
}
