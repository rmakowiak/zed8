use anyhow::{Context as _, Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::model::{MAX_SLOTS, Meta};

/// The worktrees on disk. A worktree is a folder in `<wt_home>/n8n` with a
/// `meta.json`. There is no other list.
pub struct Store {
    wt_home: PathBuf,
}

impl Store {
    pub fn new(wt_home: PathBuf) -> Self {
        Self { wt_home }
    }

    pub fn envs_dir(&self) -> PathBuf {
        self.wt_home.join("n8n")
    }

    pub fn list(&self) -> Result<Vec<Meta>> {
        let envs = self.envs_dir();
        let entries = match fs::read_dir(&envs) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).with_context(|| format!("reading {}", envs.display())),
        };
        let mut worktrees = Vec::new();
        for entry in entries {
            let meta_path = entry?.path().join("meta.json");
            if meta_path.is_file() {
                worktrees.push(read_meta(&meta_path)?);
            }
        }
        worktrees.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(worktrees)
    }

    pub fn meta_path(&self, name: &str) -> PathBuf {
        self.envs_dir().join(name).join("meta.json")
    }

    /// Takes the lowest free slot and writes the `meta.json` that `build`
    /// makes for it, under the lock. The written file is the claim, so two
    /// claims at the same time never get the same slot.
    pub fn claim(&self, build: impl FnOnce(u16) -> Meta) -> Result<Meta> {
        let _lock = Lock::acquire(&self.wt_home.join("registry.lock"))?;
        let taken: BTreeSet<u16> = self.list()?.iter().map(|meta| meta.slot).collect();
        let Some(slot) = (0..MAX_SLOTS).find(|slot| !taken.contains(slot)) else {
            bail!(
                "No free slot left (0-{} are all in use). Tear down a worktree first.",
                MAX_SLOTS - 1
            );
        };
        let meta = build(slot);
        if self.meta_path(&meta.name).exists() {
            bail!("A worktree named '{}' exists already", meta.name);
        }
        self.write(&meta)?;
        Ok(meta)
    }

    /// Writes `meta.json` through a temp file and a rename, so a reader never
    /// sees half a file.
    pub fn write(&self, meta: &Meta) -> Result<()> {
        let path = self.meta_path(&meta.name);
        let text = serde_json::to_string_pretty(meta)? + "\n";
        write_atomic(&path, &text)
    }
}

pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
    fs::rename(&temp, path).with_context(|| format!("renaming {}", temp.display()))
}

/// A lock is a folder, because `mkdir` is atomic. The Bash version uses the
/// same folder, so both versions can run side by side.
struct Lock {
    path: PathBuf,
}

impl Lock {
    fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        for _ in 0..50 {
            match fs::create_dir(path) {
                Ok(()) => {
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("creating {}", path.display()));
                }
            }
        }
        bail!(
            "Could not get the lock at {}. If no other process runs, remove the folder.",
            path.display()
        )
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir(&self.path) {
            eprintln!("could not remove the lock {}: {error}", self.path.display());
        }
    }
}

pub fn read_meta(path: &Path) -> Result<Meta> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}
