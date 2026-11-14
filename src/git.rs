//! Compare a git revision with the working tree (or with another revision).
//!
//! The revision is exported to a temporary directory with `git cat-file`, so the
//! rest of the program only ever sees two ordinary directories. Only the `git`
//! command line is used, no extra crates. The exported sides are read-only;
//! copying from one to the working tree is how a change is reverted.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

/// What the user asked for on the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitOpts {
    /// 0 to 2 revisions.
    pub revs: Vec<String>,
    /// With fewer than two revisions: compare against the index instead of the working tree.
    pub staged: bool,
    /// Where to look for the repository.
    pub repo: PathBuf,
}

/// Two directories ready to be compared. Dropping it removes the temporary exports.
pub struct Session {
    pub left: PathBuf,
    pub right: PathBuf,
    /// Extra gitignore-style patterns (ignored and submodule paths of the working tree).
    pub ignores: Vec<String>,
    /// (left, right) is a snapshot that must not be changed.
    pub readonly: (bool, bool),
    pub labels: (String, String),
    tmp: Vec<PathBuf>,
}

impl Drop for Session {
    fn drop(&mut self) {
        for t in &self.tmp {
            let _ = fs::remove_dir_all(t);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    mode: u32,
    oid: String,
    path: String,
}

fn run_git(repo: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "git was not found in PATH".to_string()
            } else {
                format!("could not run git: {e}")
            }
        })?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        let msg = String::from_utf8_lossy(&out.stderr);
        Err(msg.trim().trim_start_matches("fatal: ").to_string())
    }
}

/// Top directory of the repository containing `dir`.
pub fn repo_root(dir: &Path) -> Result<PathBuf, String> {
    let out = run_git(dir, &["rev-parse", "--show-toplevel"])
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let s = String::from_utf8_lossy(&out);
    let s = s.trim_end_matches(['\n', '\r']);
    if s.is_empty() {
        return Err(format!("{}: not a git repository", dir.display()));
    }
    Ok(PathBuf::from(s))
}

fn check_rev(repo: &Path, rev: &str) -> Result<(), String> {
    if rev.starts_with('-') {
        return Err(format!("'{rev}' is not a revision"));
    }
    let spec = format!("{rev}^{{tree}}");
    run_git(repo, &["rev-parse", "--verify", "--quiet", &spec, "--"])
        .map(|_| ())
        .map_err(|_| format!("unknown revision '{rev}'"))
}

fn split_nul(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split(|b| *b == 0).filter(|s| !s.is_empty())
}

/// Parse `<mode> <type> <oid>\t<path>` (ls-tree) or `<mode> <oid> <stage>\t<path>` (ls-files -s).
fn parse_entries(bytes: &[u8], tree: bool) -> Vec<Entry> {
    let mut out = Vec::new();
    for rec in split_nul(bytes) {
        let Some(tab) = rec.iter().position(|b| *b == b'\t') else { continue };
        let head = String::from_utf8_lossy(&rec[..tab]);
        let Ok(path) = String::from_utf8(rec[tab + 1..].to_vec()) else { continue };
        let f: Vec<&str> = head.split(' ').collect();
        let (mode, oid) = match (tree, f.as_slice()) {
            (true, [m, _ty, oid]) => (m, oid),
            (false, [m, oid, _stage]) => (m, oid),
            _ => continue,
        };
        let Ok(mode) = u32::from_str_radix(mode, 8) else { continue };
        out.push(Entry { mode, oid: (*oid).to_string(), path });
    }
    out
}

fn list_tree(repo: &Path, rev: &str) -> Result<Vec<Entry>, String> {
    Ok(parse_entries(&run_git(repo, &["ls-tree", "-r", "-z", rev, "--"])?, true))
}

fn list_index(repo: &Path) -> Result<Vec<Entry>, String> {
    let mut v = parse_entries(&run_git(repo, &["ls-files", "-s", "-z"])?, false);
    // an unresolved merge lists a path several times; keep one
    v.dedup_by(|a, b| a.path == b.path);
    Ok(v)
}

fn safe_rel(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.split('/').any(|c| c == ".." || c.is_empty() || c == ".git")
        && !p.contains(':')
        && !p.contains('\\')
}

/// Write `entries` below `dest` (created). Submodules (commits) are skipped.
fn export(repo: &Path, entries: &[Entry], dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let want: Vec<&Entry> = entries.iter().filter(|e| e.mode != 0o160000 && safe_rel(&e.path)).collect();
    if want.is_empty() {
        return Ok(());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not run git: {e}"))?;
    let mut sin = child.stdin.take().ok_or("no stdin")?;
    let mut sout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
    let result = (|| -> Result<(), String> {
        let mut line = String::new();
        for e in &want {
            writeln!(sin, "{}", e.oid).map_err(|e| e.to_string())?;
            sin.flush().map_err(|e| e.to_string())?;
            line.clear();
            sout.read_line(&mut line).map_err(|e| e.to_string())?;
            let f: Vec<&str> = line.split_whitespace().collect();
            let size: usize = match f.as_slice() {
                [_, _, size] => size.parse().map_err(|_| format!("bad reply for {}", e.path))?,
                _ => return Err(format!("object for '{}' is missing", e.path)),
            };
            let mut data = vec![0u8; size];
            sout.read_exact(&mut data).map_err(|e| e.to_string())?;
            let mut nl = [0u8; 1];
            sout.read_exact(&mut nl).map_err(|e| e.to_string())?;
            write_entry(&dest.join(&e.path), e.mode, &data).map_err(|er| format!("{}: {er}", e.path))?;
        }
        Ok(())
    })();
    drop(sin);
    let _ = child.wait();
    result
}

fn write_entry(path: &Path, mode: u32, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if mode == 0o120000 {
        #[cfg(unix)]
        {
            let target = String::from_utf8_lossy(data).into_owned();
            return std::os::unix::fs::symlink(target, path);
        }
    }
    fs::write(path, data)?;
    #[cfg(unix)]
    if mode & 0o111 != 0 {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn temp_dir(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!("dirrcmp-git-{}-{}-{tag}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)))
}

fn has_glob(s: &str) -> bool {
    s.contains(['*', '?', '[', '\\', '!', '#']) || s.starts_with(' ') || s.ends_with(' ')
}

/// Patterns for everything git ignores in the working tree, plus submodule directories.
fn worktree_ignores(repo: &Path) -> Result<Vec<String>, String> {
    let mut pats = Vec::new();
    let out = run_git(
        repo,
        &["ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--directory"],
    )?;
    for rec in split_nul(&out) {
        let Ok(p) = std::str::from_utf8(rec) else { continue };
        if has_glob(p) {
            continue; // can't be written as a pattern safely; shows up as a difference
        }
        pats.push(format!("/{p}"));
    }
    // submodules are not exported, so keep their working directories out of the comparison
    let out = run_git(repo, &["ls-files", "-s", "-z"])?;
    for e in parse_entries(&out, false) {
        if e.mode == 0o160000 && !has_glob(&e.path) {
            pats.push(format!("/{}/", e.path));
        }
    }
    Ok(pats)
}

fn short(rev: &str) -> String {
    rev.to_string()
}

/// Resolve `opts` into two directories to compare.
pub fn prepare(opts: &GitOpts) -> Result<Session, String> {
    if opts.revs.len() > 2 {
        return Err("--git takes at most two revisions".to_string());
    }
    if opts.staged && opts.revs.len() > 1 {
        return Err("--staged compares one revision with the index; give at most one".to_string());
    }
    let repo = repo_root(&opts.repo)?;
    for r in &opts.revs {
        check_rev(&repo, r)?;
    }
    let mut tmp = Vec::new();
    let mut make = |entries: Vec<Entry>, tag: &str| -> Result<PathBuf, String> {
        let d = temp_dir(tag);
        tmp.push(d.clone());
        export(&repo, &entries, &d)?;
        Ok(d)
    };
    let (left_rev, right_rev) = match opts.revs.as_slice() {
        [] => ("HEAD".to_string(), None),
        [a] => (a.clone(), None),
        [a, b] => (a.clone(), Some(b.clone())),
        _ => unreachable!(),
    };
    // A repository without commits has no HEAD: the left side is simply empty.
    let left_entries = match list_tree(&repo, &left_rev) {
        Ok(v) => v,
        Err(_) if left_rev == "HEAD" && opts.revs.is_empty() => Vec::new(),
        Err(e) => return Err(e),
    };
    let left_dir = make(left_entries, "left")?;
    let (right, ignores, ro_right, label_right) = match right_rev {
        Some(r) => {
            let d = make(list_tree(&repo, &r)?, "right")?;
            (d, Vec::new(), true, short(&r))
        }
        None if opts.staged => {
            let d = make(list_index(&repo)?, "index")?;
            (d, Vec::new(), true, "index (staged)".to_string())
        }
        None => (repo.clone(), worktree_ignores(&repo)?, false, "working tree".to_string()),
    };
    Ok(Session {
        left: left_dir,
        right,
        ignores,
        readonly: (true, ro_right),
        labels: (short(&left_rev), label_right),
        tmp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_ok() -> bool {
        Command::new("git").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
    }

    fn tmp() -> PathBuf {
        let d = temp_dir("t");
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn g(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn put(p: PathBuf, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    /// A repo with two commits and a dirty working tree and index.
    fn repo() -> PathBuf {
        let r = tmp();
        g(&r, &["init", "-q"]);
        put(r.join("a.txt"), "one\n");
        put(r.join("sub/b.txt"), "b1\n");
        put(r.join("gone.txt"), "bye\n");
        put(r.join(".gitignore"), "*.log\nbuild/\n");
        g(&r, &["add", "-A"]);
        g(&r, &["commit", "-q", "-m", "first"]);
        put(r.join("a.txt"), "two\n");
        g(&r, &["commit", "-q", "-am", "second"]);
        // staged: sub/b.txt changed; unstaged: a.txt changed again, gone.txt removed
        put(r.join("sub/b.txt"), "b2\n");
        g(&r, &["add", "sub/b.txt"]);
        put(r.join("a.txt"), "three\n");
        fs::remove_file(r.join("gone.txt")).unwrap();
        put(r.join("new.txt"), "untracked\n");
        put(r.join("debug.log"), "ignored\n");
        put(r.join("build/out.o"), "ignored\n");
        r
    }

    fn read(p: PathBuf) -> String {
        fs::read_to_string(p).unwrap()
    }

    #[test]
    fn head_against_the_working_tree() {
        if !git_ok() {
            return;
        }
        let r = repo();
        let s = prepare(&GitOpts { revs: vec![], staged: false, repo: r.join("sub") }).unwrap();
        assert_eq!(read(s.left.join("a.txt")), "two\n", "HEAD, not the working copy");
        assert_eq!(read(s.left.join("sub/b.txt")), "b1\n");
        assert!(s.left.join("gone.txt").is_file());
        assert!(!s.left.join(".gitignore").is_dir());
        assert_eq!(s.right, repo_root(&r).unwrap(), "the working tree itself");
        assert_eq!(s.readonly, (true, false));
        assert_eq!(s.labels, ("HEAD".to_string(), "working tree".to_string()));
        assert!(s.ignores.contains(&"/debug.log".to_string()) || s.ignores.iter().any(|p| p.contains("debug.log")));
        assert!(s.ignores.contains(&"/build/".to_string()), "{:?}", s.ignores);
        let left = s.left.clone();
        drop(s);
        assert!(!left.exists(), "the export is removed on drop");
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn ignored_files_do_not_show_up_as_differences() {
        if !git_ok() {
            return;
        }
        use crate::engine::{self, Ignore, Opts, Status};
        let r = repo();
        let s = prepare(&GitOpts { revs: vec![], staged: false, repo: r.clone() }).unwrap();
        let mut ig = Ignore::defaults();
        for p in &s.ignores {
            ig.add(p);
        }
        let opts = Opts { ignore: ig, follow_symlinks: false, compare_dates: false, compare_contents: true, pairs: Vec::new() };
        let tree = engine::compare(&s.left, &s.right, &opts);
        let mut names: Vec<(String, Status)> = tree.children.iter().map(|c| (c.name.clone(), c.status)).collect();
        names.sort_by(|a, b| a.0.cmp(&b.0));
        let n: Vec<&str> = names.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(n, [".gitignore", "a.txt", "gone.txt", "new.txt", "sub"], "{names:?}");
        assert_eq!(names[0].1, Status::Same);
        let st = |name: &str| names.iter().find(|(n, _)| n == name).unwrap().1;
        assert_eq!(st("a.txt"), Status::Modified);
        assert_eq!(st("gone.txt"), Status::LeftOnly);
        assert_eq!(st("new.txt"), Status::RightOnly);
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn revisions_and_the_index() {
        if !git_ok() {
            return;
        }
        let r = repo();
        // one revision against the working tree
        let s = prepare(&GitOpts { revs: vec!["HEAD~1".into()], staged: false, repo: r.clone() }).unwrap();
        assert_eq!(read(s.left.join("a.txt")), "one\n");
        assert_eq!(s.labels.0, "HEAD~1");
        drop(s);
        // two revisions: both are snapshots
        let s = prepare(&GitOpts { revs: vec!["HEAD~1".into(), "HEAD".into()], staged: false, repo: r.clone() }).unwrap();
        assert_eq!(read(s.left.join("a.txt")), "one\n");
        assert_eq!(read(s.right.join("a.txt")), "two\n");
        assert_eq!(s.readonly, (true, true));
        assert!(s.ignores.is_empty());
        assert_eq!(s.labels, ("HEAD~1".to_string(), "HEAD".to_string()));
        drop(s);
        // staged: HEAD against the index
        let s = prepare(&GitOpts { revs: vec![], staged: true, repo: r.clone() }).unwrap();
        assert_eq!(read(s.left.join("sub/b.txt")), "b1\n");
        assert_eq!(read(s.right.join("sub/b.txt")), "b2\n", "staged content");
        assert_eq!(read(s.right.join("a.txt")), "two\n", "unstaged edits are not in the index");
        assert!(!s.right.join("new.txt").exists());
        assert_eq!(s.readonly, (true, true));
        assert_eq!(s.labels.1, "index (staged)");
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn errors_are_reported() {
        if !git_ok() {
            return;
        }
        let r = repo();
        let e = |revs: &[&str], staged: bool, repo: &Path| {
            prepare(&GitOpts { revs: revs.iter().map(|s| s.to_string()).collect(), staged, repo: repo.to_path_buf() })
                .err()
                .unwrap()
        };
        assert_eq!(e(&["nope"], false, &r), "unknown revision 'nope'");
        assert_eq!(e(&["--output=x"], false, &r), "'--output=x' is not a revision");
        assert!(e(&["a", "b", "c"], false, &r).contains("at most two"));
        assert!(e(&["HEAD", "HEAD~1"], true, &r).contains("--staged"));
        let plain = tmp();
        assert!(e(&[], false, &plain).contains("not a git repository"), "{}", e(&[], false, &plain));
        let _ = fs::remove_dir_all(&r);
        let _ = fs::remove_dir_all(&plain);
    }

    #[test]
    fn a_repository_without_commits_has_an_empty_left_side() {
        if !git_ok() {
            return;
        }
        let r = tmp();
        g(&r, &["init", "-q"]);
        put(r.join("x.txt"), "x\n");
        let s = prepare(&GitOpts { revs: vec![], staged: false, repo: r.clone() }).unwrap();
        assert!(s.left.is_dir() && fs::read_dir(&s.left).unwrap().count() == 0);
        let _ = fs::remove_dir_all(&r);
    }

    #[cfg(unix)]
    #[test]
    fn modes_and_symlinks_are_kept() {
        if !git_ok() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let r = tmp();
        g(&r, &["init", "-q"]);
        put(r.join("run.sh"), "#!/bin/sh\n");
        fs::set_permissions(r.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        put(r.join("plain"), "p\n");
        std::os::unix::fs::symlink("plain", r.join("link")).unwrap();
        g(&r, &["add", "-A"]);
        g(&r, &["commit", "-q", "-m", "m"]);
        let s = prepare(&GitOpts { revs: vec![], staged: false, repo: r.clone() }).unwrap();
        assert_eq!(fs::metadata(s.left.join("run.sh")).unwrap().permissions().mode() & 0o111, 0o111);
        assert_eq!(fs::metadata(s.left.join("plain")).unwrap().permissions().mode() & 0o111, 0);
        assert_eq!(fs::read_link(s.left.join("link")).unwrap(), PathBuf::from("plain"));
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn binary_files_and_odd_names_survive() {
        if !git_ok() {
            return;
        }
        let r = tmp();
        g(&r, &["init", "-q"]);
        fs::write(r.join("bin.dat"), [0u8, 1, 2, 255, 10, 13, 0]).unwrap();
        put(r.join("sp ace/ü.txt"), "u\n");
        put(r.join("big.txt"), &"line\n".repeat(50_000));
        g(&r, &["add", "-A"]);
        g(&r, &["commit", "-q", "-m", "m"]);
        let s = prepare(&GitOpts { revs: vec![], staged: false, repo: r.clone() }).unwrap();
        assert_eq!(fs::read(s.left.join("bin.dat")).unwrap(), [0u8, 1, 2, 255, 10, 13, 0]);
        assert_eq!(read(s.left.join("sp ace/ü.txt")), "u\n");
        assert_eq!(fs::metadata(s.left.join("big.txt")).unwrap().len(), 250_000);
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn entry_parsing_and_path_safety() {
        let tree = b"100644 blob 1111111111111111111111111111111111111111\ta b.txt\0160000 commit 2222222222222222222222222222222222222222\tmod\0";
        let v = parse_entries(tree, true);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], Entry { mode: 0o100644, oid: "1".repeat(40), path: "a b.txt".into() });
        assert_eq!(v[1].mode, 0o160000);
        let idx = b"100755 3333333333333333333333333333333333333333 0\tdir/run\0";
        let v = parse_entries(idx, false);
        assert_eq!(v, vec![Entry { mode: 0o100755, oid: "3".repeat(40), path: "dir/run".into() }]);
        assert!(parse_entries(b"garbage\0", true).is_empty());
        assert!(safe_rel("a/b.txt") && safe_rel("dir/.hidden"));
        assert!(!safe_rel("../x") && !safe_rel("/abs") && !safe_rel("a/../b") && !safe_rel(".git/config"));
        assert!(!safe_rel("a//b") && !safe_rel("") && !safe_rel("c:/x"));
        assert!(has_glob("a*b") && has_glob("#x") && !has_glob("plain/name.txt"));
    }
}
