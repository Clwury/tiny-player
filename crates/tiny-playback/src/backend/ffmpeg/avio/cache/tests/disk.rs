use crate::backend::CacheUnlinkPolicy;

use super::super::HttpDiskCache;

#[test]
fn large_media_offsets_and_repeated_eviction_never_grow_the_disk_file_past_its_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache =
        HttpDiskCache::new(16, Some(dir.path().into()), CacheUnlinkPolicy::WhenDone).unwrap();
    for index in 0..100 {
        let offset = 10_000_000_000 + index * 4;
        cache.write_at(offset, &[index as u8; 4]).unwrap();
        let mut restored = [0; 4];
        assert_eq!(cache.read_at(offset, &mut restored), Some(4));
        assert_eq!(restored, [index as u8; 4]);
        assert!(std::fs::metadata(&cache.path).unwrap().len() <= 16);
        assert!(cache.cached_bytes() <= 16);
    }
    cache.set_limit(4);
    cache.storage.maintain_file_size();
    assert!(std::fs::metadata(&cache.path).unwrap().len() <= 4);
    cache.write_at(100, b"last").unwrap();
    let mut restored = [0; 4];
    assert_eq!(cache.read_at(100, &mut restored), Some(4));
    assert_eq!(&restored, b"last");
}

#[test]
fn http_disk_cache_uses_the_supplied_directory() {
    let directory = tempfile::tempdir().unwrap();
    let expected = directory.path();
    let mut disk_cache = HttpDiskCache::new(
        1024,
        Some(expected.to_path_buf()),
        CacheUnlinkPolicy::WhenDone,
    )
    .expect("supplied disk cache directory creates");
    let path = disk_cache.path.clone();
    assert_eq!(path.parent(), Some(expected));
    disk_cache.write_at(0, b"payload").expect("payload writes");
    let mut restored = [0; 7];
    assert_eq!(disk_cache.read_at(0, &mut restored), Some(7));
    assert_eq!(&restored, b"payload");
    drop(disk_cache);
    assert!(!path.exists());
}

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

#[test]
fn quota_change_before_http_write_publication_reclaims_the_rejected_lease() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache =
        HttpDiskCache::new(16, Some(dir.path().into()), CacheUnlinkPolicy::WhenDone).unwrap();
    cache.write_at(0, b"retained").unwrap();
    let block = cache.reserve_write(100, 8).unwrap();
    let result = block.write(b"rejected");
    let write = super::super::PendingHttpDiskCacheWrite {
        block,
        result,
        storage: cache.storage.clone(),
    };
    cache.set_limit(8);
    cache.storage.maintain_file_size();
    assert_eq!(std::fs::metadata(&cache.path).unwrap().len(), 16);
    super::super::shared::finish_http_disk_write(Some(write), |write| {
        // A config/restart may reject publication after the writer finished.
        drop(write);
    });
    assert_eq!(std::fs::metadata(&cache.path).unwrap().len(), 8);
    let mut output = [0; 8];
    assert_eq!(cache.read_at(0, &mut output), Some(8));
    assert_eq!(&output, b"retained");
}
