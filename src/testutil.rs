//! Test helper: a throw-away directory tree that stands in for `/sys`.

use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A unique temporary directory, removed on drop.
pub struct FakeSys {
    root: PathBuf,
}

impl FakeSys {
    pub fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("fan_ctl-test-{}-{n}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Creates (or overwrites) a file, creating parent directories.
    pub fn file(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    pub fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.root.join(rel)).unwrap()
    }

    pub fn remove(&self, rel: &str) {
        fs::remove_dir_all(self.root.join(rel)).unwrap();
    }

    /// Creates a symlink at `rel` pointing to `target` (target need not exist).
    pub fn symlink(&self, rel: &str, target: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        symlink(target, p).unwrap();
    }
}

impl Drop for FakeSys {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
