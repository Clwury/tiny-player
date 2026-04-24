pub mod media;
pub mod mpv_backend;
pub mod player_app;
pub mod render_host;
pub mod state;
pub mod video_presenter;

use std::path::PathBuf;

pub fn sample_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("sample")
}

pub fn run() {
    player_app::run();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    static CWD_LOCK: Mutex<()> = Mutex::new(());

    struct CurrentDirGuard(PathBuf);

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).unwrap();
        }
    }

    #[test]
    fn sample_dir_uses_manifest_dir_instead_of_current_dir() {
        let _lock = CWD_LOCK.lock().unwrap();
        let _original_dir = CurrentDirGuard(std::env::current_dir().unwrap());
        let temp_dir = tempdir().unwrap();
        std::env::set_current_dir(temp_dir.path()).unwrap();

        let sample_dir = sample_dir();

        assert_eq!(
            sample_dir,
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("sample")
        );
    }
}
