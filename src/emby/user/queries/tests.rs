use super::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::Duration,
};

#[test]
fn favorite_persons_endpoint_is_user_scoped_and_preserves_sort_paging_and_portrait_tags() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 1024];
        while !bytes.windows(4).any(|value| value == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
        }
        let request = String::from_utf8(bytes).unwrap();
        let body = r#"{"Items":[{"Id":"person-1","Name":"Actor","Type":"Person","ImageTags":{"Primary":"portrait"},"UserData":{"IsFavorite":true}}],"TotalRecordCount":31}"#;
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        request
    });
    let server: CachedServer = serde_json::from_value(serde_json::json!({
        "id": "persons-test", "endpoint": { "protocol": "Http", "address": "127.0.0.1", "port": port, "path": "/emby" },
        "username": "test", "password": "", "user_id": "user-1", "access_token": "test-token", "added_at_unix": 0
    })).unwrap();
    let client = EmbyClient::new("persons-test".into()).unwrap();
    let items = client
        .query_persons(
            &server,
            &UserItemsQuery {
                is_favorite: Some(true),
                start_index: 30,
                limit: 30,
                sort_by: Some(UserItemsSort::SortName),
                sort_order: SortOrder::Descending,
                ..Default::default()
            },
        )
        .unwrap();
    let request = worker.join().unwrap();
    assert!(
        request
            .to_ascii_lowercase()
            .contains("x-emby-token: test-token")
    );
    let target = request
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
    assert_eq!(url.path(), "/emby/Persons");
    let pairs = url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    for (key, value) in [
        ("UserId", "user-1"),
        ("Filters", "IsFavorite"),
        ("StartIndex", "30"),
        ("Limit", "30"),
        ("SortBy", "SortName"),
        ("SortOrder", "Descending"),
        ("EnableUserData", "true"),
        ("EnableImages", "true"),
    ] {
        assert_eq!(pairs.get(key).map(|value| value.as_ref()), Some(value));
    }
    assert!(!pairs.contains_key("IncludeItemTypes"));
    assert!(!pairs.contains_key("PersonIds"));
    assert_eq!(items.total_record_count, 31);
    assert_eq!(items.items[0].item_type.as_deref(), Some("Person"));
    assert_eq!(items.items[0].primary_image_tag(), Some("portrait"));
    assert!(items.items[0].user_data.as_ref().unwrap().is_favorite);
}
