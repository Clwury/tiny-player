use super::*;
use serde_json::json;

#[test]
fn genre_tags_keep_individual_names_and_accept_text_and_numeric_ids() {
    let item: MediaItem = serde_json::from_value(json!({
        "Id": "movie", "Name": "Movie",
        "Genres": [" 动画 ", "", "动画", "旧类型"],
        "GenreItems": [
            {"Name": "动画", "Id": " g18 "},
            {"Name": "动作", "Id": 19},
            {"Name": "旧类型", "Id": " "},
            {"Name": " ", "Id": 0}
        ]
    }))
    .unwrap();
    let tags = item.genre_tags();
    assert_eq!(
        tags,
        [
            MediaGenre {
                name: "动画".into(),
                id: Some("g18".into())
            },
            MediaGenre {
                name: "旧类型".into(),
                id: None
            },
            MediaGenre {
                name: "动作".into(),
                id: Some("19".into())
            },
        ]
    );
    assert_eq!(tags[0].key(), "id:g18");
    assert_eq!(tags[1].key(), "name:旧类型");
}

#[test]
fn legacy_genres_remain_clickable_without_ids_and_empty_names_are_ignored() {
    let item: MediaItem = serde_json::from_value(json!({
        "Id": "movie", "Name": "Movie", "Genres": ["Drama", " Drama ", ""]
    }))
    .unwrap();
    assert_eq!(
        item.genre_tags(),
        [MediaGenre {
            name: "Drama".into(),
            id: None
        }]
    );
    let empty: MediaGenre = serde_json::from_value(json!({"Name": "  ", "Id": null})).unwrap();
    assert!(empty.normalized().is_none());
    assert!(serde_json::from_value::<MediaGenre>(json!({"Name": "Drama", "Id": true})).is_err());
}
