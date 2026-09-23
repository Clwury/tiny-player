use std::{fs::File, io};

// Use OS positional I/O: a shared File is read and written by several cache
// workers, so seeking and then reading/writing would race on its cursor.
pub(in crate::backend::ffmpeg) fn read_at(
    file: &File,
    output: &mut [u8],
    offset: u64,
) -> io::Result<usize> {
    loop {
        #[cfg(unix)]
        let result = std::os::unix::fs::FileExt::read_at(file, output, offset);
        #[cfg(windows)]
        let result = std::os::windows::fs::FileExt::seek_read(file, output, offset);
        match result {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

pub(super) fn write_all_at(file: &File, mut data: &[u8], mut offset: u64) -> io::Result<()> {
    while !data.is_empty() {
        #[cfg(unix)]
        let result = std::os::unix::fs::FileExt::write_at(file, data, offset);
        #[cfg(windows)]
        let result = std::os::windows::fs::FileExt::seek_write(file, data, offset);
        match result {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => {
                data = &data[written..];
                offset = offset
                    .checked_add(written as u64)
                    .ok_or(io::ErrorKind::InvalidInput)?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};

    #[test]
    fn concurrent_cache_workers_do_not_share_a_file_position() {
        let file = Arc::new(tempfile::tempfile().unwrap());
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let file = Arc::clone(&file);
                thread::spawn(move || {
                    let expected = vec![worker as u8 + 1; 4096];
                    let offset = worker * expected.len() as u64;
                    for _ in 0..100 {
                        write_all_at(&file, &expected, offset).unwrap();
                        let mut actual = vec![0; expected.len()];
                        let mut read = 0;
                        while read < actual.len() {
                            let count =
                                read_at(&file, &mut actual[read..], offset + read as u64).unwrap();
                            assert_ne!(count, 0);
                            read += count;
                        }
                        assert_eq!(actual, expected);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(file.metadata().unwrap().len(), 8 * 4096);
        assert_eq!(read_at(&file, &mut [0; 1], 8 * 4096).unwrap(), 0);
    }
}
