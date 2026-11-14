//! Undo / redo for copy and delete operations in the directory view.
//!
//! Deleting moves the entry into a private per-session backup directory
//! instead of removing it; overwriting copies first snapshot whatever they
//! replace. Backups live in the system temp dir and are removed on exit, so
//! undo only works within a session. A snapshot of an overwritten directory
//! is a real copy and temporarily needs that much free disk space.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::engine::{self, Ignore};

const MAX_ENTRIES: usize = 50;

static SESSION: AtomicUsize = AtomicUsize::new(0);

/// What to do (and what to redo).
#[derive(Clone)]
pub enum Recipe {
    Copy { src: PathBuf, dst: PathBuf, rel: PathBuf, follow: bool, ignore: Ignore },
    Delete { path: PathBuf },
    /// Move/rename `from` to `to` (nothing is overwritten).
    Move { from: PathBuf, to: PathBuf },
}

impl Recipe {
    pub fn rename(from: PathBuf, to: PathBuf) -> Recipe {
        Recipe::Move { from, to }
    }
    pub fn copy(src: PathBuf, dst: PathBuf, rel: PathBuf, follow: bool, ignore: Ignore) -> Recipe {
        Recipe::Copy { src, dst, rel, follow, ignore }
    }
    pub fn delete(path: PathBuf) -> Recipe {
        Recipe::Delete { path }
    }
    fn describe(&self) -> String {
        match self {
            Recipe::Copy { rel, .. } => format!("copy of {}", engine::rel_string(rel)),
            Recipe::Delete { path } => format!("delete of {}", path.display()),
            Recipe::Move { from, to } => format!("move of {} to {}", from.display(), to.display()),
        }
    }
}

struct Done {
    recipe: Recipe,
    /// What the operation replaced (copy over an existing entry) or removed (delete).
    backup: Option<PathBuf>,
    /// Directories the operation had to create, deepest first.
    made_dirs: Vec<PathBuf>,
}

pub struct Journal {
    dir: Option<PathBuf>,
    n: usize,
    max: usize,
    undo: Vec<Done>,
    redo: Vec<Recipe>,
}

impl Journal {
    pub fn new() -> Journal {
        Journal::with_limit(MAX_ENTRIES)
    }

    pub fn with_limit(max: usize) -> Journal {
        Journal { dir: None, n: 0, max: max.max(1), undo: Vec::new(), redo: Vec::new() }
    }

    #[allow(dead_code)]
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Perform `r` and record it so it can be undone. Clears the redo stack.
    pub fn run(&mut self, r: Recipe) -> io::Result<()> {
        let done = self.perform(r)?;
        self.undo.push(done);
        self.redo.clear();
        while self.undo.len() > self.max {
            let old = self.undo.remove(0);
            if let Some(b) = old.backup {
                let _ = engine::remove_any(&b);
            }
        }
        Ok(())
    }

    /// Undo the most recent operation. `Ok(None)` when there is nothing to undo.
    pub fn undo(&mut self) -> io::Result<Option<String>> {
        let Some(done) = self.undo.pop() else {
            return Ok(None);
        };
        match self.revert(&done) {
            Ok(()) => {
                let msg = format!("undid {}", done.recipe.describe());
                self.redo.push(done.recipe);
                Ok(Some(msg))
            }
            Err(e) => {
                self.undo.push(done); // still there to retry
                Err(e)
            }
        }
    }

    /// Redo the most recently undone operation.
    pub fn redo(&mut self) -> io::Result<Option<String>> {
        let Some(recipe) = self.redo.pop() else {
            return Ok(None);
        };
        let msg = format!("redid {}", recipe.describe());
        match self.perform(recipe.clone()) {
            Ok(done) => {
                self.undo.push(done);
                Ok(Some(msg))
            }
            Err(e) => {
                self.redo.push(recipe);
                Err(e)
            }
        }
    }

    // ── internals ──

    fn slot(&mut self) -> io::Result<PathBuf> {
        if self.dir.is_none() {
            let id = SESSION.fetch_add(1, Ordering::SeqCst);
            let d = env::temp_dir().join(format!("dirrcmp-undo-{}-{}", std::process::id(), id));
            create_private_dir(&d)?;
            self.dir = Some(d);
        }
        self.n += 1;
        Ok(self.dir.as_ref().unwrap().join(self.n.to_string()))
    }

    fn perform(&mut self, r: Recipe) -> io::Result<Done> {
        match &r {
            Recipe::Copy { src, dst, rel, follow, ignore } => {
                let existed = fs::symlink_metadata(dst).is_ok();
                let made_dirs = missing_ancestors(dst);
                let backup = if existed {
                    let slot = self.slot()?;
                    engine::copy_entry(dst, &slot, false, &Ignore::new(), Path::new(""))?;
                    Some(slot)
                } else {
                    None
                };
                if let Err(e) = engine::copy_entry(src, dst, *follow, ignore, rel) {
                    // roll back whatever was half done
                    let _ = engine::remove_any(dst);
                    if let Some(b) = &backup {
                        let _ = move_path(b, dst);
                    }
                    for d in &made_dirs {
                        let _ = fs::remove_dir(d);
                    }
                    return Err(e);
                }
                Ok(Done { recipe: r, backup, made_dirs })
            }
            Recipe::Delete { path } => {
                fs::symlink_metadata(path)?; // "not found" surfaces here
                let slot = self.slot()?;
                move_path(path, &slot)?;
                Ok(Done { recipe: r, backup: Some(slot), made_dirs: Vec::new() })
            }
            Recipe::Move { from, to } => {
                fs::symlink_metadata(from)?;
                if fs::symlink_metadata(to).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} already exists", to.display()),
                    ));
                }
                let made_dirs = missing_ancestors(to);
                if let Err(e) = move_path(from, to) {
                    for d in &made_dirs {
                        let _ = fs::remove_dir(d);
                    }
                    return Err(e);
                }
                Ok(Done { recipe: r, backup: None, made_dirs })
            }
        }
    }

    fn revert(&mut self, d: &Done) -> io::Result<()> {
        match &d.recipe {
            Recipe::Copy { dst, .. } => {
                match engine::remove_any(dst) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
                if let Some(b) = &d.backup {
                    move_path(b, dst)?;
                }
                for dir in &d.made_dirs {
                    let _ = fs::remove_dir(dir); // only if now empty
                }
                Ok(())
            }
            Recipe::Delete { path } => {
                if fs::symlink_metadata(path).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists again; cannot undo the delete", path.display()),
                    ));
                }
                let b = d.backup.as_ref().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "backup is missing")
                })?;
                move_path(b, path)
            }
            Recipe::Move { from, to } => {
                if fs::symlink_metadata(from).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists again; cannot undo the move", from.display()),
                    ));
                }
                move_path(to, from)?;
                for dir in &d.made_dirs {
                    let _ = fs::remove_dir(dir); // only if now empty
                }
                Ok(())
            }
        }
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        if let Some(d) = &self.dir {
            let _ = fs::remove_dir_all(d);
        }
    }
}

/// Backups of deleted files must not be readable by other users of the machine.
fn create_private_dir(d: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(d)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(d)
    }
}

/// Directories above `p` that do not exist yet, deepest first.
fn missing_ancestors(p: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = p.parent();
    while let Some(d) = cur {
        if d.as_os_str().is_empty() || fs::symlink_metadata(d).is_ok() {
            break;
        }
        out.push(d.to_path_buf());
        cur = d.parent();
    }
    out
}

/// Rename, falling back to copy + remove when crossing filesystems.
fn move_path(src: &Path, dst: &Path) -> io::Result<()> {
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p)?;
    }
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    engine::copy_entry(src, dst, false, &Ignore::new(), Path::new(""))?;
    engine::remove_any(src)
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    static N: AtomicUsize = AtomicUsize::new(0);

    fn tmp() -> PathBuf {
        let p = env::temp_dir()
            .join(format!("dirrcmp-undotest-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    fn read(p: &Path) -> String {
        fs::read_to_string(p).unwrap()
    }

    fn cp(src: &Path, dst: &Path, rel: &str) -> Recipe {
        Recipe::copy(src.to_path_buf(), dst.to_path_buf(), PathBuf::from(rel), false, Ignore::new())
    }

    /// Sorted listing "relative/path=content" of a directory tree.
    fn snapshot(root: &Path) -> Vec<String> {
        fn rec(base: &Path, dir: &Path, out: &mut Vec<String>) {
            let mut es: Vec<_> = fs::read_dir(dir).unwrap().flatten().collect();
            es.sort_by_key(|e| e.file_name());
            for e in es {
                let p = e.path();
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/");
                if p.is_dir() {
                    out.push(format!("{rel}/"));
                    rec(base, &p, out);
                } else {
                    out.push(format!("{rel}={}", fs::read_to_string(&p).unwrap()));
                }
            }
        }
        let mut v = Vec::new();
        rec(root, root, &mut v);
        v
    }

    #[test]
    fn copy_new_file_undo_redo() {
        let root = tmp();
        write(&root.join("l/f"), "hello");
        let mut j = Journal::new();
        j.run(cp(&root.join("l/f"), &root.join("r/f"), "f")).unwrap();
        assert_eq!(read(&root.join("r/f")), "hello");
        assert!(j.can_undo());
        let m = j.undo().unwrap().unwrap();
        assert_eq!(m, "undid copy of f");
        assert!(!root.join("r/f").exists());
        assert!(!root.join("r").exists(), "directories created by the copy are removed again");
        let m = j.redo().unwrap().unwrap();
        assert_eq!(m, "redid copy of f");
        assert_eq!(read(&root.join("r/f")), "hello");
        assert!(j.redo().unwrap().is_none());
        j.undo().unwrap();
        assert!(j.undo().unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn overwrite_restores_previous_content() {
        let root = tmp();
        write(&root.join("l/f"), "new");
        write(&root.join("r/f"), "old");
        let mut j = Journal::new();
        j.run(cp(&root.join("l/f"), &root.join("r/f"), "f")).unwrap();
        assert_eq!(read(&root.join("r/f")), "new");
        j.undo().unwrap();
        assert_eq!(read(&root.join("r/f")), "old");
        assert!(root.join("r").exists(), "pre-existing directories stay");
        j.redo().unwrap();
        assert_eq!(read(&root.join("r/f")), "new");
        j.undo().unwrap();
        assert_eq!(read(&root.join("r/f")), "old");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn directory_merge_is_fully_reverted() {
        let root = tmp();
        write(&root.join("l/d/a.txt"), "A-left");
        write(&root.join("l/d/sub/b.txt"), "B-left");
        write(&root.join("l/d/new.txt"), "N");
        write(&root.join("r/d/a.txt"), "A-right");
        write(&root.join("r/d/extra.txt"), "E");
        write(&root.join("r/d/sub/keep.txt"), "K");
        let before = snapshot(&root.join("r"));
        let mut j = Journal::new();
        j.run(cp(&root.join("l/d"), &root.join("r/d"), "d")).unwrap();
        let after = snapshot(&root.join("r"));
        assert_ne!(before, after);
        assert_eq!(read(&root.join("r/d/a.txt")), "A-left");
        assert!(root.join("r/d/extra.txt").exists(), "merge keeps extras");
        j.undo().unwrap();
        assert_eq!(snapshot(&root.join("r")), before);
        j.redo().unwrap();
        assert_eq!(snapshot(&root.join("r")), after);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_undo_redo() {
        let root = tmp();
        write(&root.join("r/f.txt"), "file");
        write(&root.join("r/d/x"), "X");
        write(&root.join("r/d/deep/y"), "Y");
        let before = snapshot(&root.join("r"));
        let mut j = Journal::new();
        j.run(Recipe::delete(root.join("r/f.txt"))).unwrap();
        j.run(Recipe::delete(root.join("r/d"))).unwrap();
        assert!(!root.join("r/f.txt").exists() && !root.join("r/d").exists());
        assert_eq!(j.undo().unwrap().unwrap(), format!("undid delete of {}", root.join("r/d").display()));
        assert_eq!(read(&root.join("r/d/deep/y")), "Y");
        j.undo().unwrap();
        assert_eq!(snapshot(&root.join("r")), before);
        j.redo().unwrap(); // deletes f.txt again
        assert!(!root.join("r/f.txt").exists());
        assert!(root.join("r/d").exists());
        // deleting something that is not there is an error and records nothing
        let n = j.undo.len();
        assert!(j.run(Recipe::delete(root.join("r/ghost"))).is_err());
        assert_eq!(j.undo.len(), n);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn undo_delete_refuses_to_clobber() {
        let root = tmp();
        write(&root.join("r/f"), "original");
        let mut j = Journal::new();
        j.run(Recipe::delete(root.join("r/f"))).unwrap();
        write(&root.join("r/f"), "someone made this");
        let e = j.undo().unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(read(&root.join("r/f")), "someone made this");
        // the entry is kept: once the path is free again the undo works
        fs::remove_file(root.join("r/f")).unwrap();
        j.undo().unwrap();
        assert_eq!(read(&root.join("r/f")), "original");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn new_action_clears_redo() {
        let root = tmp();
        write(&root.join("l/a"), "a");
        write(&root.join("l/b"), "b");
        let mut j = Journal::new();
        j.run(cp(&root.join("l/a"), &root.join("r/a"), "a")).unwrap();
        j.undo().unwrap();
        j.run(cp(&root.join("l/b"), &root.join("r/b"), "b")).unwrap();
        assert!(j.redo().unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn move_undo_redo_and_refusals() {
        let root = tmp();
        write(&root.join("r/a/old.txt"), "data");
        let mut j = Journal::new();
        j.run(Recipe::rename(root.join("r/a/old.txt"), root.join("r/b/c/new.txt"))).unwrap();
        assert!(!root.join("r/a/old.txt").exists());
        assert_eq!(read(&root.join("r/b/c/new.txt")), "data");
        assert!(j.undo().unwrap().is_some());
        assert_eq!(read(&root.join("r/a/old.txt")), "data");
        assert!(!root.join("r/b").exists(), "directories made for the move are removed again");
        assert!(j.redo().unwrap().is_some());
        assert_eq!(read(&root.join("r/b/c/new.txt")), "data");
        // never overwrites, and a missing source is an error that records nothing
        write(&root.join("r/x"), "x");
        assert!(j.run(Recipe::rename(root.join("r/x"), root.join("r/b/c/new.txt"))).is_err());
        assert_eq!(read(&root.join("r/b/c/new.txt")), "data");
        assert!(j.run(Recipe::rename(root.join("r/none"), root.join("r/y"))).is_err());
        assert_eq!(j.undo.len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_copy_changes_nothing() {
        let root = tmp();
        write(&root.join("r/f"), "keep me");
        let mut j = Journal::new();
        let r = j.run(cp(&root.join("l/missing"), &root.join("r/f"), "f"));
        assert!(r.is_err());
        assert_eq!(read(&root.join("r/f")), "keep me");
        assert!(!j.can_undo());
        // and a missing source with missing parents leaves no stray directories
        let r = j.run(cp(&root.join("l/missing"), &root.join("new/deep/f"), "f"));
        assert!(r.is_err());
        assert!(!root.join("new").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn history_is_capped_and_backups_freed() {
        let root = tmp();
        let mut j = Journal::with_limit(3);
        for i in 0..6 {
            write(&root.join(format!("r/f{i}")), "x");
            j.run(Recipe::delete(root.join(format!("r/f{i}")))).unwrap();
        }
        assert_eq!(j.undo.len(), 3);
        let dir = j.dir.clone().unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 3, "evicted backups are removed from disk");
        for _ in 0..3 {
            assert!(j.undo().unwrap().is_some());
        }
        assert!(j.undo().unwrap().is_none());
        // the oldest three deletes can no longer be undone
        assert!(!root.join("r/f0").exists() && root.join("r/f3").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn session_dir_is_removed_on_drop_and_private() {
        let root = tmp();
        write(&root.join("r/f"), "secret");
        let dir;
        {
            let mut j = Journal::new();
            j.run(Recipe::delete(root.join("r/f"))).unwrap();
            dir = j.dir.clone().unwrap();
            assert!(dir.exists());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o700, "backups must not be readable by other users");
            }
        }
        assert!(!dir.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_survive_a_round_trip() {
        use std::os::unix::fs::symlink;
        let root = tmp();
        write(&root.join("r/target"), "t");
        symlink("target", root.join("r/link")).unwrap();
        let mut j = Journal::new();
        j.run(Recipe::delete(root.join("r/link"))).unwrap();
        assert!(fs::symlink_metadata(root.join("r/link")).is_err());
        j.undo().unwrap();
        assert!(fs::symlink_metadata(root.join("r/link")).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(root.join("r/link")).unwrap(), PathBuf::from("target"));
        let _ = fs::remove_dir_all(&root);
    }
}
