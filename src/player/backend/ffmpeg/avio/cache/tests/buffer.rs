use super::super::ByteRingBuffer;

#[test]
fn paged_byte_buffer_reads_across_page_boundaries() {
    let mut buffer = ByteRingBuffer::new_with_page_size_for_test(32, 4);
    buffer.append(b"abcdefghij");

    assert_eq!(buffer.page_count_for_test(), 3);
    let mut output = [0; 7];
    assert_eq!(buffer.copy_at(2, &mut output), output.len());
    assert_eq!(&output, b"cdefghi");
}

#[test]
fn paged_byte_buffer_discards_partial_and_complete_pages_without_copying_tail() {
    let mut buffer = ByteRingBuffer::new_with_page_size_for_test(32, 4);
    buffer.append(b"abcdefghijkl");

    buffer.discard_front(6);
    assert_eq!(buffer.len(), 6);
    assert_eq!(buffer.page_count_for_test(), 2);
    let mut output = [0; 6];
    assert_eq!(buffer.copy_at(0, &mut output), output.len());
    assert_eq!(&output, b"ghijkl");
}

#[test]
fn paged_byte_buffer_capacity_shrink_only_drops_old_pages() {
    let mut buffer = ByteRingBuffer::new_with_page_size_for_test(32, 4);
    buffer.append(b"abcdefghijkl");

    buffer.resize_capacity(5);
    assert_eq!(buffer.len(), 5);
    assert_eq!(buffer.max_capacity(), 5);
    let mut output = [0; 5];
    assert_eq!(buffer.copy_at(0, &mut output), output.len());
    assert_eq!(&output, b"hijkl");
}

#[test]
fn paged_byte_buffer_appends_do_not_reallocate_existing_pages() {
    let mut buffer = ByteRingBuffer::new_with_page_size_for_test(32, 4);
    buffer.append(b"abcd");
    let first_page_count = buffer.page_count_for_test();
    buffer.append(b"efgh");

    assert_eq!(first_page_count, 1);
    assert_eq!(buffer.page_count_for_test(), 2);
    let mut output = [0; 8];
    assert_eq!(buffer.copy_at(0, &mut output), output.len());
    assert_eq!(&output, b"abcdefgh");
}

#[test]
fn paged_byte_buffer_split_transfers_whole_pages_and_shares_only_boundary_slab() {
    let mut buffer = ByteRingBuffer::new_with_page_size_for_test(32, 4);
    buffer.append(b"abcdefghijkl");
    let original_ptrs = buffer.page_data_ptrs_for_test();

    let suffix = buffer.split_off(6);

    assert_eq!(buffer.page_data_ptrs_for_test(), original_ptrs[..2]);
    assert_eq!(suffix.page_data_ptrs_for_test(), original_ptrs[1..]);
    let mut prefix_bytes = [0; 6];
    let mut suffix_bytes = [0; 6];
    assert_eq!(buffer.copy_at(0, &mut prefix_bytes), 6);
    assert_eq!(suffix.copy_at(0, &mut suffix_bytes), 6);
    assert_eq!(&prefix_bytes, b"abcdef");
    assert_eq!(&suffix_bytes, b"ghijkl");
}
