use crate::player::backend::CacheUnlinkPolicy;

use super::super::HttpDiskCache;

#[test]
fn http_disk_cache_unlinks_immediately_but_keeps_open_file_usable() {
    let dir = tempfile::tempdir().expect("temp dir creates");
    let mut disk_cache = HttpDiskCache::new(
        1024,
        Some(dir.path().to_path_buf()),
        CacheUnlinkPolicy::Immediate,
    )
    .expect("disk cache creates");
    let path = disk_cache.path.clone();

    assert!(!path.exists());
    disk_cache.write_at(0, b"payload").expect("payload writes");
    let mut restored = [0; 7];

    assert_eq!(disk_cache.read_at(0, &mut restored), Some(7));
    assert_eq!(&restored, b"payload");
}
#[test]
fn http_disk_cache_prunes_least_recently_used_range() {
    let dir = tempfile::tempdir().expect("temp dir creates");
    let mut disk_cache = HttpDiskCache::new(
        8,
        Some(dir.path().to_path_buf()),
        CacheUnlinkPolicy::WhenDone,
    )
    .expect("disk cache creates");
    disk_cache.write_at(0, b"aaaa").expect("first range writes");
    disk_cache
        .write_at(10, b"bbbb")
        .expect("second range writes");
    let mut restored = [0; 1];
    assert_eq!(disk_cache.read_at(0, &mut restored), Some(1));

    disk_cache
        .write_at(20, b"cccc")
        .expect("third range writes");

    assert!(disk_cache.read_at(10, &mut restored).is_none());
    assert_eq!(disk_cache.read_at(0, &mut restored), Some(1));
    assert_eq!(restored[0], b'a');
    assert_eq!(disk_cache.read_at(20, &mut restored), Some(1));
    assert_eq!(restored[0], b'c');
}
#[test]
fn http_disk_cache_removes_file_when_done() {
    let dir = tempfile::tempdir().expect("temp dir creates");
    let path = {
        let disk_cache = HttpDiskCache::new(
            1024,
            Some(dir.path().to_path_buf()),
            CacheUnlinkPolicy::WhenDone,
        )
        .expect("disk cache creates");
        let path = disk_cache.path.clone();
        assert!(path.exists());
        path
    };

    assert!(!path.exists());
}
#[test]
fn http_disk_cache_can_leave_file_for_inspection() {
    let dir = tempfile::tempdir().expect("temp dir creates");
    let path = {
        let disk_cache = HttpDiskCache::new(
            1024,
            Some(dir.path().to_path_buf()),
            CacheUnlinkPolicy::Never,
        )
        .expect("disk cache creates");
        let path = disk_cache.path.clone();
        assert!(path.exists());
        path
    };

    assert!(path.exists());
    std::fs::remove_file(path).expect("leftover cache file removes");
}
