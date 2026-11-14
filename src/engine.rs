//! Comparison engine: scanning, comparing, ignore patterns, line diff and
//! file operations. Pure std, no external crates.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

// ───────────────────────────── model ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Same,
    Modified,
    LeftOnly,
    RightOnly,
}

#[derive(Clone, Debug)]
pub struct Meta {
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub mtime: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub name: String,
    /// Path relative to the left root.
    pub rel: PathBuf,
    /// Path relative to the right root. Equal to `rel` unless this entry (or an
    /// ancestor) is a manually paired directory with a different name on each side.
    pub rrel: PathBuf,
    pub left: Option<Meta>,
    pub right: Option<Meta>,
    pub status: Status,
    pub children: Vec<Node>,
}

impl Node {
    /// A node is a directory when it is a directory on every side it exists on.
    /// A file-vs-directory clash is treated as a (modified) file entry.
    pub fn is_dir(&self) -> bool {
        match (&self.left, &self.right) {
            (Some(l), Some(r)) => l.is_dir && r.is_dir,
            (Some(m), None) | (None, Some(m)) => m.is_dir,
            (None, None) => false,
        }
    }

    /// True for a directory that was paired by hand with a differently named one
    /// (its own name differs between the sides; entries *inside* it do not count).
    pub fn is_paired(&self) -> bool {
        self.left.is_some() && self.right.is_some() && self.rel.file_name() != self.rrel.file_name()
    }
}

#[derive(Clone, Debug)]
pub struct Opts {
    pub ignore: Ignore,
    pub follow_symlinks: bool,
    /// Treat a differing modification time as a difference.
    pub compare_dates: bool,
    /// Compare file contents (exact, default). When false, files with the same
    /// size and modification time are assumed identical and nothing is read.
    pub compare_contents: bool,
    /// Manually paired directories `(left_rel, right_rel)`: a directory that only
    /// exists on the left and one that only exists on the right, with the same
    /// parent, compared as if they were one entry. Invalid pairs are ignored.
    pub pairs: Vec<(PathBuf, PathBuf)>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            ignore: Ignore::new(),
            follow_symlinks: false,
            compare_dates: false,
            compare_contents: true,
            pairs: Vec::new(),
        }
    }
}

pub fn rel_string(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < 4 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n}B")
    } else {
        format!("{v:.1}{}", U[i])
    }
}

// ───────────────────────────── ignore ─────────────────────────────

#[derive(Clone, Debug)]
struct Pattern {
    glob: String,
    dir_only: bool,
    anchored: bool,
    negate: bool,
}

/// Gitignore-style patterns (subset): `*`, `?`, `**`, leading `/` or an inner
/// `/` anchors to the comparison root, trailing `/` matches directories only,
/// `!` negates, `#` comments. Last matching pattern wins.
#[derive(Clone, Debug, Default)]
pub struct Ignore {
    patterns: Vec<Pattern>,
}

impl Ignore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn defaults() -> Self {
        let mut i = Self::new();
        for p in [".git", "node_modules", ".DS_Store"] {
            i.add(p);
        }
        i
    }

    pub fn add(&mut self, line: &str) {
        let mut s = line.trim();
        if s.is_empty() || s.starts_with('#') {
            return;
        }
        let negate = s.starts_with('!');
        if negate {
            s = &s[1..];
        }
        let dir_only = s.ends_with('/');
        let s = s.trim_end_matches('/');
        let anchored = s.contains('/');
        let s = s.trim_start_matches('/');
        if s.is_empty() {
            return;
        }
        self.patterns.push(Pattern { glob: s.to_string(), dir_only, anchored, negate });
    }

    pub fn add_all(&mut self, text: &str) {
        for l in text.lines() {
            self.add(l);
        }
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// The patterns as ignore-file lines (adding them again gives an equal set).
    pub fn lines(&self) -> Vec<String> {
        self.patterns
            .iter()
            .map(|p| {
                format!(
                    "{}{}{}{}",
                    if p.negate { "!" } else { "" },
                    if p.anchored { "/" } else { "" },
                    p.glob,
                    if p.dir_only { "/" } else { "" }
                )
            })
            .collect()
    }

    pub fn is_ignored(&self, rel: &Path, is_dir: bool) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        let rel_s = rel_string(rel);
        let name = rel
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut ignored = false;
        for p in &self.patterns {
            if p.dir_only && !is_dir {
                continue;
            }
            let subject = if p.anchored { rel_s.as_str() } else { name.as_str() };
            if glob_match(p.glob.as_bytes(), subject.as_bytes()) {
                ignored = !p.negate;
            }
        }
        ignored
    }
}

/// `*` and `?` never cross `/`; `**` does; `**/` also matches zero directories.
pub fn glob_match(p: &[u8], t: &[u8]) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    if p.starts_with(b"**") {
        let rest = &p[2..];
        if rest.first() == Some(&b'/') && glob_match(&rest[1..], t) {
            return true;
        }
        return (0..=t.len()).any(|i| glob_match(rest, &t[i..]));
    }
    match p[0] {
        b'*' => {
            let rest = &p[1..];
            let mut i = 0;
            loop {
                if glob_match(rest, &t[i..]) {
                    return true;
                }
                if i < t.len() && t[i] != b'/' {
                    i += 1;
                } else {
                    return false;
                }
            }
        }
        b'?' => !t.is_empty() && t[0] != b'/' && glob_match(&p[1..], &t[1..]),
        c => !t.is_empty() && t[0] == c && glob_match(&p[1..], &t[1..]),
    }
}

// ───────────────────────────── scanning ─────────────────────────────

fn meta_of(path: &Path, follow: bool) -> Option<Meta> {
    let md = if follow {
        fs::metadata(path).or_else(|_| fs::symlink_metadata(path))
    } else {
        fs::symlink_metadata(path)
    }
    .ok()?;
    Some(Meta {
        is_dir: md.is_dir(),
        is_symlink: md.file_type().is_symlink(),
        size: md.len(),
        mtime: md.modified().ok(),
    })
}

fn list_dir(path: &Path, follow: bool) -> BTreeMap<OsString, Meta> {
    let mut out = BTreeMap::new();
    if let Ok(rd) = fs::read_dir(path) {
        for e in rd.flatten() {
            if let Some(m) = meta_of(&e.path(), follow) {
                out.insert(e.file_name(), m);
            }
        }
    }
    out
}

fn fill(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let k = r.read(&mut buf[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

/// Streaming byte-for-byte comparison (exact, unlike a hash, and it stops at
/// the first difference).
pub fn files_equal(a: &Path, b: &Path) -> io::Result<bool> {
    let mut fa = File::open(a)?;
    let mut fb = File::open(b)?;
    let mut ba = vec![0u8; 64 * 1024];
    let mut bb = vec![0u8; 64 * 1024];
    loop {
        let na = fill(&mut fa, &mut ba)?;
        let nb = fill(&mut fb, &mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

fn same_time(a: SystemTime, b: SystemTime) -> bool {
    let d = match a.duration_since(b) {
        Ok(d) => d,
        Err(e) => e.duration(),
    };
    d.as_secs() < 2 // tolerate FAT-style 2s granularity
}

fn file_status(a: &Meta, b: &Meta, lp: &Path, rp: &Path, opts: &Opts) -> Status {
    if a.is_symlink || b.is_symlink {
        if a.is_symlink && b.is_symlink {
            return if fs::read_link(lp).ok() == fs::read_link(rp).ok() {
                Status::Same
            } else {
                Status::Modified
            };
        }
        return Status::Modified;
    }
    if a.size != b.size {
        return Status::Modified;
    }
    if opts.compare_dates {
        if let (Some(x), Some(y)) = (a.mtime, b.mtime) {
            if !same_time(x, y) {
                return Status::Modified;
            }
        }
    }
    if !opts.compare_contents {
        return match (a.mtime, b.mtime) {
            (Some(x), Some(y)) if same_time(x, y) => Status::Same,
            _ => Status::Modified,
        };
    }
    match files_equal(lp, rp) {
        Ok(true) => Status::Same,
        _ => Status::Modified,
    }
}

const MAX_DEPTH: usize = 128;

/// Pairs that apply in the directory at (`lrel`, `rrel`): both names must exist
/// on exactly one side each and be the same kind (two directories or two
/// files), otherwise the pair is stale.
fn pairs_here(
    lm: &BTreeMap<OsString, Meta>,
    rm: &BTreeMap<OsString, Meta>,
    lrel: &Path,
    rrel: &Path,
    opts: &Opts,
) -> Vec<(OsString, OsString)> {
    let mut out: Vec<(OsString, OsString)> = Vec::new();
    for (pl, pr) in &opts.pairs {
        if pl.parent() != Some(lrel) || pr.parent() != Some(rrel) {
            continue;
        }
        let (Some(ln), Some(rn)) = (pl.file_name(), pr.file_name()) else {
            continue;
        };
        let (ln, rn) = (ln.to_os_string(), rn.to_os_string());
        let same_kind = match (lm.get(&ln), rm.get(&rn)) {
            (Some(a), Some(b)) => a.is_dir == b.is_dir,
            _ => false,
        };
        let ok = same_kind
            && !rm.contains_key(&ln)
            && !lm.contains_key(&rn);
        if ok && !out.iter().any(|(a, b)| *a == ln || *b == rn || *a == rn || *b == ln) {
            out.push((ln, rn));
        }
    }
    out
}

fn compare_children(
    l: Option<&Path>,
    r: Option<&Path>,
    lrel: &Path,
    rrel: &Path,
    opts: &Opts,
    depth: usize,
) -> Vec<Node> {
    if depth > MAX_DEPTH {
        return Vec::new();
    }
    let lm = l.map(|p| list_dir(p, opts.follow_symlinks)).unwrap_or_default();
    let rm = r.map(|p| list_dir(p, opts.follow_symlinks)).unwrap_or_default();

    // key (sort position) -> (name on the left, name on the right)
    let mut entries: BTreeMap<OsString, (Option<OsString>, Option<OsString>)> = BTreeMap::new();
    let paired = pairs_here(&lm, &rm, lrel, rrel, opts);
    let is_paired_name = |n: &OsString| paired.iter().any(|(a, b)| a == n || b == n);
    for name in lm.keys().chain(rm.keys()) {
        if is_paired_name(name) {
            continue;
        }
        entries.insert(
            name.clone(),
            (lm.contains_key(name).then(|| name.clone()), rm.contains_key(name).then(|| name.clone())),
        );
    }
    for (ln, rn) in &paired {
        entries.insert(ln.clone(), (Some(ln.clone()), Some(rn.clone())));
    }

    let mut out = Vec::new();
    for (_, (lname, rname)) in entries {
        let lmeta = lname.as_ref().and_then(|n| lm.get(n)).cloned();
        let rmeta = rname.as_ref().and_then(|n| rm.get(n)).cloned();
        let child_l = lname.clone().or_else(|| rname.clone()).unwrap();
        let child_r = rname.clone().or_else(|| lname.clone()).unwrap();
        let child_lrel = lrel.join(&child_l);
        let child_rrel = rrel.join(&child_r);
        let any_dir = lmeta.as_ref().map_or(false, |m| m.is_dir)
            || rmeta.as_ref().map_or(false, |m| m.is_dir);
        if opts.ignore.is_ignored(&child_lrel, any_dir) {
            continue;
        }
        let lp = match (&lname, l) {
            (Some(n), Some(base)) => Some(base.join(n)),
            _ => None,
        };
        let rp = match (&rname, r) {
            (Some(n), Some(base)) => Some(base.join(n)),
            _ => None,
        };
        let mut children = Vec::new();
        let status = match (&lmeta, &rmeta) {
            (Some(a), None) => {
                if a.is_dir {
                    children = compare_children(
                        lp.as_deref(),
                        None,
                        &child_lrel,
                        &child_rrel,
                        opts,
                        depth + 1,
                    );
                }
                Status::LeftOnly
            }
            (None, Some(b)) => {
                if b.is_dir {
                    children = compare_children(
                        None,
                        rp.as_deref(),
                        &child_lrel,
                        &child_rrel,
                        opts,
                        depth + 1,
                    );
                }
                Status::RightOnly
            }
            (Some(a), Some(b)) => {
                if a.is_dir && b.is_dir {
                    children = compare_children(
                        lp.as_deref(),
                        rp.as_deref(),
                        &child_lrel,
                        &child_rrel,
                        opts,
                        depth + 1,
                    );
                    if children.iter().all(|c| c.status == Status::Same) {
                        Status::Same
                    } else {
                        Status::Modified
                    }
                } else if a.is_dir != b.is_dir {
                    Status::Modified
                } else {
                    file_status(a, b, lp.as_deref().unwrap(), rp.as_deref().unwrap(), opts)
                }
            }
            (None, None) => Status::Same,
        };
        let name = match (&lname, &rname) {
            (Some(a), Some(b)) if a != b => format!("{} \u{21c4} {}", a.to_string_lossy(), b.to_string_lossy()),
            _ => child_l.to_string_lossy().into_owned(),
        };
        out.push(Node {
            name,
            rel: child_lrel,
            rrel: child_rrel,
            left: lmeta,
            right: rmeta,
            status,
            children,
        });
    }
    out
}

/// Compare two directory trees. The returned root has an empty name/rel.
pub fn compare(left: &Path, right: &Path, opts: &Opts) -> Node {
    let children =
        compare_children(Some(left), Some(right), Path::new(""), Path::new(""), opts, 0);
    let status = if children.iter().all(|c| c.status == Status::Same) {
        Status::Same
    } else {
        Status::Modified
    };
    Node {
        name: String::new(),
        rel: PathBuf::new(),
        rrel: PathBuf::new(),
        left: meta_of(left, true),
        right: meta_of(right, true),
        status,
        children,
    }
}

// ───────────────────────────── sorting ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Mtime,
    Status,
}

impl SortKey {
    pub fn next(self) -> Self {
        match self {
            SortKey::Name => SortKey::Size,
            SortKey::Size => SortKey::Mtime,
            SortKey::Mtime => SortKey::Status,
            SortKey::Status => SortKey::Name,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Mtime => "date",
            SortKey::Status => "status",
        }
    }
}

fn node_size(n: &Node) -> u64 {
    let l = n.left.as_ref().map_or(0, |m| m.size);
    let r = n.right.as_ref().map_or(0, |m| m.size);
    l.max(r)
}

fn node_mtime(n: &Node) -> Option<SystemTime> {
    let l = n.left.as_ref().and_then(|m| m.mtime);
    let r = n.right.as_ref().and_then(|m| m.mtime);
    l.max(r)
}

fn status_rank(s: Status) -> u8 {
    match s {
        Status::Modified => 0,
        Status::LeftOnly => 1,
        Status::RightOnly => 2,
        Status::Same => 3,
    }
}

fn cmp_name(a: &Node, b: &Node) -> std::cmp::Ordering {
    a.name
        .to_lowercase()
        .cmp(&b.name.to_lowercase())
        .then_with(|| a.name.cmp(&b.name))
}

pub fn sort_tree(node: &mut Node, key: SortKey, dirs_first: bool) {
    node.children.sort_by(|a, b| {
        if dirs_first && a.is_dir() != b.is_dir() {
            return b.is_dir().cmp(&a.is_dir());
        }
        let o = match key {
            SortKey::Name => cmp_name(a, b),
            SortKey::Size => node_size(b).cmp(&node_size(a)),
            SortKey::Mtime => node_mtime(b).cmp(&node_mtime(a)),
            SortKey::Status => status_rank(a.status).cmp(&status_rank(b.status)),
        };
        o.then_with(|| cmp_name(a, b))
    });
    for c in &mut node.children {
        sort_tree(c, key, dirs_first);
    }
}

// ───────────────────────────── stats ─────────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub modified: usize,
    pub left_only: usize,
    pub right_only: usize,
    pub identical: usize,
}

/// Counts files (and file-vs-dir clashes); directories themselves are not counted.
pub fn stats(n: &Node) -> Stats {
    let mut s = Stats::default();
    fn rec(n: &Node, s: &mut Stats) {
        for c in &n.children {
            if c.is_dir() {
                rec(c, s);
            } else {
                match c.status {
                    Status::Same => s.identical += 1,
                    Status::Modified => s.modified += 1,
                    Status::LeftOnly => s.left_only += 1,
                    Status::RightOnly => s.right_only += 1,
                }
            }
        }
    }
    rec(n, &mut s);
    s
}

// ───────────────────────────── line diff ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Equal,
    Delete,
    Insert,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    pub op: Op,
    /// Index into the left sequence (valid for Equal/Delete).
    pub a: usize,
    /// Index into the right sequence (valid for Equal/Insert).
    pub b: usize,
}

/// Beyond this edit distance (after trimming common prefix/suffix) we give up
/// on a minimal diff and report the middle section as replaced.
const MAX_D: usize = 2000;

fn myers(a: &[&str], b: &[&str]) -> Option<Vec<Edit>> {
    let n = a.len() as i32;
    let m = b.len() as i32;
    let dmax = ((n + m) as usize).min(MAX_D) as i32;
    let off = dmax + 1;
    let mut v = vec![0i32; (2 * dmax + 3) as usize];
    // trace[d] = window v[-d..=d] as it was before round d.
    let mut trace: Vec<Vec<i32>> = Vec::new();
    let mut found = false;
    'outer: for d in 0..=dmax {
        trace.push(v[(off - d) as usize..=(off + d) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let ki = (off + k) as usize;
            let mut x = if k == -d || (k != d && v[ki - 1] < v[ki + 1]) {
                v[ki + 1]
            } else {
                v[ki - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[ki] = x;
            if x >= n && y >= m {
                found = true;
                break 'outer;
            }
            k += 2;
        }
    }
    if !found {
        return None;
    }

    let mut x = n;
    let mut y = m;
    let mut ops: Vec<Edit> = Vec::new();
    for d in (0..trace.len() as i32).rev() {
        if d == 0 {
            while x > 0 && y > 0 {
                x -= 1;
                y -= 1;
                ops.push(Edit { op: Op::Equal, a: x as usize, b: y as usize });
            }
            break;
        }
        let w = &trace[d as usize];
        let at = |k: i32| -> i32 { w[(k + d) as usize] };
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) { k + 1 } else { k - 1 };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            ops.push(Edit { op: Op::Equal, a: x as usize, b: y as usize });
        }
        if x == prev_x {
            ops.push(Edit { op: Op::Insert, a: prev_x as usize, b: prev_y as usize });
        } else {
            ops.push(Edit { op: Op::Delete, a: prev_x as usize, b: prev_y as usize });
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    Some(ops)
}

pub fn diff_ops(a: &[&str], b: &[&str]) -> Vec<Edit> {
    let (n, m) = (a.len(), b.len());
    let mut p = 0;
    while p < n && p < m && a[p] == b[p] {
        p += 1;
    }
    let mut s = 0;
    while s < n - p && s < m - p && a[n - 1 - s] == b[m - 1 - s] {
        s += 1;
    }
    let am = &a[p..n - s];
    let bm = &b[p..m - s];

    let mut out = Vec::with_capacity(n.max(m));
    for i in 0..p {
        out.push(Edit { op: Op::Equal, a: i, b: i });
    }
    let replace_all = |am: &[&str], bm: &[&str]| -> Vec<Edit> {
        let mut v: Vec<Edit> = (0..am.len()).map(|i| Edit { op: Op::Delete, a: i, b: 0 }).collect();
        v.extend((0..bm.len()).map(|j| Edit { op: Op::Insert, a: 0, b: j }));
        v
    };
    let mid = if am.is_empty() || bm.is_empty() {
        replace_all(am, bm)
    } else {
        myers(am, bm).unwrap_or_else(|| replace_all(am, bm))
    };
    for e in mid {
        out.push(Edit { op: e.op, a: e.a + p, b: e.b + p });
    }
    for i in 0..s {
        out.push(Edit { op: Op::Equal, a: n - s + i, b: m - s + i });
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Hunk,
    Context,
    Add,
    Del,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
    pub left_no: Option<usize>,
    pub right_no: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct Diff {
    pub lines: Vec<DiffLine>,
    pub added: usize,
    pub removed: usize,
}

fn strip_eol(s: &str) -> String {
    s.trim_end_matches(|c| c == '\n' || c == '\r').to_string()
}

/// Unified diff with `ctx` lines of context. Lines are compared including
/// their terminator, so an EOL-only change is reported as a change.
pub fn unified(a: &str, b: &str, ctx: usize) -> Diff {
    let al: Vec<&str> = a.split_inclusive('\n').collect();
    let bl: Vec<&str> = b.split_inclusive('\n').collect();
    let ops = diff_ops(&al, &bl);

    let mut all: Vec<DiffLine> = Vec::with_capacity(ops.len());
    let (mut added, mut removed) = (0, 0);
    for e in &ops {
        match e.op {
            Op::Equal => all.push(DiffLine {
                kind: LineKind::Context,
                text: strip_eol(al[e.a]),
                left_no: Some(e.a + 1),
                right_no: Some(e.b + 1),
            }),
            Op::Delete => {
                removed += 1;
                all.push(DiffLine {
                    kind: LineKind::Del,
                    text: strip_eol(al[e.a]),
                    left_no: Some(e.a + 1),
                    right_no: None,
                });
            }
            Op::Insert => {
                added += 1;
                all.push(DiffLine {
                    kind: LineKind::Add,
                    text: strip_eol(bl[e.b]),
                    left_no: None,
                    right_no: Some(e.b + 1),
                });
            }
        }
    }

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (i, l) in all.iter().enumerate() {
        if l.kind == LineKind::Context {
            continue;
        }
        let s = i.saturating_sub(ctx);
        let e = (i + ctx + 1).min(all.len());
        match ranges.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => ranges.push((s, e)),
        }
    }

    let mut lines = Vec::new();
    for (s, e) in ranges {
        let slice = &all[s..e];
        let lstart = slice.iter().find_map(|l| l.left_no).unwrap_or(0);
        let rstart = slice.iter().find_map(|l| l.right_no).unwrap_or(0);
        let lcount = slice.iter().filter(|l| l.kind != LineKind::Add).count();
        let rcount = slice.iter().filter(|l| l.kind != LineKind::Del).count();
        lines.push(DiffLine {
            kind: LineKind::Hunk,
            text: format!("@@ -{lstart},{lcount} +{rstart},{rcount} @@"),
            left_no: None,
            right_no: None,
        });
        lines.extend(slice.iter().cloned());
    }
    Diff { lines, added, removed }
}

#[derive(Clone, Debug)]
pub enum FileDiff {
    Identical,
    Text(Diff),
    Binary,
    TooLarge(u64),
    TypeMismatch,
    Error(String),
}

const MAX_DIFF_BYTES: u64 = 8 * 1024 * 1024;

fn read_for_diff(p: Option<&Path>) -> Result<Vec<u8>, FileDiff> {
    match p {
        None => Ok(Vec::new()),
        Some(p) => {
            let md = fs::metadata(p)
                .map_err(|e| FileDiff::Error(format!("{}: {e}", p.display())))?;
            if md.is_dir() {
                return Err(FileDiff::TypeMismatch);
            }
            if md.len() > MAX_DIFF_BYTES {
                return Err(FileDiff::TooLarge(md.len()));
            }
            fs::read(p).map_err(|e| FileDiff::Error(format!("{}: {e}", p.display())))
        }
    }
}

fn is_binary(b: &[u8]) -> bool {
    b[..b.len().min(8192)].contains(&0)
}

/// Diff two files. A missing side (`None`) is treated as empty.
pub fn file_diff(left: Option<&Path>, right: Option<&Path>) -> FileDiff {
    let a = match read_for_diff(left) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let b = match read_for_diff(right) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let both = left.is_some() && right.is_some();
    if both && a == b {
        return FileDiff::Identical;
    }
    if is_binary(&a) || is_binary(&b) {
        return FileDiff::Binary;
    }
    let (sa, sb) = (String::from_utf8_lossy(&a), String::from_utf8_lossy(&b));
    let d = unified(&sa, &sb, 3);
    if both && d.added == 0 && d.removed == 0 {
        FileDiff::Identical
    } else {
        FileDiff::Text(d)
    }
}

// ───────────────────────────── file operations ─────────────────────────────

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(src)?, dst)
}

#[cfg(not(unix))]
fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    // Best effort on non-Unix: copy what the link points at.
    fs::copy(src, dst).map(|_| ())
}

pub fn remove_any(p: &Path) -> io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(md) if md.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p).or_else(|_| fs::remove_dir(p)),
        Err(_) => Ok(()),
    }
}

/// Copy `src` over `dst`. Directories are merged (nothing extra is deleted
/// from the destination). Entries matching `ignore` are skipped; `rel` is the
/// path of `src` relative to the comparison root.
pub fn copy_entry(
    src: &Path,
    dst: &Path,
    follow: bool,
    ignore: &Ignore,
    rel: &Path,
) -> io::Result<()> {
    let md = if follow { fs::metadata(src)? } else { fs::symlink_metadata(src)? };
    if md.file_type().is_symlink() {
        remove_any(dst)?;
        copy_symlink(src, dst)
    } else if md.is_dir() {
        if let Ok(dmd) = fs::symlink_metadata(dst) {
            if !dmd.is_dir() {
                remove_any(dst)?;
            }
        }
        fs::create_dir_all(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            let child_rel = rel.join(e.file_name());
            let is_dir = fs::metadata(e.path()).map(|m| m.is_dir()).unwrap_or(false);
            if ignore.is_ignored(&child_rel, is_dir) {
                continue;
            }
            copy_entry(&e.path(), &dst.join(e.file_name()), follow, ignore, &child_rel)?;
        }
        Ok(())
    } else {
        remove_any(dst)?;
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
        if let Ok(t) = md.modified() {
            if let Ok(f) = fs::OpenOptions::new().write(true).open(dst) {
                let _ = f.set_modified(t);
            }
        }
        Ok(())
    }
}

#[allow(dead_code)] // deletes now go through undo::Journal; kept for direct use
pub fn delete_entry(path: &Path) -> io::Result<()> {
    fs::symlink_metadata(path)?; // surface "not found"
    remove_any(path)
}

// ───────────────────────────── misc ─────────────────────────────

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

// ───────────────────────────── side-by-side merge model ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Left,
    Right,
}

impl Pane {
    pub fn other(self) -> Pane {
        match self {
            Pane::Left => Pane::Right,
            Pane::Right => Pane::Left,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Pane::Left => "left",
            Pane::Right => "right",
        }
    }
}

/// A text file split into lines, remembering its line-ending style so that
/// saving does not rewrite every line ending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextBuf {
    pub lines: Vec<String>,
    pub crlf: bool,
    pub final_nl: bool,
    pub exists: bool,
}

impl TextBuf {
    pub fn parse(text: &str) -> TextBuf {
        let crlf = text.contains("\r\n");
        if text.is_empty() {
            return TextBuf { lines: Vec::new(), crlf, final_nl: false, exists: true };
        }
        let final_nl = text.ends_with('\n');
        let body = if final_nl { &text[..text.len() - 1] } else { text };
        let lines = body
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
            .collect();
        TextBuf { lines, crlf, final_nl, exists: true }
    }

    /// Mixed line endings are normalised to the dominant style (CRLF if any).
    pub fn render(&self) -> String {
        let eol = if self.crlf { "\r\n" } else { "\n" };
        let mut s = self.lines.join(eol);
        if !self.lines.is_empty() && self.final_nl {
            s.push_str(eol);
        }
        s
    }
}

/// Load a file for side-by-side editing. `None` means "does not exist" (empty
/// buffer). Binary, non-UTF-8, oversized or non-regular files are reported as
/// the `FileDiff` the caller should show instead.
pub fn load_text(p: Option<&Path>) -> Result<TextBuf, FileDiff> {
    match p {
        None => Ok(TextBuf { lines: Vec::new(), crlf: false, final_nl: true, exists: false }),
        Some(p) => {
            let bytes = read_for_diff(Some(p))?;
            if is_binary(&bytes) {
                return Err(FileDiff::Binary);
            }
            match String::from_utf8(bytes) {
                Ok(s) => Ok(TextBuf::parse(&s)),
                Err(_) => Err(FileDiff::Binary),
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    Equal,
    /// A line on each side that differ.
    Changed,
    /// Line exists only on the left (right shows a gap).
    LeftOnly,
    /// Line exists only on the right (left shows a gap).
    RightOnly,
}

#[derive(Clone, Debug)]
pub struct SideRow {
    pub kind: RowKind,
    pub left: Option<usize>,
    pub right: Option<usize>,
    /// Number of left / right lines before this row (insertion points).
    pub lpos: usize,
    pub rpos: usize,
    pub hunk: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct Hunk {
    pub left: Range<usize>,
    pub right: Range<usize>,
    pub rows: Range<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct Align {
    pub rows: Vec<SideRow>,
    pub hunks: Vec<Hunk>,
}

/// Align two line lists into rows for a side-by-side view. Within a change
/// block, deleted and inserted lines are paired up row by row.
pub fn align(a: &[String], b: &[String]) -> Align {
    let ar: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    let br: Vec<&str> = b.iter().map(|s| s.as_str()).collect();
    let ops = diff_ops(&ar, &br);
    let mut out = Align::default();
    let (mut ai, mut bi) = (0usize, 0usize);
    let mut i = 0;
    while i < ops.len() {
        if ops[i].op == Op::Equal {
            out.rows.push(SideRow {
                kind: RowKind::Equal,
                left: Some(ai),
                right: Some(bi),
                lpos: ai,
                rpos: bi,
                hunk: None,
            });
            ai += 1;
            bi += 1;
            i += 1;
            continue;
        }
        let (sa, sb) = (ai, bi);
        let mut dels = Vec::new();
        let mut ins = Vec::new();
        while i < ops.len() && ops[i].op != Op::Equal {
            match ops[i].op {
                Op::Delete => {
                    dels.push(ai);
                    ai += 1;
                }
                Op::Insert => {
                    ins.push(bi);
                    bi += 1;
                }
                Op::Equal => {}
            }
            i += 1;
        }
        let h = out.hunks.len();
        let row_start = out.rows.len();
        for k in 0..dels.len().max(ins.len()) {
            let l = dels.get(k).copied();
            let r = ins.get(k).copied();
            let kind = match (l, r) {
                (Some(_), Some(_)) => RowKind::Changed,
                (Some(_), None) => RowKind::LeftOnly,
                _ => RowKind::RightOnly,
            };
            out.rows.push(SideRow {
                kind,
                left: l,
                right: r,
                lpos: sa + k.min(dels.len()),
                rpos: sb + k.min(ins.len()),
                hunk: Some(h),
            });
        }
        out.hunks.push(Hunk { left: sa..ai, right: sb..bi, rows: row_start..out.rows.len() });
    }
    out
}

#[derive(Clone)]
struct Snap {
    left: Vec<String>,
    right: Vec<String>,
}

const MAX_UNDO: usize = 100;

/// Two editable buffers plus their alignment, with undo/redo and saving.
pub struct Merge {
    pub left: TextBuf,
    pub right: TextBuf,
    pub align: Align,
    orig_l: Vec<String>,
    orig_r: Vec<String>,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    dirty_l: bool,
    dirty_r: bool,
    /// Bumped on every change to either side; lets callers cache derived data.
    pub rev: u64,
}

impl Merge {
    pub fn new(mut left: TextBuf, mut right: TextBuf) -> Merge {
        // A side that does not exist yet adopts the other side's line endings
        // and trailing-newline style, so a copy is byte-identical.
        if !left.exists {
            left.crlf = right.crlf;
            left.final_nl = right.final_nl;
        }
        if !right.exists {
            right.crlf = left.crlf;
            right.final_nl = left.final_nl;
        }
        let al = align(&left.lines, &right.lines);
        Merge {
            orig_l: left.lines.clone(),
            orig_r: right.lines.clone(),
            left,
            right,
            align: al,
            undo: Vec::new(),
            redo: Vec::new(),
            dirty_l: false,
            dirty_r: false,
            rev: 0,
        }
    }

    pub fn buf(&self, p: Pane) -> &TextBuf {
        match p {
            Pane::Left => &self.left,
            Pane::Right => &self.right,
        }
    }

    fn lines_mut(&mut self, p: Pane) -> &mut Vec<String> {
        match p {
            Pane::Left => &mut self.left.lines,
            Pane::Right => &mut self.right.lines,
        }
    }

    pub fn is_dirty(&self, p: Pane) -> bool {
        match p {
            Pane::Left => self.dirty_l,
            Pane::Right => self.dirty_r,
        }
    }

    fn snap(&self) -> Snap {
        Snap { left: self.left.lines.clone(), right: self.right.lines.clone() }
    }

    fn recompute(&mut self) {
        self.rev += 1;
        self.align = align(&self.left.lines, &self.right.lines);
        self.dirty_l = self.left.lines != self.orig_l;
        self.dirty_r = self.right.lines != self.orig_r;
    }

    fn commit(&mut self, before: Snap) {
        self.undo.push(before);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.recompute();
    }

    pub fn hunk_at(&self, row: usize) -> Option<usize> {
        self.align.rows.get(row).and_then(|r| r.hunk)
    }

    /// First row of the next (or previous) hunk relative to `row`.
    pub fn next_hunk_row(&self, row: usize, forward: bool) -> Option<usize> {
        let starts = self.align.hunks.iter().map(|h| h.rows.start);
        if forward {
            starts.filter(|&s| s > row).next()
        } else {
            let cur = self
                .hunk_at(row)
                .map(|h| self.align.hunks[h].rows.start)
                .unwrap_or(row);
            starts.filter(|&s| s < cur).last()
        }
    }

    /// Replace the other side's part of hunk `h` with `from`'s part.
    pub fn put_hunk(&mut self, h: usize, from: Pane) -> bool {
        let Some(hk) = self.align.hunks.get(h).cloned() else {
            return false;
        };
        let (src_range, dst_range) = match from {
            Pane::Left => (hk.left, hk.right),
            Pane::Right => (hk.right, hk.left),
        };
        let src: Vec<String> = self.buf(from).lines[src_range].to_vec();
        let before = self.snap();
        self.adopt_style_if_empty(from);
        self.lines_mut(from.other()).splice(dst_range, src);
        self.commit(before);
        true
    }

    /// When lines are copied into a buffer that has none, it takes over the
    /// source's line-ending and trailing-newline style.
    fn adopt_style_if_empty(&mut self, from: Pane) {
        let (crlf, final_nl) = {
            let b = self.buf(from);
            (b.crlf, b.final_nl)
        };
        let to = match from {
            Pane::Left => &mut self.right,
            Pane::Right => &mut self.left,
        };
        if to.lines.is_empty() {
            to.crlf = crlf;
            to.final_nl = final_nl;
        }
    }

    /// Make this row on the other side equal to `from`'s: overwrite a differing
    /// line, insert a line the other side lacks, or remove a line `from` lacks.
    pub fn put_line(&mut self, row: usize, from: Pane) -> bool {
        let Some(r) = self.align.rows.get(row).cloned() else {
            return false;
        };
        let (src_idx, dst_idx, dst_pos) = match from {
            Pane::Left => (r.left, r.right, r.rpos),
            Pane::Right => (r.right, r.left, r.lpos),
        };
        let to = from.other();
        match (src_idx, dst_idx) {
            (Some(s), Some(d)) => {
                let text = self.buf(from).lines[s].clone();
                if self.buf(to).lines[d] == text {
                    return false;
                }
                let before = self.snap();
                self.lines_mut(to)[d] = text;
                self.commit(before);
            }
            (Some(s), None) => {
                let text = self.buf(from).lines[s].clone();
                let before = self.snap();
                self.adopt_style_if_empty(from);
                self.lines_mut(to).insert(dst_pos, text);
                self.commit(before);
            }
            (None, Some(d)) => {
                let before = self.snap();
                self.lines_mut(to).remove(d);
                self.commit(before);
            }
            (None, None) => return false,
        }
        true
    }

    /// Change the line at `row` on `pane`; on a gap this inserts a new line there.
    pub fn set_line(&mut self, row: usize, pane: Pane, text: String) -> bool {
        let (idx, pos) = match self.align.rows.get(row) {
            Some(r) => match pane {
                Pane::Left => (r.left, r.lpos),
                Pane::Right => (r.right, r.rpos),
            },
            None => (None, self.buf(pane).lines.len()),
        };
        if let Some(i) = idx {
            if self.buf(pane).lines[i] == text {
                return false;
            }
        }
        let before = self.snap();
        if self.buf(pane).lines.is_empty() {
            self.set_final_nl(pane, true);
        }
        let lines = self.lines_mut(pane);
        match idx {
            Some(i) => lines[i] = text,
            None => lines.insert(pos, text),
        }
        self.commit(before);
        true
    }

    /// Insert a new line on `pane` after the line shown at `row`.
    pub fn insert_after(&mut self, row: usize, pane: Pane, text: String) -> bool {
        let pos = match self.align.rows.get(row) {
            Some(r) => match pane {
                Pane::Left => r.left.map(|i| i + 1).unwrap_or(r.lpos),
                Pane::Right => r.right.map(|i| i + 1).unwrap_or(r.rpos),
            },
            None => self.buf(pane).lines.len(),
        };
        let before = self.snap();
        if self.buf(pane).lines.is_empty() {
            self.set_final_nl(pane, true);
        }
        self.lines_mut(pane).insert(pos, text);
        self.commit(before);
        true
    }

    fn set_final_nl(&mut self, p: Pane, v: bool) {
        match p {
            Pane::Left => self.left.final_nl = v,
            Pane::Right => self.right.final_nl = v,
        }
    }

    pub fn delete_line(&mut self, row: usize, pane: Pane) -> bool {
        let idx = match self.align.rows.get(row) {
            Some(r) => match pane {
                Pane::Left => r.left,
                Pane::Right => r.right,
            },
            None => None,
        };
        let Some(i) = idx else {
            return false;
        };
        let before = self.snap();
        self.lines_mut(pane).remove(i);
        self.commit(before);
        true
    }

    pub fn undo(&mut self) -> bool {
        let Some(s) = self.undo.pop() else {
            return false;
        };
        let cur = self.snap();
        self.redo.push(cur);
        self.left.lines = s.left;
        self.right.lines = s.right;
        self.recompute();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(s) = self.redo.pop() else {
            return false;
        };
        let cur = self.snap();
        self.undo.push(cur);
        self.left.lines = s.left;
        self.right.lines = s.right;
        self.recompute();
        true
    }

    /// Write every modified side (creating a file that did not exist). Returns
    /// the panes that were written.
    pub fn save(&mut self, lpath: &Path, rpath: &Path) -> io::Result<Vec<Pane>> {
        fn write_buf(p: &Path, b: &TextBuf) -> io::Result<()> {
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(p, b.render())
        }
        let mut written = Vec::new();
        let mut err: Option<io::Error> = None;
        for pane in [Pane::Left, Pane::Right] {
            if !self.is_dirty(pane) {
                continue;
            }
            let path = if pane == Pane::Left { lpath } else { rpath };
            match write_buf(path, self.buf(pane)) {
                Ok(()) => {
                    match pane {
                        Pane::Left => {
                            self.orig_l = self.left.lines.clone();
                            self.left.exists = true;
                        }
                        Pane::Right => {
                            self.orig_r = self.right.lines.clone();
                            self.right.exists = true;
                        }
                    }
                    written.push(pane);
                }
                Err(e) => {
                    if err.is_none() {
                        err = Some(e);
                    }
                }
            }
        }
        self.recompute();
        match err {
            Some(e) => Err(e),
            None => Ok(written),
        }
    }
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn tmp() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("dirrcmp-test-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn ignore_lines_roundtrip() {
        let mut a = Ignore::new();
        a.add_all("*.log\n/build/\ndocs/*.tmp\n!keep.log\n.git\n# c\n\n");
        let lines = a.lines();
        assert_eq!(lines, vec!["*.log", "/build/", "/docs/*.tmp", "!keep.log", ".git"]);
        let mut b = Ignore::new();
        for l in &lines {
            b.add(l);
        }
        assert_eq!(b.lines(), lines);
        for (p, d) in [("x/a.log", false), ("build", true), ("sub/build", true), ("docs/a.tmp", false), ("x/keep.log", false)] {
            assert_eq!(a.is_ignored(Path::new(p), d), b.is_ignored(Path::new(p), d), "{p}");
        }
    }

    #[test]
    fn globs() {
        assert!(glob_match(b"*.rs", b"main.rs"));
        assert!(!glob_match(b"*.rs", b"a/main.rs"));
        assert!(glob_match(b"**/*.rs", b"a/b/main.rs"));
        assert!(glob_match(b"**/*.rs", b"main.rs"));
        assert!(glob_match(b"a?c", b"abc"));
        assert!(!glob_match(b"a?c", b"a/c"));
        assert!(glob_match(b"src/**", b"src/x/y"));
        assert!(!glob_match(b"abc", b"abcd"));
    }

    #[test]
    fn ignore_rules() {
        let mut ig = Ignore::defaults();
        ig.add_all("# comment\n*.log\n/build/\ndocs/*.tmp\n!keep.log\n");
        assert!(ig.is_ignored(Path::new("a/node_modules"), true));
        assert!(ig.is_ignored(Path::new(".DS_Store"), false));
        assert!(ig.is_ignored(Path::new("x/y.log"), false));
        assert!(!ig.is_ignored(Path::new("x/keep.log"), false));
        assert!(ig.is_ignored(Path::new("build"), true));
        assert!(!ig.is_ignored(Path::new("build"), false)); // dir-only
        assert!(!ig.is_ignored(Path::new("sub/build"), true)); // anchored
        assert!(ig.is_ignored(Path::new("docs/a.tmp"), false));
        assert!(!ig.is_ignored(Path::new("other/a.tmp"), false));
    }

    fn lcs_len(a: &[&str], b: &[&str]) -> usize {
        let mut dp = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                dp[i][j] = if a[i - 1] == b[j - 1] {
                    dp[i - 1][j - 1] + 1
                } else {
                    dp[i - 1][j].max(dp[i][j - 1])
                };
            }
        }
        dp[a.len()][b.len()]
    }

    #[test]
    fn myers_is_minimal_and_valid() {
        let mut s = 12345u64;
        let mut rnd = |m: usize| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 33) as usize) % m
        };
        let alpha = ["a", "b", "c", "d"];
        for _ in 0..3000 {
            let n = rnd(14);
            let m = rnd(14);
            let a: Vec<&str> = (0..n).map(|_| alpha[rnd(4)]).collect();
            let b: Vec<&str> = (0..m).map(|_| alpha[rnd(4)]).collect();
            let ops = diff_ops(&a, &b);
            let (mut ia, mut ib) = (0, 0);
            let mut eq = 0;
            for e in &ops {
                match e.op {
                    Op::Equal => {
                        assert_eq!((e.a, e.b), (ia, ib), "{a:?} {b:?}");
                        assert_eq!(a[e.a], b[e.b]);
                        ia += 1;
                        ib += 1;
                        eq += 1;
                    }
                    Op::Delete => {
                        assert_eq!(e.a, ia, "{a:?} {b:?}");
                        ia += 1;
                    }
                    Op::Insert => {
                        assert_eq!(e.b, ib, "{a:?} {b:?}");
                        ib += 1;
                    }
                }
            }
            assert_eq!((ia, ib), (a.len(), b.len()), "{a:?} {b:?}");
            assert_eq!(eq, lcs_len(&a, &b), "not minimal: {a:?} {b:?}");
        }
    }

    #[test]
    fn unified_output() {
        let a = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
        let b = "one\ntwo\nTHREE\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\n";
        let d = unified(a, b, 1);
        assert_eq!((d.added, d.removed), (2, 1));
        let hunks = d.lines.iter().filter(|l| l.kind == LineKind::Hunk).count();
        assert_eq!(hunks, 2);
        assert!(d.lines.iter().any(|l| l.kind == LineKind::Del && l.text == "three"));
        assert!(d.lines.iter().any(|l| l.kind == LineKind::Add && l.text == "eleven"));
        assert_eq!(unified("x\n", "x\n", 3).lines.len(), 0);
    }

    #[test]
    fn large_diff_falls_back() {
        let a: Vec<String> = (0..6000).map(|i| format!("a{i}")).collect();
        let b: Vec<String> = (0..6000).map(|i| format!("b{i}")).collect();
        let ar: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
        let br: Vec<&str> = b.iter().map(|s| s.as_str()).collect();
        let ops = diff_ops(&ar, &br);
        assert_eq!(ops.iter().filter(|e| e.op == Op::Delete).count(), 6000);
        assert_eq!(ops.iter().filter(|e| e.op == Op::Insert).count(), 6000);
    }

    #[test]
    fn compare_trees() {
        let root = tmp();
        let (l, r) = (root.join("l"), root.join("r"));
        write(&l.join("same.txt"), "x");
        write(&r.join("same.txt"), "x");
        write(&l.join("mod.txt"), "aaa");
        write(&r.join("mod.txt"), "bbb");
        write(&l.join("only_l/f.txt"), "1");
        write(&r.join("only_r.txt"), "2");
        write(&l.join("sub/deep/a.txt"), "same");
        write(&r.join("sub/deep/a.txt"), "same");
        write(&l.join("sub/deep/b.txt"), "left");
        write(&r.join("sub/deep/b.txt"), "right!");
        write(&l.join("same_dir/f"), "q");
        write(&r.join("same_dir/f"), "q");
        write(&l.join("node_modules/x"), "ignored");
        write(&l.join("clash"), "file");
        write(&r.join("clash/inner"), "dir");

        let opts = Opts { ignore: Ignore::defaults(), ..Opts::default() };
        let mut t = compare(&l, &r, &opts);
        sort_tree(&mut t, SortKey::Name, true);
        assert_eq!(t.status, Status::Modified);
        let get = |n: &Node, name: &str| n.children.iter().find(|c| c.name == name).cloned();
        assert_eq!(get(&t, "same.txt").unwrap().status, Status::Same);
        assert_eq!(get(&t, "mod.txt").unwrap().status, Status::Modified);
        let ol = get(&t, "only_l").unwrap();
        assert_eq!(ol.status, Status::LeftOnly);
        assert!(ol.is_dir());
        assert_eq!(ol.children.len(), 1);
        assert_eq!(ol.children[0].status, Status::LeftOnly);
        assert_eq!(get(&t, "only_r.txt").unwrap().status, Status::RightOnly);
        assert_eq!(get(&t, "same_dir").unwrap().status, Status::Same);
        assert!(get(&t, "node_modules").is_none());
        let clash = get(&t, "clash").unwrap();
        assert_eq!(clash.status, Status::Modified);
        assert!(!clash.is_dir());
        let sub = get(&t, "sub").unwrap();
        assert_eq!(sub.status, Status::Modified);
        let deep = get(&sub, "deep").unwrap();
        assert_eq!(deep.status, Status::Modified);
        assert_eq!(get(&deep, "a.txt").unwrap().status, Status::Same);
        assert_eq!(get(&deep, "b.txt").unwrap().status, Status::Modified);
        // dirs first
        assert!(t.children[0].is_dir());

        let st = stats(&t);
        assert_eq!(st.left_only, 1); // only_l/f.txt
        assert_eq!(st.right_only, 1); // only_r.txt
        assert_eq!(st.modified, 3); // mod.txt, clash, b.txt
        assert_eq!(st.identical, 3); // same.txt, a.txt, same_dir/f

        // without ignore, node_modules appears
        let t2 = compare(&l, &r, &Opts::default());
        assert!(get(&t2, "node_modules").is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn quick_mode_trusts_size_and_mtime() {
        let root = tmp();
        let (l, r) = (root.join("l"), root.join("r"));
        write(&l.join("f"), "aaaa");
        write(&r.join("f"), "bbbb"); // same size, different content
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let set = |p: &Path, t: SystemTime| {
            fs::OpenOptions::new().write(true).open(p).unwrap().set_modified(t).unwrap();
        };
        set(&l.join("f"), t);
        set(&r.join("f"), t);
        let full = compare(&l, &r, &Opts::default());
        assert_eq!(full.children[0].status, Status::Modified);
        let quick = Opts { compare_contents: false, ..Opts::default() };
        assert_eq!(compare(&l, &r, &quick).children[0].status, Status::Same);
        // different mtime (beyond the 2s tolerance) -> modified without reading
        set(&r.join("f"), t + std::time::Duration::from_secs(60));
        assert_eq!(compare(&l, &r, &quick).children[0].status, Status::Modified);
        // different size is always modified
        write(&r.join("f"), "longer");
        set(&r.join("f"), t);
        assert_eq!(compare(&l, &r, &quick).children[0].status, Status::Modified);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn big_files_differ_at_end() {
        let root = tmp();
        let mut a = vec![7u8; 300_000];
        let b = a.clone();
        fs::write(root.join("a"), &a).unwrap();
        fs::write(root.join("b"), &b).unwrap();
        assert!(files_equal(&root.join("a"), &root.join("b")).unwrap());
        *a.last_mut().unwrap() = 8;
        fs::write(root.join("a"), &a).unwrap();
        assert!(!files_equal(&root.join("a"), &root.join("b")).unwrap());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn file_diff_kinds() {
        let root = tmp();
        write(&root.join("t1"), "a\nb\n");
        write(&root.join("t2"), "a\nc\n");
        fs::write(root.join("bin"), [0u8, 1, 2]).unwrap();
        assert!(matches!(
            file_diff(Some(&root.join("t1")), Some(&root.join("t2"))),
            FileDiff::Text(_)
        ));
        assert!(matches!(
            file_diff(Some(&root.join("t1")), Some(&root.join("t1"))),
            FileDiff::Identical
        ));
        assert!(matches!(
            file_diff(Some(&root.join("t1")), Some(&root.join("bin"))),
            FileDiff::Binary
        ));
        assert!(matches!(file_diff(Some(&root), Some(&root.join("t1"))), FileDiff::TypeMismatch));
        match file_diff(None, Some(&root.join("t1"))) {
            FileDiff::Text(d) => assert_eq!((d.added, d.removed), (2, 0)),
            _ => panic!(),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_and_delete() {
        let root = tmp();
        let (l, r) = (root.join("l"), root.join("r"));
        write(&l.join("d/a.txt"), "A");
        write(&l.join("d/skip.log"), "S");
        write(&l.join("d/sub/b.txt"), "B");
        write(&r.join("d/extra.txt"), "E");
        let mut ig = Ignore::new();
        ig.add("*.log");
        copy_entry(&l.join("d"), &r.join("d"), false, &ig, Path::new("d")).unwrap();
        assert_eq!(fs::read_to_string(r.join("d/a.txt")).unwrap(), "A");
        assert_eq!(fs::read_to_string(r.join("d/sub/b.txt")).unwrap(), "B");
        assert!(r.join("d/extra.txt").exists(), "merge must not delete extras");
        assert!(!r.join("d/skip.log").exists(), "ignored entries are not copied");

        // mtime preserved on file copy
        let m1 = fs::metadata(l.join("d/a.txt")).unwrap().modified().unwrap();
        let m2 = fs::metadata(r.join("d/a.txt")).unwrap().modified().unwrap();
        assert!(same_time(m1, m2));

        // file replacing a directory
        write(&l.join("x"), "file");
        write(&r.join("x/inner"), "i");
        copy_entry(&l.join("x"), &r.join("x"), false, &Ignore::new(), Path::new("x")).unwrap();
        assert!(r.join("x").is_file());

        delete_entry(&r.join("d")).unwrap();
        assert!(!r.join("d").exists());
        assert!(delete_entry(&r.join("nope")).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn base64_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(12), "12B");
        assert_eq!(human_size(1536), "1.5K");
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir()
            .join(format!("dirrcmp-merge-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn v(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn merge(a: &str, b: &str) -> Merge {
        let mk = |s: &str| TextBuf { lines: v(s), crlf: false, final_nl: true, exists: true };
        Merge::new(mk(a), mk(b))
    }

    #[test]
    fn textbuf_roundtrip() {
        for t in ["", "a", "a\n", "a\nb", "a\nb\n", "a\r\nb\r\n", "a\r\nb", "\n", "\n\n", "a\n\nb\n"] {
            assert_eq!(TextBuf::parse(t).render(), t, "{t:?}");
        }
        assert_eq!(TextBuf::parse("a\r\nb\n").render(), "a\r\nb\r\n"); // normalised
        assert_eq!(TextBuf::parse("a\n\n").lines, vec!["a".to_string(), "".to_string()]);
    }

    #[test]
    fn alignment() {
        let m = merge("a b c d", "a X c d e");
        let kinds: Vec<RowKind> = m.align.rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![RowKind::Equal, RowKind::Changed, RowKind::Equal, RowKind::Equal, RowKind::RightOnly]
        );
        assert_eq!(m.align.hunks.len(), 2);
        assert_eq!(m.align.hunks[0].left, 1..2);
        assert_eq!(m.align.hunks[0].right, 1..2);
        assert_eq!(m.align.hunks[1].left, 4..4);
        assert_eq!(m.align.hunks[1].right, 4..5);
        assert_eq!(m.hunk_at(0), None);
        assert_eq!(m.hunk_at(1), Some(0));
        assert_eq!(m.next_hunk_row(0, true), Some(1));
        assert_eq!(m.next_hunk_row(1, true), Some(4));
        assert_eq!(m.next_hunk_row(4, true), None);
        assert_eq!(m.next_hunk_row(4, false), Some(1));
        assert_eq!(m.next_hunk_row(1, false), None);
        // uneven block: 3 deleted vs 1 inserted pairs the first, rest are gaps
        let m = merge("x p q r y", "x Z y");
        let kinds: Vec<RowKind> = m.align.rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![RowKind::Equal, RowKind::Changed, RowKind::LeftOnly, RowKind::LeftOnly, RowKind::Equal]
        );
        assert_eq!(m.align.rows[3].rpos, 2);
    }

    #[test]
    fn hunk_and_line_ops() {
        let mut m = merge("a b c d", "a X c d e");
        assert!(m.put_hunk(0, Pane::Left)); // right gets b instead of X
        assert_eq!(m.right.lines, v("a b c d e"));
        assert!(m.is_dirty(Pane::Right) && !m.is_dirty(Pane::Left));
        assert_eq!(m.align.hunks.len(), 1);
        assert!(m.put_hunk(0, Pane::Left)); // left has nothing there: removes e
        assert_eq!(m.right.lines, v("a b c d"));
        assert!(m.is_dirty(Pane::Right)); // still differs from what was loaded
        assert!(m.align.hunks.is_empty());
        assert!(!m.put_hunk(0, Pane::Left)); // no such hunk any more

        // put_line: overwrite, insert, remove
        let mut m = merge("a b c", "a X c d");
        assert!(m.put_line(1, Pane::Left)); // X -> b
        assert_eq!(m.right.lines, v("a b c d"));
        assert!(!m.put_line(1, Pane::Left)); // already equal
        let last = m.align.rows.len() - 1;
        assert_eq!(m.align.rows[last].kind, RowKind::RightOnly);
        assert!(m.put_line(last, Pane::Right)); // insert d on the left
        assert_eq!(m.left.lines, v("a b c d"));
        assert!(m.align.hunks.is_empty());

        let mut m = merge("a b c", "a c");
        let row = m.align.rows.iter().position(|r| r.kind == RowKind::LeftOnly).unwrap();
        assert!(m.put_line(row, Pane::Right)); // right has a gap: removes b on the left
        assert_eq!(m.left.lines, v("a c"));

        // inserting into the middle of a gap lands in the right place
        let mut m = merge("x p q r y", "x Z y");
        let row = m.align.rows.iter().position(|r| r.left == Some(3)).unwrap(); // r
        assert!(m.put_line(row, Pane::Left));
        assert_eq!(m.right.lines, v("x Z r y"));
    }

    #[test]
    fn editing() {
        let mut m = merge("a b c", "a b c");
        assert!(m.set_line(1, Pane::Right, "B".into()));
        assert_eq!(m.right.lines, v("a B c"));
        assert!(!m.set_line(0, Pane::Right, "a".into())); // unchanged
        let row = m.align.rows.iter().position(|r| r.kind == RowKind::Changed).unwrap();
        assert!(m.insert_after(row, Pane::Left, "new".into()));
        assert_eq!(m.left.lines, v("a b new c"));
        assert!(m.delete_line(0, Pane::Left));
        assert_eq!(m.left.lines, v("b new c"));
        // editing a gap row inserts a line there
        let mut m = merge("a b", "a");
        let gap = m.align.rows.iter().position(|r| r.right.is_none()).unwrap();
        assert!(m.set_line(gap, Pane::Right, "b".into()));
        assert_eq!(m.right.lines, v("a b"));
        assert!(m.align.hunks.is_empty());
        assert!(!m.delete_line(99, Pane::Left));
    }

    #[test]
    fn undo_redo() {
        let mut m = merge("a b c", "a X c");
        let orig = (m.left.lines.clone(), m.right.lines.clone());
        m.put_hunk(0, Pane::Left);
        m.set_line(0, Pane::Left, "Q".into());
        assert!(m.undo());
        assert_eq!(m.left.lines, v("a b c"));
        assert_eq!(m.right.lines, v("a b c"));
        assert!(m.undo());
        assert_eq!((m.left.lines.clone(), m.right.lines.clone()), orig);
        assert!(!m.is_dirty(Pane::Left) && !m.is_dirty(Pane::Right));
        assert!(!m.undo());
        assert!(m.redo() && m.redo());
        assert_eq!(m.left.lines, v("Q b c"));
        assert!(!m.redo());
        // a new edit clears the redo stack
        m.undo();
        m.set_line(2, Pane::Right, "Z".into());
        assert!(!m.redo());
        // undo depth is capped
        let mut m = merge("a", "a");
        for i in 0..(MAX_UNDO + 20) {
            m.set_line(0, Pane::Left, format!("l{i}"));
        }
        let mut n = 0;
        while m.undo() {
            n += 1;
        }
        assert_eq!(n, MAX_UNDO);
    }

    #[test]
    fn converges_randomly() {
        let mut s = 99u64;
        let mut rnd = |m: usize| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 33) as usize) % m
        };
        let alpha = ["a", "b", "c", "d", "e"];
        for _ in 0..1500 {
            let a: Vec<String> = (0..rnd(12)).map(|_| alpha[rnd(5)].to_string()).collect();
            let b: Vec<String> = (0..rnd(12)).map(|_| alpha[rnd(5)].to_string()).collect();
            let tb = |l: &Vec<String>| TextBuf { lines: l.clone(), crlf: false, final_nl: true, exists: true };

            // copying every hunk (always the first one left) from left makes right == left
            let mut m = Merge::new(tb(&a), tb(&b));
            let mut guard = 0;
            while !m.align.hunks.is_empty() {
                assert!(m.put_hunk(0, Pane::Left));
                guard += 1;
                assert!(guard <= a.len() + b.len() + 2, "no progress {a:?} {b:?}");
            }
            assert_eq!(m.right.lines, a);
            assert_eq!(m.left.lines, a);

            // same from the right
            let mut m = Merge::new(tb(&a), tb(&b));
            while !m.align.hunks.is_empty() {
                assert!(m.put_hunk(m.align.hunks.len() - 1, Pane::Right));
            }
            assert_eq!(m.left.lines, b);

            // line-by-line: repeatedly applying the first differing row converges too
            let mut m = Merge::new(tb(&a), tb(&b));
            let mut guard = 0;
            while let Some(r) = m.align.rows.iter().position(|r| r.kind != RowKind::Equal) {
                let from = if rnd(2) == 0 { Pane::Left } else { Pane::Right };
                if !m.put_line(r, from) {
                    assert!(m.put_line(r, from.other()), "stuck {a:?} {b:?}");
                }
                guard += 1;
                assert!(guard <= 2 * (a.len() + b.len()) + 4, "no progress {a:?} {b:?}");
            }
            assert_eq!(m.left.lines, m.right.lines);

            // rows cover each line exactly once, in order, hunks line up with rows
            let m = Merge::new(tb(&a), tb(&b));
            let ls: Vec<usize> = m.align.rows.iter().filter_map(|r| r.left).collect();
            let rs: Vec<usize> = m.align.rows.iter().filter_map(|r| r.right).collect();
            assert_eq!(ls, (0..a.len()).collect::<Vec<_>>());
            assert_eq!(rs, (0..b.len()).collect::<Vec<_>>());
            for h in &m.align.hunks {
                assert!(m.align.rows[h.rows.clone()].iter().all(|r| r.kind != RowKind::Equal));
            }

            // undo everything restores the originals
            let mut m = Merge::new(tb(&a), tb(&b));
            while !m.align.hunks.is_empty() {
                m.put_hunk(0, Pane::Left);
            }
            while m.undo() {}
            assert_eq!((m.left.lines.clone(), m.right.lines.clone()), (a.clone(), b.clone()));
        }
    }

    #[test]
    fn copies_are_byte_identical() {
        let root = tmp();
        let (lp, rp) = (root.join("l"), root.join("r"));
        // source without trailing newline, CRLF; target missing
        fs::write(&lp, "a\r\nb").unwrap();
        let mut m = Merge::new(load_text(Some(&lp)).unwrap(), load_text(None).unwrap());
        m.put_hunk(0, Pane::Left);
        m.save(&lp, &rp).unwrap();
        assert_eq!(fs::read(&rp).unwrap(), b"a\r\nb");
        // target exists but is empty: it adopts the source style when lines are copied in
        fs::write(&rp, "").unwrap();
        fs::write(&lp, "x\ny").unwrap();
        let mut m = Merge::new(load_text(Some(&lp)).unwrap(), load_text(Some(&rp)).unwrap());
        m.put_line(0, Pane::Left);
        m.put_line(1, Pane::Left);
        m.save(&lp, &rp).unwrap();
        assert_eq!(fs::read(&rp).unwrap(), b"x\ny");
        // typing into an empty file gives it a trailing newline
        fs::write(&rp, "").unwrap();
        let mut m = Merge::new(load_text(Some(&rp)).unwrap(), load_text(Some(&rp)).unwrap());
        m.insert_after(0, Pane::Left, "hi".into());
        assert!(m.left.final_nl);
        m.save(&rp, &rp).unwrap();
        assert_eq!(fs::read(&rp).unwrap(), b"hi\n");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn saving() {
        let root = tmp();
        let (lp, rp) = (root.join("l.txt"), root.join("sub/r.txt"));
        fs::write(&lp, "one\r\ntwo\r\n").unwrap();
        let mut m = Merge::new(load_text(Some(&lp)).unwrap(), load_text(None).unwrap());
        assert!(!m.right.exists);
        assert!(m.right.crlf, "a new file adopts the other side's line endings");
        assert_eq!(m.save(&lp, &rp).unwrap(), Vec::<Pane>::new()); // nothing dirty
        assert!(!rp.exists());
        m.put_hunk(0, Pane::Left);
        m.set_line(0, Pane::Left, "ONE".into());
        assert_eq!(m.save(&lp, &rp).unwrap(), vec![Pane::Left, Pane::Right]);
        assert_eq!(fs::read(&lp).unwrap(), b"ONE\r\ntwo\r\n");
        assert_eq!(fs::read(&rp).unwrap(), b"one\r\ntwo\r\n");
        assert!(!m.is_dirty(Pane::Left) && !m.is_dirty(Pane::Right));
        assert!(m.right.exists);
        // saving twice is a no-op, a later edit is dirty again
        assert!(m.save(&lp, &rp).unwrap().is_empty());
        m.set_line(1, Pane::Right, "TWO".into());
        assert!(m.is_dirty(Pane::Right));
        assert_eq!(m.save(&lp, &rp).unwrap(), vec![Pane::Right]);
        // undo back to the saved state is not dirty; past it is
        m.undo();
        assert!(m.is_dirty(Pane::Right));

        // a file without a trailing newline keeps that
        fs::write(&lp, "x\ny").unwrap();
        let mut m = Merge::new(load_text(Some(&lp)).unwrap(), load_text(Some(&rp)).unwrap());
        m.set_line(1, Pane::Left, "Y".into());
        m.save(&lp, &rp).unwrap();
        assert_eq!(fs::read(&lp).unwrap(), b"x\nY");

        // unreadable kinds
        fs::write(root.join("bin"), [0u8, 1]).unwrap();
        assert!(matches!(load_text(Some(&root.join("bin"))), Err(FileDiff::Binary)));
        fs::write(root.join("latin"), [0x68u8, 0xe9, 0x0a]).unwrap();
        assert!(matches!(load_text(Some(&root.join("latin"))), Err(FileDiff::Binary)));
        assert!(matches!(load_text(Some(&root)), Err(FileDiff::TypeMismatch)));
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod pair_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir()
            .join(format!("dirrcmp-pair-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    fn pair(l: &str, r: &str) -> (PathBuf, PathBuf) {
        (PathBuf::from(l), PathBuf::from(r))
    }

    fn find<'a>(n: &'a Node, name: &str) -> &'a Node {
        n.children.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no {name}"))
    }

    #[test]
    fn renamed_directory_is_compared_as_one_entry() {
        let root = tmp();
        let (l, r) = (root.join("l"), root.join("r"));
        write(&l.join("old/a"), "same");
        write(&r.join("new/a"), "same");
        write(&l.join("old/b"), "left");
        write(&r.join("new/b"), "right!");
        write(&l.join("old/c"), "only left");
        write(&r.join("new/d"), "only right");
        write(&l.join("x"), "x");
        write(&r.join("x"), "x");

        let plain = compare(&l, &r, &Opts::default());
        assert_eq!(find(&plain, "old").status, Status::LeftOnly);
        assert_eq!(find(&plain, "new").status, Status::RightOnly);
        assert!(!find(&plain, "old").is_paired());

        let opts = Opts { pairs: vec![pair("old", "new")], ..Opts::default() };
        let t = compare(&l, &r, &opts);
        assert_eq!(t.children.len(), 2, "old and new merge into one entry");
        let n = find(&t, "old \u{21c4} new");
        assert!(n.is_paired() && n.is_dir());
        assert_eq!((n.rel.as_path(), n.rrel.as_path()), (Path::new("old"), Path::new("new")));
        assert_eq!(n.status, Status::Modified);
        let (a, b, c, d) = (find(n, "a"), find(n, "b"), find(n, "c"), find(n, "d"));
        assert_eq!(a.status, Status::Same);
        assert_eq!(b.status, Status::Modified);
        assert_eq!(c.status, Status::LeftOnly);
        assert_eq!(d.status, Status::RightOnly);
        assert_eq!((b.rel.as_path(), b.rrel.as_path()), (Path::new("old/b"), Path::new("new/b")));
        assert_eq!(d.rrel, Path::new("new/d"));
        let st = stats(&t);
        assert_eq!((st.modified, st.left_only, st.right_only, st.identical), (1, 1, 1, 2));

        // identical contents under different names -> the pair is Same
        write(&l.join("p/f"), "1");
        write(&r.join("q/f"), "1");
        let opts = Opts { pairs: vec![pair("p", "q")], ..Opts::default() };
        let t = compare(&l, &r, &opts);
        assert_eq!(find(&t, "p \u{21c4} q").status, Status::Same);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_pairs_and_ignore() {
        let root = tmp();
        let (l, r) = (root.join("l"), root.join("r"));
        write(&l.join("p/old/f"), "1");
        write(&r.join("q/new/f"), "2");
        write(&l.join("p/skip.log"), "ignored");
        let mut ig = Ignore::new();
        ig.add("*.log");
        let opts = Opts {
            ignore: ig,
            pairs: vec![pair("p", "q"), pair("p/old", "q/new")],
            ..Opts::default()
        };
        let t = compare(&l, &r, &opts);
        let outer = find(&t, "p \u{21c4} q");
        assert_eq!(outer.children.len(), 1, "ignored file is skipped");
        let inner = find(outer, "old \u{21c4} new");
        assert_eq!(inner.rrel, Path::new("q/new"));
        let f = find(inner, "f");
        assert_eq!((f.rel.as_path(), f.rrel.as_path()), (Path::new("p/old/f"), Path::new("q/new/f")));
        assert_eq!(f.status, Status::Modified);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_pairs_are_ignored() {
        let root = tmp();
        let (l, r) = (root.join("l"), root.join("r"));
        write(&l.join("old/f"), "1");
        write(&r.join("new/f"), "1");
        write(&l.join("both/f"), "1");
        write(&r.join("both/f"), "1");
        write(&r.join("sub/deep/f"), "1");
        write(&l.join("file"), "x");
        write(&r.join("file2"), "x");
        for bad in [
            pair("both", "new"),         // left name also exists on the right
            pair("old", "both"),         // right name also exists on the left
            pair("old", "sub/deep"),     // different parents
            pair("old", "file2"),        // a directory cannot pair with a file
            pair("file", "new"),         // nor a file with a directory
            pair("old", "missing"),      // does not exist
            pair("", "new"),             // nonsense
        ] {
            let opts = Opts { pairs: vec![bad.clone()], ..Opts::default() };
            let t = compare(&l, &r, &opts);
            assert!(t.children.iter().all(|c| !c.is_paired()), "{bad:?} must be ignored");
            assert_eq!(find(&t, "old").status, Status::LeftOnly, "{bad:?}");
        }
        // two pairs competing for the same directory: only the first one applies
        write(&r.join("new2/f"), "1");
        let opts = Opts { pairs: vec![pair("old", "new"), pair("old", "new2")], ..Opts::default() };
        let t = compare(&l, &r, &opts);
        assert_eq!(t.children.iter().filter(|c| c.is_paired()).count(), 1);
        assert_eq!(find(&t, "new2").status, Status::RightOnly);
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod file_pair_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    fn pair(a: &str, b: &str) -> (PathBuf, PathBuf) {
        (PathBuf::from(a), PathBuf::from(b))
    }

    #[test]
    fn renamed_files_can_be_paired() {
        let root = std::env::temp_dir()
            .join(format!("dirrcmp-filepair-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&root);
        let (l, r) = (root.join("l"), root.join("r"));
        write(&l.join("notes.txt"), "same\n");
        write(&r.join("notes-v2.txt"), "same\n");
        write(&l.join("a.txt"), "left");
        write(&r.join("b.txt"), "right!");
        write(&l.join("f"), "a file");
        write(&r.join("d2/x"), "inside a directory");

        let plain = compare(&l, &r, &Opts::default());
        assert!(plain.children.iter().any(|c| c.name == "notes.txt" && c.status == Status::LeftOnly));
        assert!(plain.children.iter().any(|c| c.name == "notes-v2.txt" && c.status == Status::RightOnly));

        let opts = Opts {
            pairs: vec![pair("notes.txt", "notes-v2.txt"), pair("a.txt", "b.txt"), pair("f", "d2")],
            ..Opts::default()
        };
        let t = compare(&l, &r, &opts);
        let get = |name: &str| t.children.iter().find(|c| c.name == name);

        let n = get("notes.txt \u{21c4} notes-v2.txt").expect("identical files under different names");
        assert!(n.is_paired() && !n.is_dir());
        assert_eq!(n.status, Status::Same);
        assert_eq!((n.rel.as_path(), n.rrel.as_path()), (Path::new("notes.txt"), Path::new("notes-v2.txt")));

        let m = get("a.txt \u{21c4} b.txt").expect("differing files under different names");
        assert!(m.is_paired() && m.status == Status::Modified);

        // a file cannot be paired with a directory: that pair is ignored
        assert_eq!(get("f").map(|c| c.status), Some(Status::LeftOnly));
        assert_eq!(get("d2").map(|c| c.status), Some(Status::RightOnly));
        assert!(get("f \u{21c4} d2").is_none());

        let st = stats(&t);
        assert_eq!((st.identical, st.modified, st.left_only, st.right_only), (1, 1, 1, 1));
        let _ = fs::remove_dir_all(&root);
    }
}
