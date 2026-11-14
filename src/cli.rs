//! Argument parsing and the non-interactive output formats (std only).

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::engine::{self, FileDiff, Ignore, LineKind, Node, Status};

pub const USAGE: &str = "\
dirrcmp - compare two directories in the terminal

USAGE:
    dirrcmp [OPTIONS] <LEFT> <RIGHT>          interactive TUI (two directories)
    dirrcmp [OPTIONS] <FILE1> <FILE2>         side-by-side editor (two files; unified diff if piped)
    dirrcmp diff  [OPTIONS] <LEFT> <RIGHT>    print differences
    dirrcmp check [OPTIONS] <LEFT> <RIGHT>    silent; exit 0 if identical, 1 if different

OPTIONS:
        --no-ignore           disable all ignore patterns
        --git [REV [REV2]]    compare a git revision (default HEAD) with the working tree,
                              or two revisions; replaces LEFT RIGHT
        --staged              with --git: compare against the index instead of the working tree
        --repo <DIR>          with --git: repository to use (default: current directory)
        --no-syntax           no syntax highlighting in the diff views
        --no-renames          don't pair moved/renamed files (same or similar content)
        --ignore <PATTERN>    add a gitignore-style pattern (repeatable)
        --follow-symlinks     follow symlinks instead of comparing link targets
        --dates               also treat differing modification times as a difference
        --quick               do not read files: same size and mtime means identical
        --diff-cmd <CMD>      external diff for the TUI, e.g. \"nvim -d\" (or set DIRCMP_DIFF)
    -h, --help                show this help
    -v, --version             show version

OPTIONS (diff / check):
        --format <FMT>        tree (default), flat or json          [diff]
        --filter <KIND>       modified, left-only or right-only (repeatable, or comma list)
        --stat                print a summary line (to stdout; before the result for check)

Ignored by default: .git, node_modules, .DS_Store.
Extra global patterns are read from $XDG_DATA_HOME/dirrcmp/ignore
(default ~/.local/share/dirrcmp/ignore). Patterns added in the TUI (i, I) are
saved per pair of directories in the pairs/ folder next to it and also apply
to 'diff' and 'check'.
Preferences live in $XDG_CONFIG_HOME/dirrcmp/config.json (default
~/.config/dirrcmp/config.json): compareDates, compareContents, dirsFirst,
showHints, nerdFont, syntaxHighlight, chunkBackground, smoothScroll, detectRenames, diffCommand.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Tree,
    Flat,
    Json,
}

#[derive(Debug)]
pub enum Command {
    Tui,
    Diff { format: Format, filter: Vec<Status>, stat: bool },
    Check { stat: bool },
    Help,
    Version,
}

#[derive(Debug)]
pub struct Cli {
    pub cmd: Command,
    pub left: Option<PathBuf>,
    pub right: Option<PathBuf>,
    pub no_ignore: bool,
    pub no_syntax: bool,
    pub no_renames: bool,
    pub ignore: Vec<String>,
    pub follow_symlinks: bool,
    pub compare_dates: bool,
    pub quick: bool,
    pub diff_cmd: Option<String>,
    /// `--git`: compare a revision with the working tree instead of two paths.
    pub git: Option<crate::git::GitOpts>,
}

fn take_value(
    name: &str,
    inline: Option<String>,
    it: &mut impl Iterator<Item = String>,
) -> Result<String, String> {
    match inline {
        Some(v) => Ok(v),
        None => it.next().ok_or_else(|| format!("option '{name}' needs a value")),
    }
}

fn parse_filter(v: &str, out: &mut Vec<Status>) -> Result<(), String> {
    for part in v.split(',') {
        out.push(match part.trim() {
            "modified" => Status::Modified,
            "left-only" => Status::LeftOnly,
            "right-only" => Status::RightOnly,
            other => return Err(format!("unknown filter '{other}' (modified, left-only, right-only)")),
        });
    }
    Ok(())
}

pub fn parse_args(args: Vec<String>) -> Result<Cli, String> {
    let mut sub: Option<String> = None;
    let mut pos: Vec<String> = Vec::new();
    let mut format = Format::Tree;
    let mut filter: Vec<Status> = Vec::new();
    let mut stat = false;
    let mut help = false;
    let mut version = false;
    let mut no_ignore = false;
    let mut no_syntax = false;
    let mut no_renames = false;
    let mut ignore = Vec::new();
    let mut follow_symlinks = false;
    let mut compare_dates = false;
    let mut quick = false;
    let mut diff_cmd = env::var("DIRCMP_DIFF").ok().filter(|s| !s.trim().is_empty());
    let mut only_positional = false;
    let mut git = false;
    let mut staged = false;
    let mut repo: Option<String> = None;

    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if only_positional || !a.starts_with('-') || a == "-" {
            if !only_positional && pos.is_empty() && sub.is_none() && (a == "diff" || a == "check") {
                sub = Some(a);
            } else {
                pos.push(a);
            }
            continue;
        }
        let (key, inline) = match a.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        match key.as_str() {
            "--" => only_positional = true,
            "-h" | "--help" => help = true,
            "-v" | "--version" => version = true,
            "--no-ignore" => no_ignore = true,
            "--no-syntax" => no_syntax = true,
            "--no-renames" => no_renames = true,
            "--git" => git = true,
            "--staged" | "--cached" => staged = true,
            "--repo" => repo = Some(take_value("--repo", inline, &mut it)?),
            "--follow-symlinks" => follow_symlinks = true,
            "--dates" => compare_dates = true,
            "--quick" => quick = true,
            "--stat" => stat = true,
            "--ignore" => ignore.push(take_value("--ignore", inline, &mut it)?),
            "--diff-cmd" => diff_cmd = Some(take_value("--diff-cmd", inline, &mut it)?),
            "--format" => {
                format = match take_value("--format", inline, &mut it)?.as_str() {
                    "tree" => Format::Tree,
                    "flat" => Format::Flat,
                    "json" => Format::Json,
                    other => return Err(format!("unknown format '{other}' (tree, flat, json)")),
                }
            }
            "--filter" => {
                let v = take_value("--filter", inline, &mut it)?;
                parse_filter(&v, &mut filter)?;
            }
            _ => return Err(format!("unknown option '{a}'")),
        }
    }

    let cmd = if help {
        Command::Help
    } else if version {
        Command::Version
    } else {
        match sub.as_deref() {
            Some("diff") => Command::Diff { format, filter, stat },
            Some("check") => Command::Check { stat },
            _ => Command::Tui,
        }
    };

    let git = if git && matches!(cmd, Command::Tui | Command::Diff { .. } | Command::Check { .. }) {
        if pos.len() > 2 {
            return Err(format!("--git takes at most two revisions, got {}", pos.len()));
        }
        Some(crate::git::GitOpts {
            revs: pos.clone(),
            staged,
            repo: PathBuf::from(repo.clone().unwrap_or_else(|| ".".to_string())),
        })
    } else {
        if !matches!(cmd, Command::Help | Command::Version) {
            if staged {
                return Err("--staged only works with --git".to_string());
            }
            if repo.is_some() {
                return Err("--repo only works with --git".to_string());
            }
        }
        None
    };

    let (left, right) = match cmd {
        Command::Help | Command::Version => (None, None),
        _ if git.is_some() => (None, None),
        _ => {
            if pos.len() != 2 {
                return Err(format!("expected exactly two paths (LEFT RIGHT), got {}", pos.len()));
            }
            (Some(PathBuf::from(&pos[0])), Some(PathBuf::from(&pos[1])))
        }
    };

    Ok(Cli { cmd, left, right, no_ignore, no_syntax, no_renames, ignore, follow_symlinks, compare_dates, quick, diff_cmd, git })
}

// ───────────────────────────── ignore loading ─────────────────────────────

pub fn data_dir() -> Option<PathBuf> {
    if let Some(x) = env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(x).join("dirrcmp"));
    }
    if let Some(h) = env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(h).join(".local/share/dirrcmp"));
    }
    env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()).map(|a| PathBuf::from(a).join("dirrcmp"))
}

/// Stable key for a pair of directories; the same for either panel order.
pub fn pair_key(left: &Path, right: &Path) -> String {
    let canon = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().into_owned();
    let (mut a, mut b) = (canon(left), canon(right));
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    // FNV-1a, 64 bit
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in a.bytes().chain(std::iter::once(0u8)).chain(b.bytes()) {
        h ^= byte as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Per directory pair ignore file: `<data dir>/pairs/<key>.ignore`.
pub fn pair_ignore_path(left: &Path, right: &Path) -> Option<PathBuf> {
    data_dir().map(|d| d.join("pairs").join(format!("{}.ignore", pair_key(left, right))))
}

/// Pattern lines of an ignore file (blank lines and `#` comments skipped).
pub fn read_pair_ignore(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .map(|t| {
            t.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect()
        })
        .unwrap_or_default()
}

/// Write the pattern lines (with a comment saying which pair they belong to);
/// an empty list removes the file.
pub fn write_pair_ignore(path: &Path, lines: &[String], header: &str) -> io::Result<()> {
    if lines.is_empty() {
        return match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut text = format!("# {}\n", header.replace('\n', " "));
    for l in lines {
        text.push_str(l);
        text.push('\n');
    }
    fs::write(path, text)
}

/// Defaults + global file + `--ignore` patterns, regardless of `--no-ignore`
/// (the TUI uses this to be able to toggle ignoring back on).
pub fn build_ignore_full(cli: &Cli) -> Ignore {
    let mut ig = Ignore::defaults();
    if let Some(d) = data_dir() {
        if let Ok(t) = fs::read_to_string(d.join("ignore")) {
            ig.add_all(&t);
        }
    }
    for p in &cli.ignore {
        ig.add(p);
    }
    ig
}

// ───────────────────────────── renderers ─────────────────────────────

/// `a/old.txt -> b/new.txt (87%)` style text for a detected rename; only the
/// names are shown when both are in the same directory.
fn rename_text(n: &Node, pct: u8) -> String {
    let (o, w) = (engine::rel_string(&n.rel), engine::rel_string(&n.rrel));
    let shown = if n.rel.parent() == n.rrel.parent() {
        let name = |p: &Path| p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        format!("{} -> {}", name(&n.rel), name(&n.rrel))
    } else {
        format!("{o} -> {w}")
    };
    if pct >= 100 { shown } else { format!("{shown} ({pct}%)") }
}

fn marker(s: Status) -> char {
    match s {
        Status::Modified => '~',
        Status::LeftOnly => '-',
        Status::RightOnly => '+',
        Status::Same => ' ',
    }
}

fn status_name(s: Status) -> &'static str {
    match s {
        Status::Modified => "modified",
        Status::LeftOnly => "left-only",
        Status::RightOnly => "right-only",
        Status::Same => "identical",
    }
}

fn wanted(s: Status, filter: &[Status]) -> bool {
    s != Status::Same && (filter.is_empty() || filter.contains(&s))
}

/// A directory present on both sides whose contents differ: we recurse into it
/// rather than reporting it as an entry itself.
fn is_container(n: &Node) -> bool {
    n.is_dir() && (n.status == Status::Modified || n.contains_rename())
}

/// Flat list of differing entries as (path, status, is_dir). One-sided
/// directories are reported as a single entry, not expanded.
/// `(path, status, is_dir, rename)` where `rename` is `(old path, similarity %)`
/// for a detected rename (then `path` is the new path).
pub type Entry = (String, Status, bool, Option<(String, u8)>);

pub fn collect_entries(root: &Node, filter: &[Status]) -> Vec<Entry> {
    fn rec(n: &Node, filter: &[Status], out: &mut Vec<Entry>) {
        for c in &n.children {
            if is_container(c) {
                rec(c, filter, out);
            } else if wanted(c.status, filter) {
                match c.rename {
                    Some(p) => out.push((
                        engine::rel_string(&c.rrel),
                        c.status,
                        false,
                        Some((engine::rel_string(&c.rel), p)),
                    )),
                    None => out.push((engine::rel_string(&c.rel), c.status, c.is_dir(), None)),
                }
            }
        }
    }
    let mut out = Vec::new();
    rec(root, filter, &mut out);
    out
}

fn has_match(n: &Node, filter: &[Status]) -> bool {
    n.children.iter().any(|c| {
        if is_container(c) {
            has_match(c, filter)
        } else {
            wanted(c.status, filter)
        }
    })
}

pub fn render_flat(root: &Node, filter: &[Status]) -> String {
    let mut s = String::new();
    for (p, st, is_dir, rn) in collect_entries(root, filter) {
        match rn {
            Some((old, pct)) => {
                let _ = writeln!(s, "R {old} -> {p}{}", if pct >= 100 { String::new() } else { format!(" ({pct}%)") });
            }
            None => {
                let _ = writeln!(s, "{} {}{}", marker(st), p, if is_dir { "/" } else { "" });
            }
        }
    }
    s
}

pub fn render_tree(root: &Node, filter: &[Status]) -> String {
    fn rec(n: &Node, depth: usize, filter: &[Status], out: &mut String) {
        let indent = "  ".repeat(depth);
        for c in &n.children {
            if is_container(c) {
                if has_match(c, filter) {
                    let _ = writeln!(out, "  {indent}{}/", c.name);
                    rec(c, depth + 1, filter, out);
                }
            } else if wanted(c.status, filter) {
                if let Some(pct) = c.rename {
                    let _ = writeln!(out, "R {indent}{}", rename_text(c, pct));
                    continue;
                }
                let _ = writeln!(
                    out,
                    "{} {indent}{}{}",
                    marker(c.status),
                    c.name,
                    if c.is_dir() { "/" } else { "" }
                );
            }
        }
    }
    let mut s = String::new();
    rec(root, 0, filter, &mut s);
    s
}

fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o
}

pub fn render_json(root: &Node, filter: &[Status]) -> String {
    let entries = collect_entries(root, filter);
    let mut s = String::from("[");
    for (i, (p, st, is_dir, rn)) in entries.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        match rn {
            Some((old, pct)) => {
                let _ = write!(
                    s,
                    "\n  {{\"path\":\"{}\",\"status\":\"renamed\",\"type\":\"file\",\"from\":\"{}\",\"similarity\":{}}}",
                    json_escape(p),
                    json_escape(old),
                    pct
                );
            }
            None => {
                let _ = write!(
                    s,
                    "\n  {{\"path\":\"{}\",\"status\":\"{}\",\"type\":\"{}\"}}",
                    json_escape(p),
                    status_name(*st),
                    if *is_dir { "dir" } else { "file" }
                );
            }
        }
    }
    if !entries.is_empty() {
        s.push('\n');
    }
    s.push_str("]\n");
    s
}

pub fn render_stat(root: &Node) -> String {
    let st = engine::stats(root);
    let renamed = if st.renamed > 0 { format!(", {} renamed", st.renamed) } else { String::new() };
    format!(
        "{} modified, {} left-only, {} right-only, {} identical{renamed}\n",
        st.modified, st.left_only, st.right_only, st.identical
    )
}

pub fn render_file_diff(fd: &FileDiff, left: &Path, right: &Path) -> String {
    match fd {
        FileDiff::Identical => "Files are identical\n".to_string(),
        FileDiff::Binary => "Binary files differ\n".to_string(),
        FileDiff::TooLarge(n) => format!("File too large to diff ({})\n", engine::human_size(*n)),
        FileDiff::TypeMismatch => "One side is a directory\n".to_string(),
        FileDiff::Error(e) => format!("error: {e}\n"),
        FileDiff::Text(d) => {
            let mut s = format!("--- {}\n+++ {}\n", left.display(), right.display());
            for l in &d.lines {
                match l.kind {
                    LineKind::Hunk => {
                        let _ = writeln!(s, "{}", l.text);
                    }
                    LineKind::Context => {
                        let _ = writeln!(s, " {}", l.text);
                    }
                    LineKind::Add => {
                        let _ = writeln!(s, "+{}", l.text);
                    }
                    LineKind::Del => {
                        let _ = writeln!(s, "-{}", l.text);
                    }
                }
            }
            s
        }
    }
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{compare, sort_tree, Opts, SortKey};

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_subcommands_and_flags() {
        let c = parse_args(args("diff --format=json --filter modified,left-only --stat --ignore *.o a b")).unwrap();
        match c.cmd {
            Command::Diff { format, filter, stat } => {
                assert_eq!(format, Format::Json);
                assert_eq!(filter, vec![Status::Modified, Status::LeftOnly]);
                assert!(stat);
            }
            _ => panic!(),
        }
        assert_eq!(c.ignore, vec!["*.o".to_string()]);
        assert_eq!(c.left.unwrap(), PathBuf::from("a"));

        let c = parse_args(args("check --no-ignore --dates --quick x y")).unwrap();
        assert!(matches!(c.cmd, Command::Check { stat: false }));
        assert!(c.no_ignore && c.compare_dates && c.quick && !c.no_syntax);
        assert!(parse_args(args("--no-syntax a b")).unwrap().no_syntax);
        assert!(c.git.is_none());

        let c = parse_args(args("--follow-symlinks --diff-cmd=nvim a b")).unwrap();
        assert!(matches!(c.cmd, Command::Tui));
        assert_eq!(c.diff_cmd.as_deref(), Some("nvim"));

        assert!(matches!(parse_args(args("-h")).unwrap().cmd, Command::Help));
        assert!(parse_args(args("a")).is_err());
        assert!(parse_args(args("--bogus a b")).is_err());
        assert!(parse_args(args("--format")).is_err());
        assert!(parse_args(args("--format xml a b")).is_err());
        // a directory literally named "diff" after `--`
        let c = parse_args(args("-- diff other")).unwrap();
        assert!(matches!(c.cmd, Command::Tui));
        assert_eq!(c.left.unwrap(), PathBuf::from("diff"));
    }

    fn sample() -> (PathBuf, Node) {
        let root = std::env::temp_dir().join(format!("dirrcmp-cli-{}-{}", std::process::id(), crate::utmp::next()));
        let _ = fs::remove_dir_all(&root);
        let (l, r) = (root.join("l"), root.join("r"));
        for (p, c) in [
            (l.join("a/x.txt"), "1"),
            (r.join("a/x.txt"), "2"),
            (l.join("a/same.txt"), "s"),
            (r.join("a/same.txt"), "s"),
            (l.join("gone/f"), "f"),
            (r.join("new \"q\".txt"), "n"),
        ] {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        }
        let mut t = compare(&l, &r, &Opts::default());
        sort_tree(&mut t, SortKey::Name, true);
        (root, t)
    }

    #[test]
    fn renames_in_the_outputs() {
        let (root, _) = sample();
        let (l, r) = (root.join("l"), root.join("r"));
        let body: String = (0..10).map(|i| format!("row {i} of text\n")).collect();
        let edited = body.replace("row 3 of text", "changed");
        for (p, c) in [
            (l.join("m/old.txt"), &body),
            (r.join("n/new.txt"), &body),
            (l.join("q/f.rs"), &body),
            (r.join("q/g.rs"), &edited),
        ] {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        }
        let on = Opts { detect_renames: true, ..Opts::default() };
        let mut t = compare(&l, &r, &on);
        sort_tree(&mut t, SortKey::Name, true);
        let flat = render_flat(&t, &[]);
        assert!(flat.contains("R m/old.txt -> n/new.txt\n"), "{flat}");
        assert!(flat.contains("R q/f.rs -> q/g.rs ("), "{flat}");
        let tree = render_tree(&t, &[]);
        assert!(tree.contains("R   m/old.txt -> n/new.txt\n"), "{tree}");
        assert!(tree.contains("R   f.rs -> g.rs ("), "{tree}");
        let j = render_json(&t, &[]);
        assert!(j.contains("\"path\":\"n/new.txt\",\"status\":\"renamed\",\"type\":\"file\",\"from\":\"m/old.txt\",\"similarity\":100"), "{j}");
        assert!(render_stat(&t).ends_with(", 2 renamed\n"), "{}", render_stat(&t));
        // off: plain one-sided files and the old stat line
        let t = compare(&l, &r, &Opts::default());
        assert!(render_flat(&t, &[]).contains("- m/\n"));
        assert!(!render_stat(&t).contains("renamed"));
        assert!(parse_args(args("--no-renames a b")).unwrap().no_renames);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn renders() {
        let (root, t) = sample();
        assert_eq!(render_flat(&t, &[]), "~ a/x.txt\n- gone/\n+ new \"q\".txt\n");
        assert_eq!(render_flat(&t, &[Status::RightOnly]), "+ new \"q\".txt\n");
        assert_eq!(
            render_tree(&t, &[]),
            "  a/\n~   x.txt\n- gone/\n+ new \"q\".txt\n"
        );
        assert_eq!(render_tree(&t, &[Status::LeftOnly]), "- gone/\n");
        let j = render_json(&t, &[Status::Modified]);
        assert_eq!(j, "[\n  {\"path\":\"a/x.txt\",\"status\":\"modified\",\"type\":\"file\"}\n]\n");
        assert_eq!(render_json(&t, &[Status::RightOnly]).contains("new \\\"q\\\".txt"), true);
        assert_eq!(render_stat(&t), "1 modified, 1 left-only, 1 right-only, 1 identical\n");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_json() {
        let (root, t) = sample();
        assert_eq!(render_json(&t, &[Status::Modified, Status::LeftOnly, Status::RightOnly]).lines().count() > 2, true);
        let same = compare(&root.join("l/a"), &root.join("l/a"), &Opts::default());
        assert_eq!(render_json(&same, &[]), "[]\n");
        assert_eq!(render_tree(&same, &[]), "");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn file_diff_text() {
        let fd = FileDiff::Text(engine::unified("a\nb\n", "a\nc\n", 3));
        let out = render_file_diff(&fd, Path::new("L"), Path::new("R"));
        assert!(out.starts_with("--- L\n+++ R\n@@ -1,2 +1,2 @@\n a\n"));
        assert!(out.contains("\n-b\n+c\n"));
    }
    #[test]
    fn pair_ignore_files() {
        let root = std::env::temp_dir().join(format!("dirrcmp-pairs-{}-{}", std::process::id(), crate::utmp::next()));
        let _ = fs::remove_dir_all(&root);
        let (a, b) = (root.join("a"), root.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        // order independent, and different pairs differ
        assert_eq!(pair_key(&a, &b), pair_key(&b, &a));
        assert_ne!(pair_key(&a, &b), pair_key(&a, &a));
        assert_eq!(pair_key(&a, &b).len(), 16);

        let f = root.join("data/pairs/x.ignore");
        assert!(read_pair_ignore(&f).is_empty()); // missing file
        let lines = vec!["/build/".to_string(), "*.log".to_string()];
        write_pair_ignore(&f, &lines, "left <-> right\nsecond line").unwrap();
        let text = fs::read_to_string(&f).unwrap();
        assert!(text.starts_with("# left <-> right second line\n"));
        assert_eq!(read_pair_ignore(&f), lines);
        // hand-edited files: blanks, comments and surrounding spaces are tolerated
        fs::write(&f, "\n# note\n  /x/  \n\n*.o\n").unwrap();
        assert_eq!(read_pair_ignore(&f), vec!["/x/".to_string(), "*.o".to_string()]);
        // an empty list removes the file; removing twice is fine
        write_pair_ignore(&f, &[], "h").unwrap();
        assert!(!f.exists());
        write_pair_ignore(&f, &[], "h").unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn git_mode_takes_revisions_instead_of_paths() {
        let g = |a: &str| parse_args(args(a));
        let c = g("--git").unwrap();
        let o = c.git.unwrap();
        assert!(o.revs.is_empty() && !o.staged && o.repo == PathBuf::from("."));
        assert!(c.left.is_none() && c.right.is_none());
        assert!(matches!(c.cmd, Command::Tui));
        let o = g("--git HEAD~2 --staged").unwrap().git.unwrap();
        assert_eq!(o.revs, ["HEAD~2"]);
        assert!(o.staged);
        let o = g("--git v1 v2 --repo /tmp/x").unwrap().git.unwrap();
        assert_eq!(o.revs, ["v1", "v2"]);
        assert_eq!(o.repo, PathBuf::from("/tmp/x"));
        assert!(g("--git --cached").unwrap().git.unwrap().staged);
        // subcommands work with it, in either order
        let c = g("diff --git main --stat").unwrap();
        assert!(matches!(c.cmd, Command::Diff { stat: true, .. }) && c.git.unwrap().revs == ["main"]);
        assert!(matches!(g("check --git").unwrap().cmd, Command::Check { .. }));
        // errors
        assert!(g("--git a b c").unwrap_err().contains("at most two"));
        assert!(g("--staged a b").unwrap_err().contains("--git"));
        assert!(g("--repo x a b").unwrap_err().contains("--git"));
        assert!(g("a b").unwrap().git.is_none());
        assert!(g("--git --help").is_ok());
    }
}
