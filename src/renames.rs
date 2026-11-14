//! Rename / move detection, like `git diff -M`.
//!
//! After the trees are compared, files that exist only on the left are matched
//! with files that exist only on the right:
//!
//! 1. identical content (exact rename or move), then
//! 2. similar text content (renamed or moved *and* edited), default threshold 50 %.
//!
//! A matched pair is merged into one node placed where the file lives on the
//! right side, with `rel` pointing at the left path and `rrel` at the right one
//! (the same representation as a manually paired entry). The node is marked
//! `Modified` and carries the similarity in `Node::rename` (100 = identical).

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs::{self, File};
use std::hash::Hasher;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::engine::{self, Node, Status};

/// Minimum similarity (percent) for an edited file to count as a rename.
pub const THRESHOLD: u8 = 50;
/// Largest file looked at for similarity.
const MAX_SIMILAR_SIZE: u64 = 512 * 1024;
/// Upper bound on scored pairs, so huge one-sided trees stay fast.
const MAX_PAIRS: usize = 200_000;
/// Identical-content groups bigger than this only pair equal base names.
const MAX_GROUP_PAIRS: usize = 10_000;

struct Cand {
    /// Absolute path of the file.
    path: PathBuf,
    /// `rel` for left candidates, `rrel` for right candidates (the node key).
    key: PathBuf,
    size: u64,
}

fn base(p: &Path) -> Option<&std::ffi::OsStr> {
    p.file_name()
}

/// Higher is a better tie-break: same base name, then same directory.
fn affinity(l: &Path, r: &Path) -> u8 {
    (base(l) == base(r)) as u8 * 2 + (l.parent() == r.parent()) as u8
}

fn collect(
    nodes: &[Node],
    status: Status,
    root: &Path,
    left_side: bool,
    out: &mut Vec<Cand>,
) {
    for n in nodes {
        if n.is_dir() {
            collect(&n.children, status, root, left_side, out);
            continue;
        }
        if n.status != status || n.rename.is_some() {
            continue;
        }
        let (meta, key) = if left_side { (&n.left, &n.rel) } else { (&n.right, &n.rrel) };
        let Some(m) = meta else { continue };
        if m.is_dir || m.is_symlink {
            continue;
        }
        out.push(Cand { path: root.join(key), key: key.clone(), size: m.size });
    }
}

fn hash_file(p: &Path) -> Option<u64> {
    let mut f = File::open(p).ok()?;
    let mut h = DefaultHasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            return Some(h.finish());
        }
        h.write(&buf[..n]);
    }
}

/// Line fingerprint of a text file: (hash of line -> total bytes of those lines, total bytes).
fn lines_of(p: &Path) -> Option<(HashMap<u64, u64>, u64)> {
    let data = fs::read(p).ok()?;
    if data.iter().take(8192).any(|&b| b == 0) {
        return None; // binary
    }
    let mut map: HashMap<u64, u64> = HashMap::new();
    let mut total = 0u64;
    let body = data.strip_suffix(b"\n").unwrap_or(&data);
    for line in body.split(|&b| b == b'\n') {
        let mut h = DefaultHasher::new();
        h.write(line);
        let w = line.len() as u64 + 1;
        *map.entry(h.finish()).or_insert(0) += w;
        total += w;
    }
    Some((map, total))
}

fn similarity(a: &(HashMap<u64, u64>, u64), b: &(HashMap<u64, u64>, u64)) -> u8 {
    let (small, big) = if a.0.len() <= b.0.len() { (&a.0, &b.0) } else { (&b.0, &a.0) };
    let common: u64 = small.iter().map(|(h, w)| (*w).min(big.get(h).copied().unwrap_or(0))).sum();
    let denom = a.1 + b.1;
    if denom == 0 {
        return 0;
    }
    ((2 * common * 100) / denom).min(100) as u8
}

/// Find the pairs `(left key, right key, similarity)`.
pub fn find_pairs(tree: &Node, left: &Path, right: &Path) -> Vec<(PathBuf, PathBuf, u8)> {
    let mut ls = Vec::new();
    let mut rs = Vec::new();
    collect(&tree.children, Status::LeftOnly, left, true, &mut ls);
    collect(&tree.children, Status::RightOnly, right, false, &mut rs);
    if ls.is_empty() || rs.is_empty() {
        return Vec::new();
    }
    let mut lused = vec![false; ls.len()];
    let mut rused = vec![false; rs.len()];
    let mut out: Vec<(PathBuf, PathBuf, u8)> = Vec::new();

    // 1. identical content (empty files carry no information: skipped, like git)
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (j, r) in rs.iter().enumerate() {
        if r.size > 0 {
            by_size.entry(r.size).or_default().push(j);
        }
    }
    let mut lhash: HashMap<usize, Option<u64>> = HashMap::new();
    let mut rhash: HashMap<usize, Option<u64>> = HashMap::new();
    let mut exact: Vec<(u8, usize, usize)> = Vec::new();
    // group sizes, to bound the work for piles of identical files
    let mut lcount: HashMap<u64, usize> = HashMap::new();
    for l in &ls {
        *lcount.entry(l.size).or_insert(0) += 1;
    }
    for (i, l) in ls.iter().enumerate() {
        let Some(cands) = by_size.get(&l.size) else { continue };
        let big = lcount[&l.size] * cands.len() > MAX_GROUP_PAIRS;
        let lh = *lhash.entry(i).or_insert_with(|| hash_file(&l.path));
        let Some(lh) = lh else { continue };
        for &j in cands {
            if big && base(&l.key) != base(&rs[j].key) {
                continue;
            }
            let rh = *rhash.entry(j).or_insert_with(|| hash_file(&rs[j].path));
            if rh != Some(lh) {
                continue;
            }
            // a hash match is verified byte for byte
            if engine::files_equal(&l.path, &rs[j].path).unwrap_or(false) {
                exact.push((affinity(&l.key, &rs[j].key), i, j));
            }
        }
    }
    exact.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    for (_, i, j) in exact {
        if !lused[i] && !rused[j] {
            lused[i] = true;
            rused[j] = true;
            out.push((ls[i].key.clone(), rs[j].key.clone(), 100));
        }
    }

    // 2. similar text
    let li: Vec<usize> = (0..ls.len()).filter(|&i| !lused[i] && ls[i].size > 0 && ls[i].size <= MAX_SIMILAR_SIZE).collect();
    let ri: Vec<usize> = (0..rs.len()).filter(|&j| !rused[j] && rs[j].size > 0 && rs[j].size <= MAX_SIMILAR_SIZE).collect();
    let near = |a: u64, b: u64| a.min(b) * 3 >= a.max(b);
    let pairs = li.iter().map(|&i| ri.iter().filter(|&&j| near(ls[i].size, rs[j].size)).count()).sum::<usize>();
    if pairs > 0 && pairs <= MAX_PAIRS {
        let lf: HashMap<usize, Option<(HashMap<u64, u64>, u64)>> =
            li.iter().map(|&i| (i, lines_of(&ls[i].path))).collect();
        let rf: HashMap<usize, Option<(HashMap<u64, u64>, u64)>> =
            ri.iter().map(|&j| (j, lines_of(&rs[j].path))).collect();
        let mut scored: Vec<(u8, u8, usize, usize)> = Vec::new();
        for &i in &li {
            let Some(a) = &lf[&i] else { continue };
            for &j in &ri {
                if !near(ls[i].size, rs[j].size) {
                    continue;
                }
                let Some(b) = &rf[&j] else { continue };
                let s = similarity(a, b);
                if s >= THRESHOLD {
                    scored.push((s, affinity(&ls[i].key, &rs[j].key), i, j));
                }
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
        for (s, _, i, j) in scored {
            if !lused[i] && !rused[j] {
                lused[i] = true;
                rused[j] = true;
                out.push((ls[i].key.clone(), rs[j].key.clone(), s));
            }
        }
    }
    out
}

/// Remove and return the left-only node whose `rel` is `key`.
fn take_left(nodes: &mut Vec<Node>, key: &Path) -> Option<Node> {
    if let Some(i) = nodes.iter().position(|n| n.status == Status::LeftOnly && n.rel == key) {
        return Some(nodes.remove(i));
    }
    for n in nodes.iter_mut() {
        if n.is_dir() && key.starts_with(&n.rel) {
            if let Some(f) = take_left(&mut n.children, key) {
                return Some(f);
            }
        }
    }
    None
}

/// Replace the right-only node whose `rrel` is `key` with a merged node.
fn merge_right(nodes: &mut [Node], key: &Path, l: Node, pct: u8) -> Result<(), Node> {
    let mut l = l;
    for n in nodes.iter_mut() {
        if n.status == Status::RightOnly && !n.is_dir() && n.rrel == key {
            *n = Node {
                name: n.name.clone(),
                rel: l.rel.clone(),
                rrel: n.rrel.clone(),
                left: l.left.take(),
                right: n.right.take(),
                status: Status::Modified,
                children: Vec::new(),
                rename: Some(pct),
            };
            return Ok(());
        }
        if n.is_dir() && key.starts_with(&n.rrel) {
            match merge_right(&mut n.children, key, l, pct) {
                Ok(()) => return Ok(()),
                Err(back) => l = back,
            }
        }
    }
    Err(l)
}

/// Left-only directories that have something inside.
fn filled_left_dirs(nodes: &[Node], out: &mut std::collections::HashSet<PathBuf>) {
    for n in nodes {
        if n.is_dir() && n.status == Status::LeftOnly && !n.children.is_empty() {
            out.insert(n.rel.clone());
        }
        filled_left_dirs(&n.children, out);
    }
}

/// Drop left-only directories whose contents were all moved away (bottom-up).
fn prune(nodes: &mut Vec<Node>, filled: &std::collections::HashSet<PathBuf>) {
    for n in nodes.iter_mut() {
        prune(&mut n.children, filled);
    }
    nodes.retain(|n| !(n.is_dir() && n.status == Status::LeftOnly && n.children.is_empty() && filled.contains(&n.rel)));
}

/// Detect renames below `tree` (compared from `left` and `right`) and merge each pair.
pub fn detect(tree: &mut Node, left: &Path, right: &Path) {
    let pairs = find_pairs(tree, left, right);
    if pairs.is_empty() {
        return;
    }
    let mut filled = std::collections::HashSet::new();
    filled_left_dirs(&tree.children, &mut filled);
    for (lk, rk, pct) in pairs {
        let Some(l) = take_left(&mut tree.children, &lk) else { continue };
        if let Err(l) = merge_right(&mut tree.children, &rk, l, pct) {
            // could not place it: put the left entry back where it came from
            tree.children.push(l);
        }
    }
    prune(&mut tree.children, &filled);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{compare, stats, Opts};

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir()
            .join(format!("dirrcmp-ren-{}-{}", std::process::id(), crate::utmp::next()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(p.join("l")).unwrap();
        fs::create_dir_all(p.join("r")).unwrap();
        p
    }

    fn put(p: PathBuf, text: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn on() -> Opts {
        Opts { detect_renames: true, ..Opts::default() }
    }

    fn all<'a>(n: &'a Node, out: &mut Vec<&'a Node>) {
        for c in &n.children {
            out.push(c);
            all(c, out);
        }
    }

    fn body(n: usize) -> String {
        (0..n).map(|i| format!("line number {i} of the file\n")).collect()
    }

    #[test]
    fn moved_file_with_identical_content() {
        let d = tmp();
        put(d.join("l/a/old.txt"), &body(10));
        put(d.join("r/b/new.txt"), &body(10));
        put(d.join("l/keep.txt"), "k");
        put(d.join("r/keep.txt"), "k");
        let t = compare(&d.join("l"), &d.join("r"), &on());
        let mut v = Vec::new();
        all(&t, &mut v);
        let r = v.iter().find(|n| n.rename.is_some()).expect("rename found");
        assert_eq!(r.rename, Some(100));
        assert_eq!(r.name, "new.txt");
        assert_eq!(r.rel, PathBuf::from("a/old.txt"));
        assert_eq!(r.rrel, PathBuf::from("b/new.txt"));
        assert!(r.left.is_some() && r.right.is_some());
        assert_eq!(r.status, Status::Modified);
        // nothing is left over as one-sided files
        assert!(!v.iter().any(|n| !n.is_dir() && matches!(n.status, Status::LeftOnly | Status::RightOnly)));
        let s = stats(&t);
        assert_eq!((s.renamed, s.left_only, s.right_only, s.modified, s.identical), (1, 0, 0, 0, 1));
        assert!(!t.children.iter().any(|n| n.name == "a"), "emptied left-only dir is dropped");
        // the merged node sits under the right-hand directory
        let b = t.children.iter().find(|n| n.name == "b").unwrap();
        assert!(b.children.iter().any(|n| n.rename.is_some()));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn same_directory_rename_and_edit() {
        let d = tmp();
        let old = body(10);
        let new = old.replace("line number 3 of the file", "something else entirely");
        put(d.join("l/x/old.rs"), &old);
        put(d.join("r/x/new.rs"), &new);
        let t = compare(&d.join("l"), &d.join("r"), &on());
        let mut v = Vec::new();
        all(&t, &mut v);
        let r = v.iter().find(|n| n.rename.is_some()).expect("similar file found");
        let p = r.rename.unwrap();
        assert!((50..100).contains(&p), "{p}");
        assert_eq!((r.rel.clone(), r.rrel.clone()), (PathBuf::from("x/old.rs"), PathBuf::from("x/new.rs")));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn unrelated_files_stay_one_sided_and_detection_is_opt_in() {
        let d = tmp();
        put(d.join("l/a.txt"), &body(10));
        put(d.join("r/b.txt"), "completely different\ncontent here\n");
        let t = compare(&d.join("l"), &d.join("r"), &on());
        let s = stats(&t);
        assert_eq!((s.renamed, s.left_only, s.right_only), (0, 1, 1));
        put(d.join("r/c.txt"), &body(10));
        let t = compare(&d.join("l"), &d.join("r"), &Opts::default());
        assert_eq!(stats(&t).renamed, 0);
        let t = compare(&d.join("l"), &d.join("r"), &on());
        assert_eq!((stats(&t).renamed, stats(&t).right_only), (1, 1));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn each_file_is_used_once_and_names_break_ties() {
        let d = tmp();
        let c = body(6);
        put(d.join("l/x/a.txt"), &c);
        put(d.join("l/y/b.txt"), &c);
        put(d.join("r/z/b.txt"), &c);
        let t = compare(&d.join("l"), &d.join("r"), &on());
        let s = stats(&t);
        assert_eq!((s.renamed, s.left_only, s.right_only), (1, 1, 0));
        let mut v = Vec::new();
        all(&t, &mut v);
        let r = v.iter().find(|n| n.rename.is_some()).unwrap();
        assert_eq!(r.rel, PathBuf::from("y/b.txt")); // same base name wins
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn empty_and_binary_files() {
        let d = tmp();
        put(d.join("l/e1"), "");
        put(d.join("r/e2"), "");
        put(d.join("l/bin1"), "\0\u{1}\u{2}abc");
        put(d.join("r/bin2"), "\0\u{1}\u{2}abd");
        let t = compare(&d.join("l"), &d.join("r"), &on());
        // empty files are never paired; binaries only when byte-identical
        assert_eq!(stats(&t).renamed, 0);
        put(d.join("r/bin3"), "\0\u{1}\u{2}abc");
        let t = compare(&d.join("l"), &d.join("r"), &on());
        assert_eq!(stats(&t).renamed, 1);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn works_inside_one_sided_directories() {
        let d = tmp();
        put(d.join("l/olddir/f.txt"), &body(8));
        put(d.join("l/olddir/g.txt"), "only left\n");
        put(d.join("r/newdir/f.txt"), &body(8));
        let t = compare(&d.join("l"), &d.join("r"), &on());
        let s = stats(&t);
        assert_eq!((s.renamed, s.left_only, s.right_only), (1, 1, 0));
        let nd = t.children.iter().find(|n| n.name == "newdir").unwrap();
        assert_eq!(nd.children.len(), 1);
        assert_eq!(nd.children[0].rel, PathBuf::from("olddir/f.txt"));
        let od = t.children.iter().find(|n| n.name == "olddir").unwrap();
        assert_eq!(od.children.len(), 1); // g.txt stays
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn similarity_scoring() {
        let a = lines_of_text("a\nb\nc\nd\n");
        let b = lines_of_text("a\nb\nc\nd\n");
        assert_eq!(similarity(&a, &b), 100);
        let c = lines_of_text("a\nb\nx\ny\n");
        assert_eq!(similarity(&a, &c), 50);
        let z = lines_of_text("p\nq\nr\ns\n");
        assert_eq!(similarity(&a, &z), 0);
    }

    fn lines_of_text(t: &str) -> (HashMap<u64, u64>, u64) {
        let p = std::env::temp_dir().join(format!("dirrcmp-ren-l-{}-{}", std::process::id(), crate::utmp::next()));
        fs::write(&p, t).unwrap();
        let r = lines_of(&p).unwrap();
        let _ = fs::remove_file(&p);
        r
    }
}
