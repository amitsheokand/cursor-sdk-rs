//! RAII guard for directories created during tests.

use std::ops::Deref;
use std::path::{Path, PathBuf};

pub struct TestDir(PathBuf);

impl TestDir {
    pub fn fresh_in_temp(prefix: &str, n: u64) -> Self {
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }

    /// Directory removed on drop; not created here (caller or SUT creates it).
    pub fn hold(path: PathBuf) -> Self {
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }

    pub fn fresh_under_home(relative_prefix: &str, n: u64) -> Self {
        let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
        let dir = home.join(format!("{relative_prefix}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("home temp dir");
        Self(dir)
    }

    pub fn to_path_buf(&self) -> PathBuf {
        self.0.clone()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Deref for TestDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}
