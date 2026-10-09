use serde::{Deserialize, Deserializer};

use super::MediaItem;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct MediaGenre {
    #[serde(default)]
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_genre_id")]
    pub id: Option<String>,
}

impl MediaGenre {
    pub(crate) fn normalized(&self) -> Option<Self> {
        let name = self.name.trim();
        if name.is_empty() {
            return None;
        }
        Some(Self {
            name: name.to_string(),
            id: self
                .id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
        })
    }

    pub(crate) fn key(&self) -> String {
        match &self.id {
            Some(id) => format!("id:{id}"),
            None => format!("name:{}", self.name),
        }
    }
}

impl MediaItem {
    pub(crate) fn genre_tags(&self) -> Vec<MediaGenre> {
        let genre_items = self.genre_items.as_deref().unwrap_or_default();
        let names = self
            .genres
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(String::as_str)
            .chain(genre_items.iter().map(|genre| genre.name.as_str()));
        let mut tags: Vec<MediaGenre> = Vec::new();
        for name in names.map(str::trim).filter(|name| !name.is_empty()) {
            if tags.iter().any(|tag| tag.name == name) {
                continue;
            }
            let id = genre_items
                .iter()
                .filter(|genre| genre.name.trim() == name)
                .filter_map(|genre| genre.id.as_deref())
                .map(str::trim)
                .find(|id| !id.is_empty())
                .map(str::to_string);
            tags.push(MediaGenre {
                name: name.to_string(),
                id,
            });
        }
        tags
    }
}

fn deserialize_genre_id<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum GenreId {
        Text(String),
        Number(i64),
    }
    Ok(
        Option::<GenreId>::deserialize(deserializer)?.map(|id| match id {
            GenreId::Text(id) => id,
            GenreId::Number(id) => id.to_string(),
        }),
    )
}

#[cfg(test)]
mod tests;
