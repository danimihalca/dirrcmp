//! Interactive two-panel TUI (ratatui 0.29).

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::cli;
use crate::config;
use crate::engine::{
    self, DiffLine, FileDiff, Ignore, LineKind, Meta, Node, Opts, SortKey, Status,
};
use crate::keys::{self, Chord, Key, Keymap, Tr};
use crate::highlight::{self, Kind};
use crate::fuzzy;
use crate::side::{self, Outcome, Request, SideView};
use crate::smooth::{self as smooth_mod, Scroller};
use std::time::Instant;
use crate::undo::{Journal, Recipe};

pub struct Config {
    pub left: PathBuf,
    pub right: PathBuf,
    pub opts: Opts,
    pub ignore_all: Ignore,
    pub ignore_on: bool,
    /// Preferences as read from `config.json` (what the preferences menu edits).
    pub prefs: config::Config,
    /// Command-line overrides, which win over `prefs` for this run only.
    pub cli_dates: bool,
    pub cli_quick: bool,
    pub cli_diff: Option<String>,
    /// `--no-syntax`: no highlighting for this run, whatever the preferences say.
    pub cli_no_syntax: bool,
    /// `--no-renames`: no rename detection for this run.
    pub cli_no_renames: bool,
    /// Sides that are read-only snapshots (git mode): (left, right).
    pub readonly: (bool, bool),
    /// Panel titles to show instead of the paths (git mode): (left, right).
    pub labels: Option<(String, String)>,
    pub keymap: Keymap,
    /// Problems found in `keybindings.json`, shown on start.
    pub key_warnings: Vec<String>,
    /// Where preferences / key bindings are saved (`None`: can't save).
    pub config_file: Option<PathBuf>,
    pub keys_file: Option<PathBuf>,
    /// File that stores the ignore patterns added for this pair of directories.
    pub pair_ignore: Option<PathBuf>,
}

pub fn run(cfg: Config) -> io::Result<()> {
    let mut app = App::new(cfg);
    let mut terminal = ratatui::init();
    let res = app.event_loop(&mut terminal);
    ratatui::restore();
    res
}

/// Two plain files: open the side-by-side editor on its own. Returns the
/// `FileDiff` to print instead when the files can't be edited as text.
pub fn run_files(left: PathBuf, right: PathBuf, syntax: bool, smooth: bool) -> io::Result<Option<FileDiff>> {
    let mut view = match SideView::open(left, right, true, true) {
        Ok(v) => v,
        Err(fd) => return Ok(Some(fd)),
    };
    view.set_syntax(syntax);
    view.set_smooth(smooth);
    let mut terminal = ratatui::init();
    let res = file_loop(&mut terminal, &mut view);
    ratatui::restore();
    res.map(|_| None)
}

fn file_loop(term: &mut DefaultTerminal, view: &mut SideView) -> io::Result<()> {
    loop {
        view.tick_scroll();
        term.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(1)])
                .split(f.area());
            view.draw(f, chunks[0]);
            f.render_widget(
                Paragraph::new(view.status()).style(Style::default().add_modifier(Modifier::REVERSED)),
                chunks[1],
            );
        })?;
        if view.scroll_active() && !event::poll(smooth_mod::FRAME)? {
            continue;
        }
        // Handle everything that is already queued before drawing again. Terminals turn a
        // mouse-wheel flick into a burst of Up/Down keys; redrawing after each one makes the
        // view lag behind and keep scrolling after the wheel has stopped or reversed.
        loop {
            if let Event::Key(k) = event::read()? {
                // Ctrl-C is ignored like in the directory view: it must not drop edits.
                if k.kind == KeyEventKind::Press {
                    if let Outcome::Close { .. } = view.on_key(k) {
                        return Ok(());
                    }
                }
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }
}

/// What the key handler needs from the terminal: running an external program
/// (so tests can drive the app without a real terminal).
trait Host {
    fn run(&mut self, cmd: &str, paths: &[&Path]) -> io::Result<String>;
}

impl Host for DefaultTerminal {
    fn run(&mut self, cmd: &str, paths: &[&Path]) -> io::Result<String> {
        run_external(self, cmd, paths)
    }
}

// ───────────────────────────── types ─────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

impl Side {
    fn other(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    All,
    Different,
    Modified,
    LeftOnly,
    RightOnly,
}

impl StatusFilter {
    fn next(self) -> Self {
        match self {
            StatusFilter::All => StatusFilter::Different,
            StatusFilter::Different => StatusFilter::Modified,
            StatusFilter::Modified => StatusFilter::LeftOnly,
            StatusFilter::LeftOnly => StatusFilter::RightOnly,
            StatusFilter::RightOnly => StatusFilter::All,
        }
    }
    fn label(self) -> &'static str {
        match self {
            StatusFilter::All => "all",
            StatusFilter::Different => "different",
            StatusFilter::Modified => "modified",
            StatusFilter::LeftOnly => "left-only",
            StatusFilter::RightOnly => "right-only",
        }
    }
    fn matches(self, s: Status) -> bool {
        match self {
            StatusFilter::All => true,
            StatusFilter::Different => s != Status::Same,
            StatusFilter::Modified => s == Status::Modified,
            StatusFilter::LeftOnly => s == Status::LeftOnly,
            StatusFilter::RightOnly => s == Status::RightOnly,
        }
    }
}

struct Filter {
    text: String, // lowercase
    status: StatusFilter,
}

impl Filter {
    fn is_none(&self) -> bool {
        self.text.is_empty() && self.status == StatusFilter::All
    }
    fn matches(&self, n: &Node) -> bool {
        (self.text.is_empty() || n.name.to_lowercase().contains(&self.text))
            && self.status.matches(n.status)
    }
}

fn visible(n: &Node, f: &Filter) -> bool {
    if f.is_none() || f.matches(n) {
        return true;
    }
    n.children.iter().any(|c| visible(c, f))
}

#[derive(Clone)]
struct Row {
    rel: PathBuf,
    /// Path on the right side; differs from `rel` below a paired directory.
    rrel: PathBuf,
    /// A directory paired by hand with a differently named one.
    paired: bool,
    name: String,
    depth: usize,
    is_dir: bool,
    expanded: bool,
    status: Status,
    left: Option<Meta>,
    right: Option<Meta>,
    /// Detected rename/move: similarity in percent (`rel` is the old path, `rrel` the new one).
    rename: Option<u8>,
}

fn flatten(nodes: &[Node], depth: usize, expanded: &HashSet<PathBuf>, f: &Filter, out: &mut Vec<Row>) {
    for n in nodes {
        if !visible(n, f) {
            continue;
        }
        let is_dir = n.is_dir();
        // With a filter active, open directories that have matching children.
        // A status-only filter does not open one-sided directories (everything
        // inside them has the same status, so it would just be noise).
        let exp = is_dir
            && if f.is_none() {
                expanded.contains(&n.rel)
            } else {
                (!f.text.is_empty() || n.status == Status::Modified)
                    && n.children.iter().any(|c| visible(c, f))
            };
        out.push(Row {
            rel: n.rel.clone(),
            rrel: n.rrel.clone(),
            paired: n.is_paired(),
            name: n.name.clone(),
            depth,
            is_dir,
            expanded: exp,
            status: n.status,
            left: n.left.clone(),
            right: n.right.clone(),
            rename: n.rename,
        });
        if exp {
            flatten(&n.children, depth + 1, expanded, f, out);
        }
    }
}

/// A file of the comparison, whether or not its directory is expanded in the list.
#[derive(Clone)]
struct FileEntry {
    rel: PathBuf,
    rrel: PathBuf,
    name: String,
    status: Status,
    left: Option<engine::Meta>,
    right: Option<engine::Meta>,
    rename: Option<u8>,
}

impl FileEntry {
    fn from_row(r: &Row) -> FileEntry {
        FileEntry {
            rel: r.rel.clone(),
            rrel: r.rrel.clone(),
            name: r.name.clone(),
            status: r.status,
            left: r.left.clone(),
            right: r.right.clone(),
            rename: r.rename,
        }
    }

    fn to_row(&self) -> Row {
        Row {
            rel: self.rel.clone(),
            rrel: self.rrel.clone(),
            paired: false,
            name: self.name.clone(),
            depth: 0,
            is_dir: false,
            expanded: false,
            status: self.status,
            left: self.left.clone(),
            right: self.right.clone(),
            rename: self.rename,
        }
    }

    /// Path shown (and matched) in the picker: the new path for a renamed file.
    fn label(&self) -> String {
        let p = if self.rename.is_some() { &self.rrel } else { &self.rel };
        p.to_string_lossy().replace('\\', "/")
    }
}

fn collect_files(nodes: &[Node], out: &mut Vec<FileEntry>) {
    for n in nodes {
        if n.is_dir() {
            collect_files(&n.children, out);
        } else {
            out.push(FileEntry {
                rel: n.rel.clone(),
                rrel: n.rrel.clone(),
                name: n.name.clone(),
                status: n.status,
                left: n.left.clone(),
                right: n.right.clone(),
                rename: n.rename,
            });
        }
    }
}

struct PickerUi {
    query: String,
    sel: usize,
    top: usize,
    changed_only: bool,
    items: Vec<FileEntry>,
    /// (index into `items`, matched char positions of the label), best first.
    shown: Vec<(usize, Vec<usize>)>,
    /// The editor to return to (the picker was opened from it).
    back: Option<Box<SideView>>,
}

impl PickerUi {
    fn refilter(&mut self) {
        let mut scored: Vec<(i32, usize, Vec<usize>)> = Vec::new();
        for (i, it) in self.items.iter().enumerate() {
            if self.changed_only && it.status == Status::Same {
                continue;
            }
            if let Some(m) = fuzzy::fuzzy(&self.query, &it.label()) {
                scored.push((m.score, i, m.positions));
            }
        }
        // best score first; ties keep the order of the tree
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.shown = scored.into_iter().map(|(_, i, p)| (i, p)).collect();
        self.sel = 0;
        self.top = 0;
    }
}

fn collect_dirs(n: &Node, only_modified: bool, out: &mut HashSet<PathBuf>) {
    for c in &n.children {
        if c.is_dir() && (!only_modified || c.status == Status::Modified || c.contains_rename()) {
            out.insert(c.rel.clone());
            collect_dirs(c, only_modified, out);
        }
    }
}

enum Action {
    Copy { src: PathBuf, dst: PathBuf, rel: PathBuf },
    Delete { path: PathBuf },
}

struct Confirm {
    prompt: String,
    action: Action,
}

struct DiffView {
    title: String,
    lines: Vec<DiffLine>,
    note: Option<String>,
    scroll: usize,
    added: usize,
    removed: usize,
    syntax: Option<UniSyntax>,
    chunk_bg: bool,
    scroller: Scroller,
}

/// Highlighting for both files of a unified diff, indexed by line number.
struct UniSyntax {
    left: Vec<Vec<Kind>>,
    right: Vec<Vec<Kind>>,
}

impl UniSyntax {
    fn load(l: Option<&Path>, r: Option<&Path>) -> Option<UniSyntax> {
        let one = |p: Option<&Path>| -> Vec<Vec<Kind>> {
            let Some(p) = p else { return Vec::new() };
            let Ok(t) = engine::load_text(Some(p)) else { return Vec::new() };
            let first = t.lines.first().map(|s| s.as_str());
            highlight::highlight_file(p, first, &t.lines).map(|x| x.1).unwrap_or_default()
        };
        let (left, right) = (one(l), one(r));
        if left.is_empty() && right.is_empty() {
            None
        } else {
            Some(UniSyntax { left, right })
        }
    }

    fn kinds(&self, l: &DiffLine) -> Option<&[Kind]> {
        let (v, no) = match l.kind {
            LineKind::Del => (&self.left, l.left_no),
            _ => (&self.right, l.right_no.or(l.left_no)),
        };
        v.get(no?.checked_sub(1)?).map(|k| k.as_slice())
    }
}

impl DiffView {
    /// Scroll by `n` lines, staying inside the text.
    fn shift(&mut self, n: isize) {
        let max = self.lines.len().saturating_sub(1) as isize;
        self.scroll = (self.scroll as isize + n).clamp(0, max) as usize;
    }

    fn new(rel: &Path, fd: FileDiff) -> DiffView {
        let title = engine::rel_string(rel);
        let note_title = title.clone();
        let note_only = move |note: String| DiffView {
            title: note_title.clone(),
            lines: Vec::new(),
            note: Some(note),
            scroll: 0,
            added: 0,
            removed: 0,
            syntax: None,
            chunk_bg: false,
            scroller: Scroller::default(),
        };
        match fd {
            FileDiff::Text(d) => {
                let empty = d.lines.is_empty();
                DiffView {
                    title,
                    added: d.added,
                    removed: d.removed,
                    lines: d.lines,
                    note: if empty { Some("(empty file)".to_string()) } else { None },
                    scroll: 0,
                    syntax: None,
                    chunk_bg: false,
                    scroller: Scroller::default(),
                }
            }
            FileDiff::Identical => note_only("Files are identical".to_string()),
            FileDiff::Binary => note_only("Binary files differ".to_string()),
            FileDiff::TooLarge(n) => {
                note_only(format!("File too large to diff ({})", engine::human_size(n)))
            }
            FileDiff::TypeMismatch => note_only("One side is a directory".to_string()),
            FileDiff::Error(e) => note_only(format!("error: {e}")),
        }
    }
}

/// A directory or file marked with `m`, waiting for its counterpart on the other side.
struct Mark {
    rel: PathBuf,
    rrel: PathBuf,
    status: Status,
    is_dir: bool,
}

/// State of the ignore-pattern manager (`I`).
struct IgnoreUi {
    sel: usize,
    adding: Option<String>,
}

struct PrefsUi {
    sel: usize,
    /// Text being typed for the external diff command.
    editing: Option<String>,
}

const PREF_ROWS: usize = 10;

struct KeysUi {
    sel: usize,
    /// `Some(add)` while waiting for the key to bind (`add`: keep existing keys).
    capture: Option<bool>,
}

struct ActionsUi {
    items: Vec<(usize, String)>,
    sel: usize,
}

enum Mode {
    Normal,
    Prefs(PrefsUi),
    Keys(KeysUi),
    Actions(ActionsUi),
    Diff(DiffView),
    Side(Box<SideView>),
    Picker(Box<PickerUi>),
    Filter(String),
    Confirm(Confirm),
    Ignores(IgnoreUi),
    Help,
}

// ───────────────────────────── app ─────────────────────────────

struct App {
    left: PathBuf,
    right: PathBuf,
    opts: Opts,
    /// Defaults + global file + `--ignore` (the per-pair lines come on top).
    ignore_all: Ignore,
    ignore_on: bool,
    pair_lines: Vec<String>,
    pair_file: Option<PathBuf>,
    mark: Option<Mark>,
    diff_cmd: Option<String>,
    dirs_first: bool,
    show_hints: bool,
    nerd_font: bool,
    prefs: config::Config,
    cli_dates: bool,
    cli_quick: bool,
    cli_diff: Option<String>,
    cli_no_syntax: bool,
    cli_no_renames: bool,
    readonly: (bool, bool),
    labels: Option<(String, String)>,
    syntax: bool,
    chunk_bg: bool,
    smooth: bool,
    scroller: Scroller,
    /// File sidebar in the side-by-side editor: shown / has the keyboard.
    sidebar: bool,
    sb_focus: bool,
    sb_top: usize,
    /// Terminal width at the last draw (the sidebar needs room).
    term_w: u16,
    /// The file open in the side-by-side editor (left-relative path).
    side_rel: Option<PathBuf>,
    keymap: Keymap,
    config_file: Option<PathBuf>,
    keys_file: Option<PathBuf>,
    tree: Node,
    journal: Journal,
    expanded: HashSet<PathBuf>,
    rows: Vec<Row>,
    cursor: usize,
    top: usize,
    focus: Side,
    mode: Mode,
    pending: Option<char>,
    msg: String,
    sort: SortKey,
    status_filter: StatusFilter,
    filter_text: String,
    view_h: usize,
}

impl App {
    fn new(cfg: Config) -> App {
        let sort = SortKey::Name;
        let pair_lines = cfg.pair_ignore.as_deref().map(cli::read_pair_ignore).unwrap_or_default();
        let mut opts = cfg.opts;
        opts.compare_dates = cfg.cli_dates || cfg.prefs.compare_dates;
        opts.compare_contents = cfg.prefs.compare_contents && !cfg.cli_quick;
        opts.detect_renames = cfg.prefs.renames && !cfg.cli_no_renames;
        opts.ignore = if cfg.ignore_on {
            let mut ig = cfg.ignore_all.clone();
            for l in &pair_lines {
                ig.add(l);
            }
            ig
        } else {
            Ignore::new()
        };
        let mut tree = engine::compare(&cfg.left, &cfg.right, &opts);
        engine::sort_tree(&mut tree, sort, cfg.prefs.dirs_first);
        let mut expanded = HashSet::new();
        collect_dirs(&tree, true, &mut expanded);
        let mut app = App {
            left: cfg.left,
            right: cfg.right,
            opts,
            ignore_all: cfg.ignore_all,
            ignore_on: cfg.ignore_on,
            pair_lines,
            pair_file: cfg.pair_ignore,
            mark: None,
            diff_cmd: cfg.cli_diff.clone().or_else(|| cfg.prefs.diff_command.clone()),
            dirs_first: cfg.prefs.dirs_first,
            show_hints: cfg.prefs.show_hints,
            nerd_font: cfg.prefs.nerd_font,
            syntax: cfg.prefs.syntax && !cfg.cli_no_syntax,
            chunk_bg: cfg.prefs.chunk_bg,
            smooth: cfg.prefs.smooth,
            scroller: Scroller::default(),
            sidebar: false,
            sb_focus: false,
            sb_top: 0,
            term_w: 100,
            side_rel: None,
            prefs: cfg.prefs,
            cli_dates: cfg.cli_dates,
            cli_quick: cfg.cli_quick,
            cli_no_syntax: cfg.cli_no_syntax,
            cli_no_renames: cfg.cli_no_renames,
            readonly: cfg.readonly,
            labels: cfg.labels,
            cli_diff: cfg.cli_diff,
            keymap: cfg.keymap,
            config_file: cfg.config_file,
            keys_file: cfg.keys_file,
            tree,
            journal: Journal::new(),
            expanded,
            rows: Vec::new(),
            cursor: 0,
            top: 0,
            focus: Side::Left,
            mode: Mode::Normal,
            pending: None,
            msg: String::new(),
            sort,
            status_filter: StatusFilter::All,
            filter_text: String::new(),
            view_h: 20,
        };
        app.rebuild_rows();
        if let Some(first) = cfg.key_warnings.first() {
            let more = cfg.key_warnings.len() - 1;
            app.msg = if more > 0 {
                format!("keybindings: {first} (+{more} more)")
            } else {
                format!("keybindings: {first}")
            };
        }
        app
    }

    fn event_loop(&mut self, term: &mut DefaultTerminal) -> io::Result<()> {
        loop {
            self.tick_scroll();
            term.draw(|f| self.draw(f))?;
            // While an animation runs, wake up every frame instead of waiting for a key.
            if self.scroll_active() && !event::poll(smooth_mod::FRAME)? {
                continue;
            }
            // Drain the queue before redrawing (see `file_loop`).
            loop {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press && self.on_key(k, term)? {
                        return Ok(());
                    }
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }

    fn filter(&self) -> Filter {
        Filter { text: self.filter_text.to_lowercase(), status: self.status_filter }
    }

    fn rebuild_rows(&mut self) {
        let selected = self.rows.get(self.cursor).map(|r| r.rel.clone());
        let f = self.filter();
        let mut rows = Vec::new();
        flatten(&self.tree.children, 0, &self.expanded, &f, &mut rows);
        if let Some(sel) = selected {
            if let Some(i) = rows.iter().position(|r| r.rel == sel) {
                self.cursor = i;
            }
        }
        self.rows = rows;
        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
    }

    fn refresh(&mut self) {
        self.tree = engine::compare(&self.left, &self.right, &self.opts);
        engine::sort_tree(&mut self.tree, self.sort, self.dirs_first);
        self.rebuild_rows();
    }

    fn adjust_top(&mut self) {
        let h = self.view_h.max(1);
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + h {
            self.top = self.cursor + 1 - h;
        }
    }

    // ── key handling ──

    fn on_key(&mut self, mut key: KeyEvent, term: &mut dyn Host) -> io::Result<bool> {
        // Some terminals (kitty protocol, Windows) report Shift+r as 'r' + SHIFT, not 'R'.
        if let KeyCode::Char(c) = key.code {
            if key.modifiers.contains(KeyModifiers::SHIFT)
                && !key.modifiers.contains(KeyModifiers::CONTROL)
                && c.is_ascii_lowercase()
            {
                key.code = KeyCode::Char(c.to_ascii_uppercase());
            }
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl-C must not throw away unsaved edits in the side-by-side view.
        let editing = matches!(self.mode, Mode::Side(_))
            || matches!(&self.mode, Mode::Picker(p) if p.back.is_some());
        if ctrl && key.code == KeyCode::Char('c') && !editing {
            return Ok(true);
        }
        self.msg.clear();
        // Any other key ends a running scroll animation first, so it acts on the final position.
        if !(side::is_smooth_key(&key) && matches!(self.mode, Mode::Normal | Mode::Diff(_) | Mode::Side(_))) {
            self.flush_scroll();
        }
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        match mode {
            Mode::Normal => self.normal_dispatch(key, term),
            Mode::Help => Ok(false),
            Mode::Prefs(ui) => {
                self.prefs_key(ui, key);
                Ok(false)
            }
            Mode::Keys(ui) => {
                self.keys_key(ui, key);
                Ok(false)
            }
            Mode::Actions(ui) => self.actions_key(ui, key, term),
            Mode::Side(mut sv) => {
                if self.sb_active() && self.sb_focus {
                    self.sidebar_key(sv, key, term)?;
                    return Ok(false);
                }
                match sv.on_key(key) {
                    Outcome::Stay => match sv.take_request() {
                        None => self.mode = Mode::Side(sv),
                        Some(r) => self.side_request(sv, r, term)?,
                    },
                    Outcome::Close { saved } => {
                        self.side_rel = None;
                        self.sb_focus = false;
                        if saved {
                            self.refresh();
                        }
                    }
                }
                Ok(false)
            }
            Mode::Picker(ui) => {
                self.picker_key(ui, key, term)?;
                Ok(false)
            }
            Mode::Filter(mut s) => {
                match key.code {
                    KeyCode::Enter => {
                        self.filter_text = s;
                        self.rebuild_rows();
                    }
                    KeyCode::Esc => {
                        self.filter_text.clear();
                        self.rebuild_rows();
                    }
                    KeyCode::Backspace => {
                        s.pop();
                        self.filter_text = s.clone();
                        self.rebuild_rows();
                        self.mode = Mode::Filter(s);
                    }
                    KeyCode::Char(c) if !ctrl => {
                        s.push(c);
                        self.filter_text = s.clone();
                        self.rebuild_rows();
                        self.mode = Mode::Filter(s);
                    }
                    _ => self.mode = Mode::Filter(s),
                }
                Ok(false)
            }
            Mode::Confirm(c) => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter) {
                    self.perform(c.action);
                }
                Ok(false)
            }
            Mode::Ignores(ui) => {
                self.ignores_key(ui, key);
                Ok(false)
            }
            Mode::Diff(mut dv) => {
                if self.diff_key(&mut dv, key) {
                    self.mode = Mode::Diff(dv);
                }
                Ok(false)
            }
        }
    }

    /// Apply the user's key bindings, then run the (possibly rewritten) keys.
    fn normal_dispatch(&mut self, key: KeyEvent, term: &mut dyn Host) -> io::Result<bool> {
        // The second key of a sequence like `gg` is never remapped.
        if self.pending.is_none() && !self.keymap.is_default() {
            if let Some(c) = chord_of(&key) {
                match self.keymap.translate(c) {
                    Tr::Pass => {}
                    Tr::Drop => return Ok(false),
                    Tr::Feed(seq) => return self.feed(&seq, term),
                }
            }
        }
        self.normal_key(key, term)
    }

    fn feed(&mut self, seq: &[Chord], term: &mut dyn Host) -> io::Result<bool> {
        for c in seq {
            if self.normal_key(event_of(*c), term)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn normal_key(&mut self, key: KeyEvent, term: &mut dyn Host) -> io::Result<bool> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if let Some(p) = self.pending.take() {
            match (p, key.code) {
                ('g', KeyCode::Char('g')) => self.cursor = 0,
                ('z', KeyCode::Char('R')) => self.expand_all(true),
                ('z', KeyCode::Char('M')) => self.expand_all(false),
                ('z', KeyCode::Char('O')) => self.fold_here(true, true),
                ('z', KeyCode::Char('C')) => self.fold_here(false, true),
                ('z', KeyCode::Char('o')) => self.fold_here(true, false),
                ('z', KeyCode::Char('c')) => self.fold_here(false, false),
                ('z', KeyCode::Char('a')) => {
                    let open = self.dir_here().map_or(true, |d| !self.expanded.contains(&d));
                    self.fold_here(open, false)
                }
                ('z', KeyCode::Char('i')) => self.toggle_ignore(),
                (']', KeyCode::Char('c')) => self.jump_diff(true),
                ('[', KeyCode::Char('c')) => self.jump_diff(false),
                _ => {}
            }
            return Ok(false);
        }

        let half = (self.view_h / 2).max(1) as isize;
        let page = self.view_h.max(1) as isize;
        match key.code {
            KeyCode::Char('p') if ctrl => self.open_picker(None),
            KeyCode::Char('d') if ctrl => self.scroll_lines(half),
            KeyCode::Char('u') if ctrl => self.scroll_lines(-half),
            KeyCode::Char('f') if ctrl => self.scroll_lines(page),
            KeyCode::Char('b') if ctrl => self.scroll_lines(-page),
            KeyCode::Char('e') if ctrl => self.scroll_view(1),
            KeyCode::Char('y') if ctrl => self.scroll_view(-1),
            _ if ctrl => {}
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('j') | KeyCode::Down => self.mv(1),
            KeyCode::Char('k') | KeyCode::Up => self.mv(-1),
            KeyCode::PageDown => self.scroll_lines(page),
            KeyCode::PageUp => self.scroll_lines(-page),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = self.rows.len().saturating_sub(1),
            KeyCode::Char(c @ ('g' | 'z' | ']' | '[')) => self.pending = Some(c),
            KeyCode::Tab | KeyCode::Char('%') => self.focus = self.focus.other(),
            KeyCode::Char('H') => self.focus = Side::Left,
            KeyCode::Char('L') => self.focus = Side::Right,
            KeyCode::Char('l') | KeyCode::Right => self.expand_or_open(term)?,
            KeyCode::Char('h') | KeyCode::Left => self.collapse_or_parent(),
            KeyCode::Enter => self.open_selected(term)?,
            KeyCode::Char('D') => self.open_unified(),
            KeyCode::Char('u') => self.undo_redo(true),
            KeyCode::Char('U') => self.undo_redo(false),
            KeyCode::Char('>') => self.start_copy(Side::Right),
            KeyCode::Char('<') => self.start_copy(Side::Left),
            KeyCode::Char(' ') => {
                let to = self.focus.other();
                self.start_copy(to)
            }
            KeyCode::Char('d') => self.start_delete(),
            KeyCode::Char('R') => self.apply_rename(),
            KeyCode::Char('y') => self.yank(),
            KeyCode::Char('e') => self.open_editor(term)?,
            KeyCode::Char('r') => {
                self.refresh();
                self.msg = "refreshed".to_string();
            }
            KeyCode::Char('S') => {
                std::mem::swap(&mut self.left, &mut self.right);
                self.readonly = (self.readonly.1, self.readonly.0);
                if let Some(l) = &mut self.labels {
                    *l = (l.1.clone(), l.0.clone());
                }
                self.refresh();
                self.msg = "swapped panels".to_string();
            }
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                engine::sort_tree(&mut self.tree, self.sort, self.dirs_first);
                self.rebuild_rows();
                self.msg = format!("sort: {}", self.sort.label());
            }
            KeyCode::Char('/') => self.mode = Mode::Filter(self.filter_text.clone()),
            KeyCode::Char('f') => {
                self.status_filter = self.status_filter.next();
                self.rebuild_rows();
            }
            KeyCode::Char('i') => self.ignore_selected(),
            KeyCode::Char('I') => self.mode = Mode::Ignores(IgnoreUi { sel: 0, adding: None }),
            KeyCode::Char('m') => self.mark_pair(),
            KeyCode::Char('M') => self.unpair(),
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char(',') => self.mode = Mode::Prefs(PrefsUi { sel: 0, editing: None }),
            KeyCode::Char('K') => self.mode = Mode::Keys(KeysUi { sel: 0, capture: None }),
            KeyCode::Char('.') => self.open_actions(),
            KeyCode::Esc => {
                if self.mark.take().is_some() {
                    self.msg = "mark cancelled".to_string();
                } else if !self.filter_text.is_empty() || self.status_filter != StatusFilter::All {
                    self.filter_text.clear();
                    self.status_filter = StatusFilter::All;
                    self.rebuild_rows();
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn diff_key(&mut self, dv: &mut DiffView, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let h = self.view_h.max(1);
        let max = dv.lines.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('d') if ctrl => self.scroll_diff(dv, (h / 2) as isize),
            KeyCode::Char('u') if ctrl => self.scroll_diff(dv, -((h / 2) as isize)),
            KeyCode::Char('f') if ctrl => self.scroll_diff(dv, h as isize),
            KeyCode::Char('b') if ctrl => self.scroll_diff(dv, -(h as isize)),
            _ if ctrl => {}
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Char('h') | KeyCode::Left => {
                return false
            }
            KeyCode::Char('j') | KeyCode::Down => dv.scroll = (dv.scroll + 1).min(max),
            KeyCode::Char('k') | KeyCode::Up => dv.scroll = dv.scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_diff(dv, h as isize),
            KeyCode::PageUp => self.scroll_diff(dv, -(h as isize)),
            KeyCode::Char('g') | KeyCode::Home => dv.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => dv.scroll = max,
            KeyCode::Char('n') | KeyCode::Char(']') => {
                if let Some(i) = dv
                    .lines
                    .iter()
                    .enumerate()
                    .skip(dv.scroll + 1)
                    .find(|(_, l)| l.kind == LineKind::Hunk)
                    .map(|(i, _)| i)
                {
                    dv.scroll = i;
                }
            }
            KeyCode::Char('N') | KeyCode::Char('[') => {
                let end = dv.scroll.min(dv.lines.len());
                if let Some(i) = dv.lines[..end].iter().rposition(|l| l.kind == LineKind::Hunk) {
                    dv.scroll = i;
                }
            }
            _ => {}
        }
        true
    }

    // ── file picker, file switching, sidebar ──

    /// The sidebar is shown only when the terminal is wide enough.
    fn sb_active(&self) -> bool {
        self.sidebar && self.term_w >= 80
    }

    fn files(&self) -> Vec<FileEntry> {
        let mut v = Vec::new();
        collect_files(&self.tree.children, &mut v);
        v
    }

    fn open_picker(&mut self, back: Option<Box<SideView>>) {
        let items = self.files();
        if items.is_empty() {
            self.msg = "no files to pick from".to_string();
            if let Some(sv) = back {
                self.mode = Mode::Side(sv);
            }
            return;
        }
        let mut ui = PickerUi { query: String::new(), sel: 0, top: 0, changed_only: false, items, shown: Vec::new(), back };
        ui.refilter();
        self.mode = Mode::Picker(Box::new(ui));
    }

    fn picker_key(&mut self, mut ui: Box<PickerUi>, key: KeyEvent, term: &mut dyn Host) -> io::Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let n = ui.shown.len();
        match key.code {
            KeyCode::Esc => return self.close_picker(ui),
            KeyCode::Char('c') | KeyCode::Char('g') if ctrl => return self.close_picker(ui),
            KeyCode::Enter => {
                let Some((i, _)) = ui.shown.get(ui.sel) else {
                    self.mode = Mode::Picker(ui);
                    return Ok(());
                };
                let entry = ui.items[*i].clone();
                return match ui.back.take() {
                    Some(sv) => self.switch_file(sv, &entry, term),
                    None => self.open_entry(&entry, term),
                };
            }
            KeyCode::Up => ui.sel = ui.sel.saturating_sub(1),
            KeyCode::Down => ui.sel = (ui.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char('p' | 'k') if ctrl => ui.sel = ui.sel.saturating_sub(1),
            KeyCode::Char('n' | 'j') if ctrl => ui.sel = (ui.sel + 1).min(n.saturating_sub(1)),
            KeyCode::PageUp => ui.sel = ui.sel.saturating_sub(10),
            KeyCode::PageDown => ui.sel = (ui.sel + 10).min(n.saturating_sub(1)),
            KeyCode::Home => ui.sel = 0,
            KeyCode::End => ui.sel = n.saturating_sub(1),
            KeyCode::Tab => {
                ui.changed_only = !ui.changed_only;
                ui.refilter();
            }
            KeyCode::Backspace => {
                ui.query.pop();
                ui.refilter();
            }
            KeyCode::Char('u') if ctrl => {
                ui.query.clear();
                ui.refilter();
            }
            KeyCode::Char('w') if ctrl => {
                let t = ui.query.trim_end().len();
                let cut = ui.query[..t].rfind(|c: char| c == '/' || c == ' ').map_or(0, |p| p + 1);
                ui.query.truncate(cut);
                ui.refilter();
            }
            KeyCode::Char(c) if !ctrl => {
                ui.query.push(c);
                ui.refilter();
            }
            _ => {}
        }
        self.mode = Mode::Picker(ui);
        Ok(())
    }

    fn close_picker(&mut self, ui: Box<PickerUi>) -> io::Result<()> {
        self.mode = match ui.back {
            Some(sv) => Mode::Side(sv),
            None => Mode::Normal,
        };
        Ok(())
    }

    /// Show `e` in the list (expanding its parents) and open it.
    fn open_entry(&mut self, e: &FileEntry, term: &mut dyn Host) -> io::Result<()> {
        let mut p = e.rel.parent();
        while let Some(d) = p {
            if d.as_os_str().is_empty() {
                break;
            }
            self.expanded.insert(d.to_path_buf());
            p = d.parent();
        }
        self.rebuild_rows();
        if let Some(i) = self.rows.iter().position(|r| r.rel == e.rel) {
            self.cursor = i;
        }
        self.open_row(e.to_row(), term)
    }

    /// Replace the editor `sv` with another file, unless it has unsaved edits.
    fn switch_file(&mut self, mut sv: Box<SideView>, e: &FileEntry, term: &mut dyn Host) -> io::Result<()> {
        if sv.has_unsaved() {
            sv.note("unsaved changes: save (w) or discard them before switching files");
            self.mode = Mode::Side(sv);
            return Ok(());
        }
        if sv.saved() {
            self.refresh();
        }
        self.open_entry(e, term)
    }

    /// `]f` / `[f`: the next / previous file that differs.
    fn step_changed(&mut self, mut sv: Box<SideView>, forward: bool, term: &mut dyn Host) -> io::Result<()> {
        let files = self.files();
        let cur = self.side_rel.as_ref().and_then(|r| files.iter().position(|f| &f.rel == r));
        let differs = |f: &FileEntry| f.status != Status::Same;
        let found = match (cur, forward) {
            (Some(i), true) => files.iter().skip(i + 1).find(|f| differs(f)),
            (Some(i), false) => files.iter().take(i).rev().find(|f| differs(f)),
            (None, true) => files.iter().find(|f| differs(f)),
            (None, false) => files.iter().rev().find(|f| differs(f)),
        };
        match found {
            Some(e) => {
                let e = e.clone();
                self.switch_file(sv, &e, term)
            }
            None => {
                sv.note(if forward { "no more changed files" } else { "no earlier changed files" });
                self.mode = Mode::Side(sv);
                Ok(())
            }
        }
    }

    fn side_request(&mut self, mut sv: Box<SideView>, r: Request, term: &mut dyn Host) -> io::Result<()> {
        match r {
            Request::NextFile => self.step_changed(sv, true, term),
            Request::PrevFile => self.step_changed(sv, false, term),
            Request::Picker => {
                self.open_picker(Some(sv));
                Ok(())
            }
            Request::ToggleSidebar => {
                self.sidebar = !self.sidebar;
                self.sb_focus = false;
                if self.sidebar && self.term_w < 80 {
                    sv.note("sidebar needs a terminal at least 80 columns wide");
                } else {
                    sv.note(if self.sidebar { "sidebar on (T to focus)" } else { "sidebar off" });
                }
                self.mode = Mode::Side(sv);
                Ok(())
            }
            Request::FocusSidebar => {
                self.sidebar = true;
                self.sb_focus = self.term_w >= 80;
                if !self.sb_focus {
                    sv.note("sidebar needs a terminal at least 80 columns wide");
                }
                self.mode = Mode::Side(sv);
                Ok(())
            }
        }
    }

    /// Keys while the sidebar has the keyboard.
    fn sidebar_key(&mut self, sv: Box<SideView>, key: KeyEvent, term: &mut dyn Host) -> io::Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = (self.view_h / 2).max(1) as isize;
        let mut sv = Some(sv);
        match key.code {
            KeyCode::Esc | KeyCode::Tab | KeyCode::Char('q') => self.sb_focus = false,
            KeyCode::Char('t') => {
                self.sidebar = false;
                self.sb_focus = false;
            }
            KeyCode::Char('p') if ctrl => {
                self.open_picker(sv.take());
                return Ok(());
            }
            KeyCode::Char('d') if ctrl => self.mv(page),
            KeyCode::Char('u') if ctrl => self.mv(-page),
            KeyCode::Char('j') | KeyCode::Down => self.mv(1),
            KeyCode::Char('k') | KeyCode::Up => self.mv(-1),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = self.rows.len().saturating_sub(1),
            KeyCode::Char('h') | KeyCode::Left => self.collapse_or_parent(),
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(row) = self.rows.get(self.cursor).cloned() {
                    if row.is_dir {
                        if !self.expanded.remove(&row.rel) {
                            self.expanded.insert(row.rel.clone());
                        }
                        self.rebuild_rows();
                    } else {
                        let e = FileEntry::from_row(&row);
                        let s = sv.take().unwrap();
                        return self.switch_file(s, &e, term);
                    }
                }
            }
            _ => {}
        }
        if let Some(s) = sv {
            self.mode = Mode::Side(s);
        }
        Ok(())
    }

    // ── smooth scrolling ──

    fn scroll_lines(&mut self, n: isize) {
        if self.smooth {
            self.scroller.push(n, Instant::now());
        } else {
            self.mv(n);
        }
    }

    fn scroll_diff(&self, dv: &mut DiffView, n: isize) {
        if self.smooth {
            dv.scroller.push(n, Instant::now());
        } else {
            dv.shift(n);
        }
    }

    /// Is an animation running in the current view?
    fn scroll_active(&self) -> bool {
        match &self.mode {
            Mode::Normal => self.scroller.active(),
            Mode::Diff(dv) => dv.scroller.active(),
            Mode::Side(sv) => sv.scroll_active(),
            _ => false,
        }
    }

    /// Move the view by what the running animation says is due now.
    fn tick_scroll(&mut self) {
        match &mut self.mode {
            Mode::Normal => {
                let d = self.scroller.tick(Instant::now());
                if d != 0 {
                    self.mv(d);
                }
            }
            Mode::Diff(dv) => {
                let d = dv.scroller.tick(Instant::now());
                dv.shift(d);
            }
            Mode::Side(sv) => sv.tick_scroll(),
            _ => {}
        }
    }

    /// Finish running animations at once (before another key is handled).
    fn flush_scroll(&mut self) {
        let d = self.scroller.flush();
        if d != 0 {
            self.mv(d);
        }
        if let Mode::Diff(dv) = &mut self.mode {
            let d = dv.scroller.flush();
            dv.shift(d);
        }
    }

    // ── navigation ──

    fn mv(&mut self, delta: isize) {
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        self.cursor = (self.cursor as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    fn scroll_view(&mut self, delta: isize) {
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        let max = len as isize - 1;
        self.top = (self.top as isize + delta).clamp(0, max) as usize;
        self.cursor = (self.cursor as isize + delta).clamp(0, max) as usize;
    }

    fn expand_all(&mut self, open: bool) {
        self.expanded.clear();
        if open {
            let mut s = HashSet::new();
            collect_dirs(&self.tree, false, &mut s);
            self.expanded = s;
        }
        self.rebuild_rows();
    }

    /// The directory under the cursor, or the parent directory of the file under it.
    fn dir_here(&self) -> Option<PathBuf> {
        let r = self.rows.get(self.cursor)?;
        if r.is_dir {
            Some(r.rel.clone())
        } else {
            r.rel.parent().filter(|p| !p.as_os_str().is_empty()).map(|p| p.to_path_buf())
        }
    }

    /// `zo`/`zc` (and `zO`/`zC` with `deep`): open or close the directory under the cursor.
    fn fold_here(&mut self, open: bool, deep: bool) {
        let Some(dir) = self.dir_here() else { return };
        if open {
            self.expanded.insert(dir.clone());
            if deep {
                let mut all = HashSet::new();
                collect_dirs(&self.tree, false, &mut all);
                self.expanded.extend(all.into_iter().filter(|d| d.starts_with(&dir)));
            }
        } else if deep {
            self.expanded.retain(|d| !d.starts_with(&dir));
        } else {
            self.expanded.remove(&dir);
        }
        // closing hides the children, so the cursor goes to the directory itself
        if !open {
            if let Some(i) = self.rows.iter().position(|r| r.rel == dir) {
                self.cursor = i;
            }
        }
        self.rebuild_rows();
    }

    fn toggle_dir(&mut self, rel: &Path) {
        if !self.expanded.remove(rel) {
            self.expanded.insert(rel.to_path_buf());
        }
        self.rebuild_rows();
    }

    fn expand_or_open(&mut self, term: &mut dyn Host) -> io::Result<()> {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return Ok(());
        };
        if row.is_dir {
            if !row.expanded {
                self.expanded.insert(row.rel.clone());
                self.rebuild_rows();
            } else if self.rows.get(self.cursor + 1).map_or(false, |r| r.depth > row.depth) {
                self.cursor += 1;
            }
            Ok(())
        } else {
            self.open_selected(term)
        }
    }

    fn collapse_or_parent(&mut self) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        if row.is_dir && self.expanded.contains(&row.rel) {
            self.expanded.remove(&row.rel);
            self.rebuild_rows();
            return;
        }
        if let Some(parent) = row.rel.parent() {
            if !parent.as_os_str().is_empty() {
                if let Some(i) = self.rows.iter().position(|r| r.rel == parent) {
                    self.cursor = i;
                }
            }
        }
    }

    fn jump_diff(&mut self, forward: bool) {
        let n = self.rows.len();
        let is_diff = |r: &Row| !r.is_dir && r.status != Status::Same;
        let found = if forward {
            (self.cursor + 1..n).find(|&i| is_diff(&self.rows[i]))
        } else {
            (0..self.cursor).rev().find(|&i| is_diff(&self.rows[i]))
        };
        match found {
            Some(i) => self.cursor = i,
            None => self.msg = "no more differences".to_string(),
        }
    }

    // ── actions ──

    /// Where `row` lives on the given side (differs below a paired directory).
    fn path_of(&self, row: &Row, side: Side) -> PathBuf {
        match side {
            Side::Left => self.left.join(&row.rel),
            Side::Right => self.right.join(&row.rrel),
        }
    }

    fn open_selected(&mut self, term: &mut dyn Host) -> io::Result<()> {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return Ok(());
        };
        self.open_row(row, term)
    }

    fn open_row(&mut self, row: Row, term: &mut dyn Host) -> io::Result<()> {
        self.side_rel = None;
        if row.is_dir {
            self.toggle_dir(&row.rel);
            return Ok(());
        }
        let lp = row.left.as_ref().map(|_| self.path_of(&row, Side::Left));
        let rp = row.right.as_ref().map(|_| self.path_of(&row, Side::Right));
        if let (Some(cmd), Some(l), Some(r)) = (self.diff_cmd.clone(), lp.as_ref(), rp.as_ref()) {
            let m = term.run(&cmd, &[l.as_path(), r.as_path()])?;
            self.msg = m;
            return Ok(());
        }
        // Side-by-side editor; binary / oversized files fall back to a note.
        match SideView::open(
            self.path_of(&row, Side::Left),
            self.path_of(&row, Side::Right),
            row.left.is_some(),
            row.right.is_some(),
        ) {
            Ok(mut v) => {
                v.set_readonly(self.readonly.0, self.readonly.1);
                v.set_syntax(self.syntax);
                v.set_chunk_bg(self.chunk_bg);
                v.set_smooth(self.smooth);
                self.side_rel = Some(row.rel.clone());
                self.mode = Mode::Side(Box::new(v));
            }
            Err(fd) => self.mode = Mode::Diff(DiffView::new(&row.rel, fd)),
        }
        Ok(())
    }

    /// Read-only unified diff (the `D` key).
    fn open_unified(&mut self) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        if row.is_dir {
            return;
        }
        let lp = row.left.as_ref().map(|_| self.path_of(&row, Side::Left));
        let rp = row.right.as_ref().map(|_| self.path_of(&row, Side::Right));
        let fd = engine::file_diff(lp.as_deref(), rp.as_deref());
        let mut dv = DiffView::new(&row.rel, fd);
        dv.chunk_bg = self.chunk_bg;
        if self.syntax {
            dv.syntax = UniSyntax::load(lp.as_deref(), rp.as_deref());
        }
        self.mode = Mode::Diff(dv);
    }

    fn open_editor(&mut self, term: &mut dyn Host) -> io::Result<()> {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return Ok(());
        };
        if row.is_dir || self.deny(self.focus) {
            return Ok(());
        }
        let (first, second) = match self.focus {
            Side::Left => ((&row.left, Side::Left), (&row.right, Side::Right)),
            Side::Right => ((&row.right, Side::Right), (&row.left, Side::Left)),
        };
        let path = if first.0.is_some() {
            self.path_of(&row, first.1)
        } else if second.0.is_some() {
            self.path_of(&row, second.1)
        } else {
            return Ok(());
        };
        let cmd = std::env::var("VISUAL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| std::env::var("EDITOR").ok().filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| "vi".to_string());
        let m = term.run(&cmd, &[path.as_path()])?;
        self.refresh();
        self.msg = m;
        Ok(())
    }

    /// True (and a message) when `side` is a read-only git snapshot.
    fn deny(&mut self, side: Side) -> bool {
        let ro = if side == Side::Left { self.readonly.0 } else { self.readonly.1 };
        if ro {
            let name = match &self.labels {
                Some(l) => if side == Side::Left { l.0.clone() } else { l.1.clone() },
                None => side.name().to_string(),
            };
            self.msg = format!("the {} side ({name}) is a read-only git snapshot", side.name());
        }
        ro
    }

    fn start_copy(&mut self, to: Side) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        let (from_meta, to_meta) = match to {
            Side::Right => (&row.left, &row.right),
            Side::Left => (&row.right, &row.left),
        };
        if self.deny(to) {
            return;
        }
        if from_meta.is_none() {
            self.msg = format!("'{}' is missing on the {} side", row.name, to.other().name());
            return;
        }
        if row.status == Status::Same {
            self.msg = "already identical".to_string();
            return;
        }
        if row.rename == Some(100) {
            self.msg = "same content under another path: R renames, > / < would only rewrite it".to_string();
            return;
        }
        let action = Action::Copy {
            src: self.path_of(&row, to.other()),
            dst: self.path_of(&row, to),
            rel: row.rel.clone(),
        };
        if to_meta.is_some() {
            self.mode = Mode::Confirm(Confirm {
                prompt: format!(
                    "Overwrite '{}' on the {} side with the {} version? (y/N)",
                    engine::rel_string(&row.rel),
                    to.name(),
                    to.other().name()
                ),
                action,
            });
        } else {
            self.perform(action);
        }
    }

    /// `R`: make the focused side use the other side's path for a renamed/moved file.
    fn apply_rename(&mut self) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        if row.rename.is_none() {
            self.msg = "not a renamed or moved file".to_string();
            return;
        }
        if self.deny(self.focus) {
            return;
        }
        let (from, to) = match self.focus {
            Side::Left => (self.left.join(&row.rel), self.left.join(&row.rrel)),
            Side::Right => (self.right.join(&row.rrel), self.right.join(&row.rel)),
        };
        let done = format!(
            "renamed {} -> {} on the {} side",
            engine::rel_string(match self.focus {
                Side::Left => &row.rel,
                Side::Right => &row.rrel,
            }),
            engine::rel_string(match self.focus {
                Side::Left => &row.rrel,
                Side::Right => &row.rel,
            }),
            self.focus.name()
        );
        let m = match self.journal.run(Recipe::rename(from, to)) {
            Ok(()) => format!("{done}  (u to undo)"),
            Err(e) => format!("error: {e}"),
        };
        self.refresh();
        self.msg = m;
    }

    fn start_delete(&mut self) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        if self.deny(self.focus) {
            return;
        }
        let meta = match self.focus {
            Side::Left => &row.left,
            Side::Right => &row.right,
        };
        if meta.is_none() {
            self.msg = format!("'{}' is missing on the {} side", row.name, self.focus.name());
            return;
        }
        let path = self.path_of(&row, self.focus);
        self.mode = Mode::Confirm(Confirm {
            prompt: format!("Delete {}? This cannot be undone. (y/N)", path.display()),
            action: Action::Delete { path },
        });
    }

    fn perform(&mut self, action: Action) {
        let (recipe, done) = match action {
            Action::Copy { src, dst, rel } => {
                let done = format!("copied {}", engine::rel_string(&rel));
                let r = Recipe::copy(
                    src,
                    dst,
                    rel,
                    self.opts.follow_symlinks,
                    self.opts.ignore.clone(),
                );
                (r, done)
            }
            Action::Delete { path } => {
                let done = format!("deleted {}", path.display());
                (Recipe::delete(path), done)
            }
        };
        let m = match self.journal.run(recipe) {
            Ok(()) => format!("{done}  (u to undo)"),
            Err(e) => format!("error: {e}"),
        };
        self.refresh();
        self.msg = m;
    }

    fn undo_redo(&mut self, undo: bool) {
        let res = if undo { self.journal.undo() } else { self.journal.redo() };
        let m = match res {
            Ok(Some(m)) => m,
            Ok(None) => format!("nothing to {}", if undo { "undo" } else { "redo" }),
            Err(e) => format!("error: {e}"),
        };
        self.refresh();
        self.msg = m;
    }

    fn yank(&mut self) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        let meta = match self.focus {
            Side::Left => &row.left,
            Side::Right => &row.right,
        };
        let side = if meta.is_some() { self.focus } else { self.focus.other() };
        let path = self.path_of(&row, side);
        let seq = format!("\x1b]52;c;{}\x07", engine::base64(path.to_string_lossy().as_bytes()));
        let mut out = io::stdout();
        let _ = out.write_all(seq.as_bytes());
        let _ = out.flush();
        self.msg = format!("yanked {} (needs OSC 52 terminal support)", path.display());
    }

    /// Defaults + global file + `--ignore` + the patterns saved for this pair.
    fn effective_ignore(&self) -> Ignore {
        let mut ig = self.ignore_all.clone();
        for l in &self.pair_lines {
            ig.add(l);
        }
        ig
    }

    fn apply_ignore(&mut self) {
        self.opts.ignore = if self.ignore_on { self.effective_ignore() } else { Ignore::new() };
    }

    /// Ok(true) = written to the pair file, Ok(false) = no place to keep it.
    fn save_pair_lines(&self) -> io::Result<bool> {
        match &self.pair_file {
            Some(p) => {
                let header = format!(
                    "dirrcmp ignore patterns for {} <-> {}",
                    self.left.display(),
                    self.right.display()
                );
                cli::write_pair_ignore(p, &self.pair_lines, &header).map(|_| true)
            }
            None => Ok(false),
        }
    }

    fn after_ignore_change(&mut self) -> Option<String> {
        let saved = self.save_pair_lines();
        self.apply_ignore();
        self.refresh();
        saved.err().map(|e| format!("could not save: {e}"))
    }

    fn toggle_ignore(&mut self) {
        self.ignore_on = !self.ignore_on;
        self.apply_ignore();
        self.refresh();
        self.msg = format!("ignore patterns {}", if self.ignore_on { "on" } else { "off" });
    }

    fn ignore_selected(&mut self) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        let pat = format!("/{}{}", engine::rel_string(&row.rel), if row.is_dir { "/" } else { "" });
        if !self.pair_lines.contains(&pat) {
            self.pair_lines.push(pat.clone());
        }
        self.ignore_on = true;
        let saved = self.save_pair_lines();
        self.apply_ignore();
        self.refresh();
        self.msg = match saved {
            Ok(true) => format!("ignoring {pat} (saved for this pair; I to manage)"),
            Ok(false) => format!("ignoring {pat} (this session)"),
            Err(e) => format!("ignoring {pat} (could not save: {e})"),
        };
    }

    /// Keys of the ignore-pattern manager. `self.mode` is Normal while this runs;
    /// the manager stays open by putting `ui` back.
    fn ignores_key(&mut self, mut ui: IgnoreUi, key: KeyEvent) {
        if let Some(mut text) = ui.adding.take() {
            match key.code {
                KeyCode::Enter => {
                    let t = text.trim().to_string();
                    if !t.is_empty() && !self.pair_lines.contains(&t) {
                        self.pair_lines.push(t);
                        ui.sel = self.pair_lines.len() - 1;
                        self.msg = self.after_ignore_change().unwrap_or_else(|| "pattern added".to_string());
                    }
                }
                KeyCode::Esc => {}
                KeyCode::Backspace => {
                    text.pop();
                    ui.adding = Some(text);
                }
                KeyCode::Char(c) => {
                    text.push(c);
                    ui.adding = Some(text);
                }
                _ => ui.adding = Some(text),
            }
            self.mode = Mode::Ignores(ui);
            return;
        }
        let n = self.pair_lines.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('I') => return,
            KeyCode::Char('j') | KeyCode::Down => ui.sel = (ui.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => ui.sel = ui.sel.saturating_sub(1),
            KeyCode::Char('a') => ui.adding = Some(String::new()),
            KeyCode::Char('d') | KeyCode::Char('x') => {
                if ui.sel < n {
                    let removed = self.pair_lines.remove(ui.sel);
                    ui.sel = ui.sel.min(self.pair_lines.len().saturating_sub(1));
                    self.msg = self
                        .after_ignore_change()
                        .unwrap_or_else(|| format!("removed {removed}"));
                }
            }
            _ => {}
        }
        self.mode = Mode::Ignores(ui);
    }

    // ── preferences ──

    fn pref_label(&self, row: usize) -> (String, String) {
        let mark = |b: bool| if b { "[x]" } else { "[ ]" };
        match row {
            0 => (
                format!("{} Compare file contents", mark(self.prefs.compare_contents)),
                if self.cli_quick { "--quick is on this run".into() } else { "off: trust size + date, read nothing".into() },
            ),
            1 => (
                format!("{} Compare modification dates", mark(self.prefs.compare_dates)),
                if self.cli_dates { "--dates is on this run".into() } else { String::new() },
            ),
            2 => (format!("{} Directories first", mark(self.prefs.dirs_first)), String::new()),
            3 => (format!("{} Show key hints", mark(self.prefs.show_hints)), String::new()),
            4 => (format!("{} Nerd Font icons", mark(self.prefs.nerd_font)), String::new()),
            5 => (
                format!("{} Syntax highlighting", mark(self.prefs.syntax)),
                if self.cli_no_syntax { "--no-syntax is on this run".into() } else { String::new() },
            ),
            6 => (format!("{} Chunk background colours", mark(self.prefs.chunk_bg)), String::new()),
            7 => (format!("{} Smooth scrolling", mark(self.prefs.smooth)), String::new()),
            8 => (
                format!("{} Detect renamed / moved files", mark(self.prefs.renames)),
                if self.cli_no_renames { "--no-renames is on this run".into() } else { "like git -M: same or ≥50% similar content".into() },
            ),
            _ => (
                format!(
                    "    External diff command: {}",
                    self.prefs.diff_command.as_deref().unwrap_or("(none)")
                ),
                if self.cli_diff.is_some() { "overridden on this run".into() } else { "Enter edits".into() },
            ),
        }
    }

    /// Re-derive everything that depends on the preferences and rescan.
    fn apply_prefs(&mut self) {
        self.dirs_first = self.prefs.dirs_first;
        self.show_hints = self.prefs.show_hints;
        self.nerd_font = self.prefs.nerd_font;
        self.syntax = self.prefs.syntax && !self.cli_no_syntax;
        self.chunk_bg = self.prefs.chunk_bg;
        self.smooth = self.prefs.smooth;
        if !self.smooth {
            self.flush_scroll();
        }
        self.diff_cmd = self.cli_diff.clone().or_else(|| self.prefs.diff_command.clone());
        self.opts.compare_dates = self.cli_dates || self.prefs.compare_dates;
        self.opts.compare_contents = self.prefs.compare_contents && !self.cli_quick;
        self.opts.detect_renames = self.prefs.renames && !self.cli_no_renames;
        self.refresh();
    }

    fn prefs_changed(&mut self, row: usize) {
        self.apply_prefs();
        let over = match row {
            0 if self.cli_quick => Some("--quick"),
            1 if self.cli_dates => Some("--dates"),
            8 if self.cli_no_renames => Some("--no-renames"),
            9 if self.cli_diff.is_some() => Some("--diff-cmd"),
            _ => None,
        };
        let saved = match &self.config_file {
            Some(p) => config::save_to(p, &self.prefs).map(|_| p.display().to_string()),
            None => Err("no config directory".to_string()),
        };
        self.msg = match (saved, over) {
            (Ok(p), None) => format!("saved to {p}"),
            (Ok(p), Some(o)) => format!("saved to {p} ({o} still applies on this run)"),
            (Err(e), _) => format!("not saved: {e}"),
        };
    }

    fn prefs_key(&mut self, mut ui: PrefsUi, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(mut text) = ui.editing.take() {
            match key.code {
                KeyCode::Enter => {
                    let t = text.trim().to_string();
                    self.prefs.diff_command = if t.is_empty() { None } else { Some(t) };
                    self.prefs_changed(9);
                }
                KeyCode::Esc => {}
                KeyCode::Backspace => {
                    text.pop();
                    ui.editing = Some(text);
                }
                KeyCode::Char(c) if !ctrl => {
                    text.push(c);
                    ui.editing = Some(text);
                }
                _ => ui.editing = Some(text),
            }
            self.mode = Mode::Prefs(ui);
            return;
        }
        if ctrl {
            self.mode = Mode::Prefs(ui);
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char(',') => return,
            KeyCode::Char('j') | KeyCode::Down => ui.sel = (ui.sel + 1).min(PREF_ROWS - 1),
            KeyCode::Char('k') | KeyCode::Up => ui.sel = ui.sel.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('l') | KeyCode::Char('h')
            | KeyCode::Left | KeyCode::Right => {
                match ui.sel {
                    0 => self.prefs.compare_contents = !self.prefs.compare_contents,
                    1 => self.prefs.compare_dates = !self.prefs.compare_dates,
                    2 => self.prefs.dirs_first = !self.prefs.dirs_first,
                    3 => self.prefs.show_hints = !self.prefs.show_hints,
                    4 => self.prefs.nerd_font = !self.prefs.nerd_font,
                    5 => self.prefs.syntax = !self.prefs.syntax,
                    6 => self.prefs.chunk_bg = !self.prefs.chunk_bg,
                    7 => self.prefs.smooth = !self.prefs.smooth,
                    8 => self.prefs.renames = !self.prefs.renames,
                    _ => {
                        if key.code == KeyCode::Enter {
                            ui.editing = Some(self.prefs.diff_command.clone().unwrap_or_default());
                        }
                    }
                }
                if ui.sel < 9 {
                    self.prefs_changed(ui.sel);
                }
            }
            _ => {}
        }
        self.mode = Mode::Prefs(ui);
    }

    // ── key binding editor ──

    fn save_keymap(&mut self, notes: Vec<String>) {
        let res = match &self.keys_file {
            Some(p) => config::save_string_map(p, &self.keymap.entries()),
            None => Err("no config directory".to_string()),
        };
        self.msg = match res {
            Ok(()) if notes.is_empty() => "saved".to_string(),
            Ok(()) => format!("saved; {}", notes.join(", ")),
            Err(e) => format!("not saved: {e}"),
        };
    }

    fn keys_key(&mut self, mut ui: KeysUi, key: KeyEvent) {
        if let Some(add) = ui.capture.take() {
            if key.code != KeyCode::Esc {
                match chord_of(&key) {
                    Some(c) if c == (Chord { key: Key::Char('c'), ctrl: true }) => {
                        self.msg = "ctrl+c always quits and can't be rebound".to_string();
                    }
                    Some(c) => {
                        let mut chords = if add { self.keymap.chords_for(ui.sel) } else { Vec::new() };
                        if !chords.contains(&c) {
                            chords.push(c);
                        }
                        let notes = self.keymap.set(ui.sel, chords);
                        self.save_keymap(notes);
                    }
                    None => self.msg = "that key can't be bound".to_string(),
                }
            }
            self.mode = Mode::Keys(ui);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            self.mode = Mode::Keys(ui);
            return;
        }
        let n = keys::ACTIONS.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('K') => return,
            KeyCode::Char('j') | KeyCode::Down => ui.sel = (ui.sel + 1).min(n - 1),
            KeyCode::Char('k') | KeyCode::Up => ui.sel = ui.sel.saturating_sub(1),
            KeyCode::PageDown => ui.sel = (ui.sel + 10).min(n - 1),
            KeyCode::PageUp => ui.sel = ui.sel.saturating_sub(10),
            KeyCode::Enter => {
                ui.capture = Some(false);
                self.msg = "press the new key (Esc cancels)".to_string();
            }
            KeyCode::Char('a') => {
                ui.capture = Some(true);
                self.msg = "press the key to add (Esc cancels)".to_string();
            }
            KeyCode::Char('x') | KeyCode::Backspace => {
                self.keymap.reset(ui.sel);
                self.save_keymap(Vec::new());
            }
            KeyCode::Char('R') => {
                self.keymap.reset_all();
                self.save_keymap(Vec::new());
            }
            _ => {}
        }
        self.mode = Mode::Keys(ui);
    }

    // ── actions menu ──

    fn action_items(&self) -> Vec<(usize, String)> {
        let mut v: Vec<(&str, String)> = Vec::new();
        if let Some(row) = self.rows.get(self.cursor) {
            if row.is_dir {
                v.push(("open", "Expand / collapse directory".to_string()));
            } else {
                v.push(("open", "Open side-by-side editor".to_string()));
                v.push(("unified_diff", "Read-only unified diff".to_string()));
                v.push(("edit", "Open in $EDITOR".to_string()));
            }
            if let Some(p) = row.rename {
                v.push((
                    "rename_apply",
                    format!("Rename on the {} side to match the other ({p}% similar)", self.focus.name()),
                ));
            }
            if row.status != Status::Same && row.left.is_some() {
                v.push(("copy_right", "Copy to right".to_string()));
            }
            if row.status != Status::Same && row.right.is_some() {
                v.push(("copy_left", "Copy to left".to_string()));
            }
            let here = match self.focus {
                Side::Left => row.left.is_some(),
                Side::Right => row.right.is_some(),
            };
            if here {
                v.push(("delete", format!("Delete on the {} side", self.focus.name())));
            }
            v.push(("yank", "Yank path".to_string()));
            v.push(("ignore_entry", "Ignore this entry".to_string()));
            if row.paired {
                v.push(("unpair", "Remove pairing".to_string()));
            } else if self.mark.is_some() {
                v.push(("pair", "Pair with the marked entry".to_string()));
            } else if row.left.is_some() != row.right.is_some() {
                v.push(("pair", "Mark for pairing".to_string()));
            }
        }
        for (a, l) in [
            ("undo", "Undo"),
            ("redo", "Redo"),
            ("refresh", "Refresh"),
            ("filter", "Filter by name"),
            ("status_filter", "Cycle status filter"),
            ("sort", "Cycle sort"),
            ("swap", "Swap panels"),
            ("toggle_ignore", "Toggle ignore patterns"),
            ("manage_ignores", "Manage ignore patterns"),
            ("preferences", "Preferences"),
            ("keybindings", "Key bindings"),
            ("help", "Help"),
            ("quit", "Quit"),
        ] {
            v.push((a, l.to_string()));
        }
        v.into_iter().filter_map(|(a, l)| keys::action_index(a).map(|i| (i, l))).collect()
    }

    fn open_actions(&mut self) {
        let items = self.action_items();
        self.mode = Mode::Actions(ActionsUi { items, sel: 0 });
    }

    fn actions_key(&mut self, mut ui: ActionsUi, key: KeyEvent, term: &mut dyn Host) -> io::Result<bool> {
        let n = ui.items.len();
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            self.mode = Mode::Actions(ui);
            return Ok(false);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('.') => return Ok(false),
            KeyCode::Char('j') | KeyCode::Down => ui.sel = (ui.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => ui.sel = ui.sel.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char('l') => {
                // The menu is closed (mode is Normal); run the action's own keys.
                return match ui.items.get(ui.sel) {
                    Some(&(a, _)) => self.feed(&keys::canon(a), term),
                    None => Ok(false),
                };
            }
            _ => {}
        }
        self.mode = Mode::Actions(ui);
        Ok(false)
    }

    // ── pairing renamed directories ──

    /// `m`: mark a directory or file that exists on one side only, then press `m`
    /// on the differently named counterpart (same kind) on the other side to
    /// compare them as one.
    fn mark_pair(&mut self) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        if self.mark.as_ref().map_or(false, |m| m.rel == row.rel) {
            self.mark = None;
            self.msg = "mark cancelled".to_string();
            return;
        }
        if !matches!(row.status, Status::LeftOnly | Status::RightOnly) {
            self.msg = "pairing needs a file or directory that exists on one side only".to_string();
            return;
        }
        let Some(first) = self.mark.take() else {
            self.msg = format!(
                "marked '{}': press m on its counterpart on the other side (Esc cancels)",
                row.name
            );
            self.mark = Some(Mark {
                rel: row.rel.clone(),
                rrel: row.rrel.clone(),
                status: row.status,
                is_dir: row.is_dir,
            });
            return;
        };
        if first.status == row.status || first.is_dir != row.is_dir {
            let want = if first.status == Status::LeftOnly { "right" } else { "left" };
            let kind = if first.is_dir { "directory" } else { "file" };
            self.msg = format!("pick a {kind} that exists only on the {want} side");
            self.mark = Some(first);
            return;
        }
        if first.rel.parent() != row.rel.parent() {
            self.msg = "both entries must have the same parent directory".to_string();
            self.mark = Some(first);
            return;
        }
        let (l, r) = if first.status == Status::LeftOnly {
            (first.rel, row.rrel.clone())
        } else {
            (row.rel.clone(), first.rrel)
        };
        let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.msg = format!("paired '{}' with '{}' (M to unpair)", name(&l), name(&r));
        self.opts.pairs.retain(|p| p.0 != l && p.1 != r);
        self.opts.pairs.push((l, r));
        self.refresh();
    }

    /// `M`: drop the pairing of the selected entry (or cancel a pending mark).
    fn unpair(&mut self) {
        if self.mark.take().is_some() {
            self.msg = "mark cancelled".to_string();
            return;
        }
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        let before = self.opts.pairs.len();
        if row.paired {
            self.opts.pairs.retain(|p| !(p.0 == row.rel && p.1 == row.rrel));
        }
        if self.opts.pairs.len() == before {
            self.msg = "not a manually paired entry".to_string();
            return;
        }
        self.refresh();
        self.msg = "unpaired".to_string();
    }

    // ── drawing ──

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(area);
        self.view_h = (chunks[0].height.saturating_sub(2) as usize).max(1);
        self.term_w = area.width;
        self.adjust_top();

        // The editor, with the file sidebar on its left when there is room.
        let in_editor = matches!(self.mode, Mode::Side(_))
            || matches!(&self.mode, Mode::Picker(p) if p.back.is_some());
        let (sb_rect, main) = if in_editor && self.sb_active() {
            let w = (chunks[0].width / 4).clamp(22, 36);
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(w), Constraint::Min(10)])
                .split(chunks[0]);
            (Some(cols[0]), cols[1])
        } else {
            (None, chunks[0])
        };
        if let Some(r) = sb_rect {
            self.sb_top = sidebar_top(self.sb_top, self.cursor, r.height.saturating_sub(2) as usize);
            draw_sidebar(
                f,
                r,
                &self.rows,
                self.cursor,
                self.sb_top,
                self.side_rel.as_ref(),
                self.sb_focus,
                self.nerd_font,
            );
        }

        if let Mode::Side(sv) = &mut self.mode {
            sv.draw(f, main);
        } else if let Mode::Picker(ui) = &mut self.mode {
            if let Some(sv) = ui.back.as_mut() {
                sv.draw(f, main);
            } else {
                draw_panels(f, chunks[0], self);
            }
        } else if let Mode::Diff(dv) = &self.mode {
            draw_diff(f, chunks[0], dv);
        } else {
            draw_panels(f, chunks[0], self);
        }
        self.draw_status(f, chunks[1]);

        match &self.mode {
            Mode::Confirm(c) => {
                let r = centered(area, 64, 5);
                f.render_widget(Clear, r);
                f.render_widget(
                    Paragraph::new(c.prompt.clone())
                        .wrap(Wrap { trim: true })
                        .block(Block::default().borders(Borders::ALL).title(" Confirm ")),
                    r,
                );
            }
            Mode::Ignores(ui) => {
                draw_ignores(f, area, ui, &self.ignore_all.lines(), &self.pair_lines, &self.msg)
            }
            Mode::Prefs(ui) => draw_prefs(f, area, ui, self),
            Mode::Picker(ui) => draw_picker(f, area, ui),
            Mode::Keys(ui) => draw_keys(f, area, ui, &self.keymap),
            Mode::Actions(ui) => draw_actions(f, area, ui, &self.keymap),
            Mode::Help => {
                let lines: Vec<Line> = HELP.lines().map(|l| Line::from(l.to_string())).collect();
                let r = centered(area, 84, lines.len() as u16 + 2);
                f.render_widget(Clear, r);
                f.render_widget(
                    Paragraph::new(lines)
                        .block(Block::default().borders(Borders::ALL).title(" Keys (any key closes) ")),
                    r,
                );
            }
            _ => {}
        }
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let info = format!(
            " sort:{} filter:{} ignore:{} ",
            self.sort.label(),
            self.status_filter.label(),
            if self.ignore_on { "on" } else { "off" }
        );
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(info.chars().count() as u16)])
            .split(area);
        let (text, style) = match &self.mode {
            Mode::Side(sv) if self.sb_active() && self.sb_focus => (
                " sidebar: j/k move  l/Enter open  h fold  Tab/Esc back to editor  t hide  ^p find file".to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ),
            Mode::Side(sv) => (sv.status(), Style::default().add_modifier(Modifier::REVERSED)),
            Mode::Picker(_) => (
                " type to filter  ^n/^p or arrows move  Enter open  Tab changed only  Esc close".to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ),
            Mode::Filter(s) => (format!("/{s}█"), Style::default().add_modifier(Modifier::REVERSED)),
            Mode::Ignores(_) => (
                " j/k select  a add  d delete  q close".to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ),
            Mode::Prefs(ui) => (
                if ui.editing.is_some() {
                    " type the command  Enter save  Esc cancel".to_string()
                } else if !self.msg.is_empty() {
                    format!(" {}", self.msg)
                } else {
                    " j/k select  Space/Enter toggle (Enter edits the command)  q close".to_string()
                },
                Style::default().add_modifier(Modifier::REVERSED),
            ),
            Mode::Keys(_) => (
                if !self.msg.is_empty() {
                    format!(" {}", self.msg)
                } else {
                    " j/k select  Enter rebind  a add key  x reset  R reset all  q close".to_string()
                },
                Style::default().add_modifier(Modifier::REVERSED),
            ),
            Mode::Actions(_) => (
                " j/k select  Enter run  q close".to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ),
            Mode::Diff(_) => (
                " j/k scroll  n/N next/prev hunk  g/G top/bottom  q back".to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ),
            _ if !self.msg.is_empty() => (
                format!(" {}", self.msg),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::REVERSED),
            ),
            Mode::Normal if self.rows.get(self.cursor).map_or(false, |r| r.rename.is_some()) => {
                let r = &self.rows[self.cursor];
                let p = r.rename.unwrap_or(0);
                (
                    format!(
                        " renamed: {} -> {}{}   R apply on the {} side",
                        engine::rel_string(&r.rel),
                        engine::rel_string(&r.rrel),
                        if p >= 100 { String::new() } else { format!(" ({p}% similar)") },
                        self.focus.name()
                    ),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::REVERSED),
                )
            }
            _ => (
                if self.show_hints {
                    " j/k move  l/h expand  Enter side-by-side  D unified  >/< copy  d delete  / filter  . actions  ? help  q quit"
                        .to_string()
                } else {
                    String::new()
                },
                Style::default().add_modifier(Modifier::REVERSED),
            ),
        };
        f.render_widget(Paragraph::new(text).style(style), cols[0]);
        f.render_widget(
            Paragraph::new(info).style(Style::default().add_modifier(Modifier::REVERSED)),
            cols[1],
        );
    }
}

const HELP: &str = "\
 Navigation                         Tree
  j k / arrows    move               l / Right   expand dir, or open file
  gg  G           first / last       h / Left    collapse dir, or go to parent
  ^d ^u ^f ^b     half / full page   Enter       side-by-side editor (dirs: toggle)
  ^e ^y           scroll view        zR  zM      expand / collapse all (zO zC: here)
  Tab %  H  L     switch panel       ]c  [c      next / previous difference

 Actions                            Filtering and ignoring
  >  <            copy right / left  /    filter by name (Enter keep, Esc clear)
  Space           copy to other side f    cycle status filter
  d               delete (focused)   i    ignore entry (saved for this pair)
  u  U            undo / redo        I    manage ignore patterns
  y               yank path (OSC 52) zi   toggle ignore patterns
  e               $VISUAL / $EDITOR  s    cycle sort: name/size/date/status
  m  M            pair / unpair dirs S    swap panels
  r               refresh            ?    this help         q  quit
  ^p              find file (fuzzy picker; Tab: changed only)
  .               actions menu       ,    preferences       K  key bindings

 Colors: yellow modified, green only on this side, red missing here, dim identical,
 cyan renamed/moved (same or similar content): R renames the focused side to match.
 Directory copy merges; nothing extra is deleted from the destination.
 Pairing: press m on a file or directory that exists on one side only, Tab to the
 other panel, press m on the renamed one (same kind, same parent). They compare as one entry.
 D  read-only unified diff (j k scroll, n N next/prev hunk, q back).
 Enter on a file opens the side-by-side editor: press ? inside it.";

fn chord_of(key: &KeyEvent) -> Option<Chord> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let k = match key.code {
        KeyCode::Char(c) => Key::Char(if ctrl { c.to_ascii_lowercase() } else { c }),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Tab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        _ => return None,
    };
    // ctrl only exists for characters
    let ctrl = ctrl && matches!(k, Key::Char(_));
    Some(Chord { key: k, ctrl })
}

fn event_of(c: Chord) -> KeyEvent {
    let code = match c.key {
        Key::Char(ch) => KeyCode::Char(ch),
        Key::Enter => KeyCode::Enter,
        Key::Esc => KeyCode::Esc,
        Key::Tab => KeyCode::Tab,
        Key::Backspace => KeyCode::Backspace,
        Key::Up => KeyCode::Up,
        Key::Down => KeyCode::Down,
        Key::Left => KeyCode::Left,
        Key::Right => KeyCode::Right,
        Key::Home => KeyCode::Home,
        Key::End => KeyCode::End,
        Key::PageUp => KeyCode::PageUp,
        Key::PageDown => KeyCode::PageDown,
    };
    KeyEvent::new(code, if c.ctrl { KeyModifiers::CONTROL } else { KeyModifiers::NONE })
}

fn sel_style(selected: bool) -> Style {
    if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    }
}

/// First visible index so that `sel` stays inside a window of `h` lines.
fn window_start(sel: usize, h: usize) -> usize {
    if h == 0 || sel < h {
        0
    } else {
        sel + 1 - h
    }
}

fn draw_prefs(f: &mut Frame, area: Rect, ui: &PrefsUi, app: &App) {
    let mut lines: Vec<Line> = Vec::new();
    for row in 0..PREF_ROWS {
        let (label, note) = app.pref_label(row);
        let text = if note.is_empty() { format!(" {label}") } else { format!(" {label:<44} {note}") };
        lines.push(Line::from(Span::styled(text, sel_style(row == ui.sel && ui.editing.is_none()))));
    }
    if let Some(t) = &ui.editing {
        lines.push(Line::from(format!(" diff command: {t}\u{2588}")));
    }
    let r = centered(area, 78, lines.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Preferences ")),
        r,
    );
}

fn draw_keys(f: &mut Frame, area: Rect, ui: &KeysUi, km: &Keymap) {
    let h = (area.height.saturating_sub(4) as usize).clamp(3, 24);
    let start = window_start(ui.sel, h);
    let mut lines: Vec<Line> = Vec::new();
    for (i, a) in keys::ACTIONS.iter().enumerate().skip(start).take(h) {
        let mark = if km.is_overridden(i) { '*' } else { ' ' };
        let waiting = ui.capture.is_some() && i == ui.sel;
        let keys_txt = if waiting { "press a key...".to_string() } else { km.keys_label(i) };
        lines.push(Line::from(Span::styled(
            format!(" {mark}{:<32} {keys_txt}", a.desc),
            sel_style(i == ui.sel),
        )));
    }
    let title = format!(" Key bindings  (* = changed, {}/{}) ", ui.sel + 1, keys::ACTIONS.len());
    let r = centered(area, 68, lines.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)), r);
}

fn status_style(status: Status, present: bool) -> Style {
    match (present, status) {
        (false, _) => Style::default().fg(Color::Red),
        (true, Status::Same) => Style::default().fg(Color::Gray),
        (true, Status::Modified) => Style::default().fg(Color::Yellow),
        (true, _) => Style::default().fg(Color::Green),
    }
}

fn sidebar_top(top: usize, cursor: usize, h: usize) -> usize {
    let h = h.max(1);
    if cursor < top {
        cursor
    } else if cursor >= top + h {
        cursor + 1 - h
    } else {
        top
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_sidebar(
    f: &mut Frame,
    area: Rect,
    rows: &[Row],
    cursor: usize,
    top: usize,
    current: Option<&PathBuf>,
    focused: bool,
    nerd: bool,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" files ")
        .border_style(Style::default().fg(if focused { Color::Green } else { Color::DarkGray }));
    let inner = block.inner(area);
    let w = inner.width as usize;
    let end = (top + inner.height as usize).min(rows.len());
    let mut lines: Vec<Line> = Vec::new();
    for (idx, row) in rows.iter().enumerate().take(end).skip(top) {
        let is_cur = current.map_or(false, |c| c == &row.rel);
        let icon = match (row.is_dir, row.expanded, nerd) {
            (true, true, false) => "▾ ",
            (true, false, false) => "▸ ",
            (true, true, true) => "\u{f07c} ",
            (true, false, true) => "\u{f07b} ",
            (false, _, true) => "\u{f15b} ",
            (false, _, false) => if is_cur { "▶ " } else { "  " },
        };
        let mut name = format!("{}{}{}", "  ".repeat(row.depth.min(8)), icon, row.name);
        if row.is_dir {
            name.push('/');
        }
        let text = format!("{:<w$}", fit(&name, w), w = w);
        let mut style = status_style(row.status, true);
        if is_cur {
            style = style.add_modifier(Modifier::BOLD);
        }
        if focused && idx == cursor {
            style = style.add_modifier(Modifier::REVERSED);
        } else if !focused && idx == cursor {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        lines.push(Line::from(Span::styled(text, style)));
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_picker(f: &mut Frame, area: Rect, ui: &PickerUi) {
    let w = area.width.saturating_sub(4).min(96);
    let h_list = (area.height.saturating_sub(6) as usize).clamp(3, 22);
    let h = (h_list as u16 + 3).min(area.height);
    let r = centered(area, w, h);
    let inner_w = r.width.saturating_sub(2) as usize;
    let start = window_start(ui.sel, h_list);
    let mut lines: Vec<Line> = Vec::new();
    let tag = if ui.changed_only { "  [changed only]" } else { "" };
    lines.push(Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Cyan)),
        Span::raw(format!("{}█", ui.query)),
        Span::styled(tag.to_string(), Style::default().fg(Color::Yellow)),
    ]));
    if ui.shown.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matching files".to_string(),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    for (pos, (i, matched)) in ui.shown.iter().enumerate().skip(start).take(h_list) {
        let it = &ui.items[*i];
        let sel = pos == ui.sel;
        let (mark, col) = match it.status {
            Status::Modified => ('~', Color::Yellow),
            Status::LeftOnly => ('-', Color::Red),
            Status::RightOnly => ('+', Color::Green),
            Status::Same => (' ', Color::Reset),
        };
        let label: Vec<char> = it.label().chars().collect();
        // keep the end of long paths visible: that is where the file name is
        let skip = label.len().saturating_sub(inner_w.saturating_sub(4));
        let base = if sel { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default() };
        let mut spans = vec![Span::styled(
            format!(" {mark} "),
            if col == Color::Reset { base } else { base.fg(col) },
        )];
        let mut run = String::new();
        let mut run_m = false;
        for (ci, ch) in label.iter().enumerate().skip(skip) {
            let m = matched.binary_search(&ci).is_ok();
            if m != run_m && !run.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut run), picker_text(base, run_m)));
            }
            run_m = m;
            run.push(*ch);
        }
        if !run.is_empty() {
            spans.push(Span::styled(run, picker_text(base, run_m)));
        }
        lines.push(Line::from(spans));
    }
    let title = format!(" Find file  {}/{} ", ui.shown.len(), ui.items.len());
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)), r);
}

fn picker_text(base: Style, matched: bool) -> Style {
    if matched {
        base.fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        base
    }
}

fn draw_actions(f: &mut Frame, area: Rect, ui: &ActionsUi, km: &Keymap) {
    let h = (area.height.saturating_sub(4) as usize).clamp(3, 30);
    let start = window_start(ui.sel, h);
    let mut lines: Vec<Line> = Vec::new();
    for (i, (a, label)) in ui.items.iter().enumerate().skip(start).take(h) {
        lines.push(Line::from(Span::styled(
            format!(" {label:<36} {}", km.keys_label(*a)),
            sel_style(i == ui.sel),
        )));
    }
    let r = centered(area, 64, lines.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Actions ")),
        r,
    );
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn fit(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else if w == 0 {
        String::new()
    } else {
        let mut t: String = s.chars().take(w - 1).collect();
        t.push('…');
        t
    }
}

/// The name shown in a panel: a paired directory shows its own name on each side.
fn own_name(row: &Row, side: Side) -> String {
    let (meta, path) = match side {
        Side::Left => (&row.left, &row.rel),
        Side::Right => (&row.right, &row.rrel),
    };
    // a file moved to another directory shows where it came from on the left
    if row.rename.is_some() && side == Side::Left && row.rel.parent() != row.rrel.parent() {
        return engine::rel_string(&row.rel);
    }
    match path.file_name() {
        Some(n) if meta.is_some() => n.to_string_lossy().into_owned(),
        _ => row.name.clone(),
    }
}

fn side_line(
    row: &Row,
    side: Side,
    width: usize,
    selected: bool,
    marked: bool,
    nerd: bool,
) -> Line<'static> {
    let meta = match side {
        Side::Left => &row.left,
        Side::Right => &row.right,
    };
    let indent = "  ".repeat(row.depth);
    // Nerd Font glyphs: folder, open folder, file (each followed by one space)
    let icon = match (row.is_dir, row.expanded, nerd) {
        (true, true, false) => "▾ ",
        (true, false, false) => "▸ ",
        (false, _, false) => "  ",
        (true, true, true) => "\u{f07c} ",
        (true, false, true) => "\u{f07b} ",
        (false, _, true) => "\u{f15b} ",
    };
    let size_txt = match meta {
        Some(m) if !m.is_dir => engine::human_size(m.size),
        _ => String::new(),
    };
    let right_w = 8usize;
    let prefix_w = indent.chars().count() + 2;
    let mark_w = if marked { 4 } else { 0 };
    let name_w = width.saturating_sub(prefix_w + mark_w + right_w + 1);
    let mut name = own_name(row, side);
    if row.is_dir {
        name.push('/');
    }
    let name = fit(&name, name_w);
    let prefix = format!("{indent}{icon}");
    let rest = format!("{:<nw$} {:>rw$}", name, size_txt, nw = name_w, rw = right_w);

    let mut style = match (meta.is_some(), row.status) {
        (true, _) if row.rename.is_some() => Style::default().fg(Color::Cyan),
        (false, _) => Style::default().fg(Color::Red),
        (true, Status::Same) => Style::default().fg(Color::Gray),
        (true, Status::Modified) => Style::default().fg(Color::Yellow),
        (true, _) => Style::default().fg(Color::Green),
    };
    if selected {
        style = if meta.is_none() {
            // reversed red would be dark text on red; black on red stays readable
            Style::default().fg(Color::Black).bg(Color::Red)
        } else if row.status == Status::Same && row.rename.is_none() {
            Style::default().fg(Color::Black).bg(Color::Gray)
        } else {
            style.add_modifier(Modifier::REVERSED)
        };
    }
    if marked {
        let mut mark_style = Style::default().fg(Color::Magenta);
        if selected {
            mark_style = mark_style.add_modifier(Modifier::REVERSED);
        }
        return Line::from(vec![
            Span::styled(prefix, style),
            Span::styled("[m] ", mark_style),
            Span::styled(rest, style),
        ]);
    }
    Line::from(Span::styled(format!("{prefix}{rest}"), style))
}

fn draw_panels(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    for (i, side) in [Side::Left, Side::Right].into_iter().enumerate() {
        let root = match (&app.labels, side) {
            (Some(l), Side::Left) => l.0.clone(),
            (Some(l), Side::Right) => l.1.clone(),
            (None, Side::Left) => app.left.display().to_string(),
            (None, Side::Right) => app.right.display().to_string(),
        };
        let focused = app.focus == side;
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {root} "))
            .border_style(Style::default().fg(if focused { Color::Green } else { Color::DarkGray }));
        let inner = block.inner(cols[i]);
        let width = inner.width as usize;
        let lines: Vec<Line> = if app.rows.is_empty() {
            vec![Line::from("  nothing to show")]
        } else {
            let end = (app.top + inner.height as usize).min(app.rows.len());
            (app.top..end)
                .map(|idx| {
                    let row = &app.rows[idx];
                    let marked = app.mark.as_ref().map_or(false, |m| m.rel == row.rel);
                    side_line(row, side, width, idx == app.cursor, marked, app.nerd_font)
                })
                .collect()
        };
        f.render_widget(Paragraph::new(lines).block(block), cols[i]);
    }
}

fn draw_ignores(
    f: &mut Frame,
    area: Rect,
    ui: &IgnoreUi,
    base: &[String],
    pair: &[String],
    msg: &str,
) {
    const SHOW_BASE: usize = 6;
    const SHOW_PAIR: usize = 8;
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(" Global (defaults, ignore file, --ignore):".to_string()));
    for l in base.iter().take(SHOW_BASE) {
        lines.push(Line::from(Span::styled(
            format!("   {l}"),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    if base.len() > SHOW_BASE {
        lines.push(Line::from(format!("   ... and {} more", base.len() - SHOW_BASE)));
    }
    lines.push(Line::from(" This pair of directories (a add, d delete):".to_string()));
    if pair.is_empty() {
        lines.push(Line::from("   (none)".to_string()));
    }
    let start = if ui.sel >= SHOW_PAIR { ui.sel + 1 - SHOW_PAIR } else { 0 };
    for (i, l) in pair.iter().enumerate().skip(start).take(SHOW_PAIR) {
        let style = if i == ui.sel && ui.adding.is_none() {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(format!("   {l}"), style)));
    }
    if let Some(t) = &ui.adding {
        lines.push(Line::from(format!(" new pattern: {t}\u{2588}")));
    } else if !msg.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().fg(Color::Yellow),
        )));
    }
    let r = centered(area, 72, lines.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Ignore patterns ")),
        r,
    );
}

fn clean(s: &str) -> String {
    s.replace('\t', "    ")
}

/// Spans for `text` (tabs expanded) coloured by `kinds`, one span per run.
fn syntax_spans(text: &str, kinds: &[Kind]) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut cur_k = Kind::Plain;
    for (i, c) in text.chars().enumerate() {
        let k = kinds.get(i).copied().unwrap_or(Kind::Plain);
        if k != cur_k && !cur.is_empty() {
            out.push(Span::styled(std::mem::take(&mut cur), side::syntax_style(cur_k)));
        }
        cur_k = k;
        if c == '\t' {
            cur.push_str("    ");
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(Span::styled(cur, side::syntax_style(cur_k)));
    }
    out
}

fn diff_line(l: &DiffLine, syn: Option<&UniSyntax>) -> Line<'static> {
    if let Some(kinds) = syn.and_then(|s| s.kinds(l)) {
        let sign = match l.kind {
            LineKind::Add => Some(("+ ", Style::default().fg(Color::Green))),
            LineKind::Del => Some(("- ", Style::default().fg(Color::Red))),
            LineKind::Context => Some(("  ", Style::default())),
            LineKind::Hunk => None,
        };
        if let Some((s, sty)) = sign {
            let dim = Style::default().add_modifier(Modifier::DIM);
            let num = |o: Option<usize>| match o {
                Some(v) => format!("{v:>5}"),
                None => "     ".to_string(),
            };
            let mut spans = vec![
                Span::styled(format!("{} {} │ ", num(l.left_no), num(l.right_no)), dim),
                Span::styled(s.to_string(), sty),
            ];
            spans.extend(syntax_spans(&l.text, kinds));
            return Line::from(spans);
        }
    }
    let num = |o: Option<usize>| match o {
        Some(v) => format!("{v:>5}"),
        None => "     ".to_string(),
    };
    let dim = Style::default().add_modifier(Modifier::DIM);
    let gutter = format!("{} {} │ ", num(l.left_no), num(l.right_no));
    match l.kind {
        LineKind::Hunk => {
            Line::from(Span::styled(l.text.clone(), Style::default().fg(Color::Cyan)))
        }
        LineKind::Context => {
            Line::from(vec![Span::styled(gutter, dim), Span::raw(format!("  {}", clean(&l.text)))])
        }
        LineKind::Add => Line::from(vec![
            Span::styled(gutter, dim),
            Span::styled(format!("+ {}", clean(&l.text)), Style::default().fg(Color::Green)),
        ]),
        LineKind::Del => Line::from(vec![
            Span::styled(gutter, dim),
            Span::styled(format!("- {}", clean(&l.text)), Style::default().fg(Color::Red)),
        ]),
    }
}

/// `diff_line` with a background on added / removed lines, padded to `width`.
fn tinted_line(l: &DiffLine, syn: Option<&UniSyntax>, width: usize) -> Line<'static> {
    let line = diff_line(l, syn);
    let color = match l.kind {
        LineKind::Add => side::chunk_bg_color(engine::RowKind::LeftOnly, true, false),
        LineKind::Del => side::chunk_bg_color(engine::RowKind::LeftOnly, false, false),
        _ => return line,
    };
    let bg = Style::default().bg(color);
    let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    let mut spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| {
            let st = s.style.patch(bg);
            Span::styled(s.content.into_owned(), st)
        })
        .collect();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), bg));
    }
    Line::from(spans)
}

fn draw_diff(f: &mut Frame, area: Rect, dv: &DiffView) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {}   +{} -{} ", dv.title, dv.added, dv.removed));
    let inner = block.inner(area);
    let mut lines: Vec<Line> = Vec::new();
    if let Some(n) = &dv.note {
        lines.push(Line::from(format!("  {n}")));
    }
    let start = dv.scroll.min(dv.lines.len());
    let end = (start + inner.height as usize).min(dv.lines.len());
    for l in &dv.lines[start..end] {
        lines.push(if dv.chunk_bg {
            tinted_line(l, dv.syntax.as_ref(), inner.width as usize)
        } else {
            diff_line(l, dv.syntax.as_ref())
        });
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
}

// ───────────────────────────── external programs ─────────────────────────────

/// Leave the alternate screen, run `cmd args... paths...`, then come back.
fn run_external(term: &mut DefaultTerminal, cmd: &str, paths: &[&Path]) -> io::Result<String> {
    let mut parts = cmd.split_whitespace();
    let Some(prog) = parts.next() else {
        return Ok("empty command".to_string());
    };
    let args: Vec<&str> = parts.collect();
    ratatui::restore();
    let status = Command::new(prog).args(&args).args(paths).status();
    *term = ratatui::init();
    term.clear()?;
    Ok(match status {
        Ok(s) if s.success() => format!("{prog} finished"),
        Ok(s) => format!("{prog} exited with {s}"),
        Err(e) => format!("could not run '{prog}': {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    /// Records external program calls instead of running them.
    #[derive(Default)]
    struct Rec {
        calls: Vec<(String, Vec<PathBuf>)>,
    }

    impl Host for Rec {
        fn run(&mut self, cmd: &str, paths: &[&Path]) -> io::Result<String> {
            self.calls.push((cmd.to_string(), paths.iter().map(|p| p.to_path_buf()).collect()));
            Ok("external done".to_string())
        }
    }

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir()
            .join(format!("dirrcmp-tui-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn put(p: PathBuf, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    fn build(root: &Path, diff_cmd: Option<&str>) -> App {
        build_with(root, diff_cmd, Keymap::default(), false)
    }

    fn build_with(root: &Path, diff_cmd: Option<&str>, keymap: Keymap, cli_dates: bool) -> App {
        App::new(cfg_for(root, diff_cmd, keymap, cli_dates))
    }

    fn cfg_for(root: &Path, diff_cmd: Option<&str>, keymap: Keymap, cli_dates: bool) -> Config {
        (Config {
            left: root.join("l"),
            right: root.join("r"),
            opts: Opts::default(),
            ignore_all: Ignore::defaults(),
            ignore_on: true,
            prefs: config::Config { renames: false, ..config::Config::default() },
            cli_dates,
            cli_quick: false,
            cli_diff: diff_cmd.map(String::from),
            cli_no_syntax: false,
            cli_no_renames: false,
            readonly: (false, false),
            labels: None,
            keymap,
            key_warnings: Vec::new(),
            config_file: Some(root.join("cfg/config.json")),
            keys_file: Some(root.join("cfg/keybindings.json")),
            pair_ignore: Some(root.join("data/pair.ignore")),
        })
    }

    fn key(app: &mut App, host: &mut Rec, code: KeyCode) -> bool {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE), host).unwrap()
    }

    fn keys(app: &mut App, s: &str) {
        let mut host = Rec::default();
        for c in s.chars() {
            key(app, &mut host, KeyCode::Char(c));
        }
    }

    fn code(app: &mut App, c: KeyCode) {
        key(app, &mut Rec::default(), c);
    }

    fn names(app: &App) -> Vec<String> {
        app.rows.iter().map(|r| r.name.clone()).collect()
    }

    fn select(app: &mut App, name: &str) {
        app.cursor = app
            .rows
            .iter()
            .position(|r| r.name == name)
            .unwrap_or_else(|| panic!("no row {name}: {:?}", names(app)));
    }

    fn basic() -> (PathBuf, App) {
        let root = tmp();
        let (l, r) = (root.join("l"), root.join("r"));
        put(l.join("a/x.txt"), "1\n");
        put(r.join("a/x.txt"), "2\n");
        put(l.join("a/same.txt"), "s");
        put(r.join("a/same.txt"), "s");
        put(l.join("gone/f"), "f");
        put(r.join("new.txt"), "n");
        put(l.join("node_modules/z"), "ignored");
        let app = build(&root, None);
        (root, app)
    }

    #[test]
    fn renamed_files_are_one_row_and_r_applies_the_rename() {
        let root = tmp();
        let (l, r) = (root.join("l"), root.join("r"));
        let body: String = (0..10).map(|i| format!("line {i} of the text\n")).collect();
        put(l.join("a/old.txt"), &body);
        put(r.join("b/new.txt"), &body);
        put(l.join("keep.txt"), "k");
        put(r.join("keep.txt"), "k");
        let mut cfg = cfg_for(&root, None, Keymap::default(), false);
        cfg.prefs.renames = true;
        let mut app = App::new(cfg);
        // one cyan row under the right-hand directory, none left over
        let row = app.rows.iter().find(|r| r.rename.is_some()).expect("rename row").clone();
        assert_eq!((row.name.as_str(), row.rename), ("new.txt", Some(100)));
        assert_eq!((row.rel.clone(), row.rrel.clone()), (PathBuf::from("a/old.txt"), PathBuf::from("b/new.txt")));
        assert_eq!(own_name(&row, Side::Left), "a/old.txt");
        assert_eq!(own_name(&row, Side::Right), "new.txt");
        assert!(!app.rows.iter().any(|r| r.name == "old.txt"));
        // the status line says what happened
        select(&mut app, "new.txt");
        app.msg.clear();
        // identical content: > / < refuse and point at R
        keys(&mut app, ">");
        assert!(app.msg.contains("R renames"), "{}", app.msg);
        // R on the (default) left panel moves the left file to the right-hand path
        app.focus = Side::Left;
        keys(&mut app, "R");
        assert!(app.msg.starts_with("renamed a/old.txt -> b/new.txt on the left side"), "{}", app.msg);
        assert!(l.join("b/new.txt").exists() && !l.join("a/old.txt").exists());
        assert!(app.rows.iter().all(|r| r.rename.is_none()));
        let st = app.rows.iter().find(|r| r.name == "new.txt").unwrap().status;
        assert_eq!(st, Status::Same);
        // undo brings the rename back
        keys(&mut app, "u");
        assert!(l.join("a/old.txt").exists() && !l.join("b/new.txt").exists());
        assert!(app.rows.iter().any(|r| r.rename.is_some()));
        // focus on the right moves the right file to the old path instead
        app.focus = Side::Right;
        select(&mut app, "new.txt");
        keys(&mut app, "R");
        assert!(r.join("a/old.txt").exists() && !r.join("b/new.txt").exists());
        keys(&mut app, "u");
        // R on an ordinary row does nothing
        select(&mut app, "keep.txt");
        keys(&mut app, "R");
        assert_eq!(app.msg, "not a renamed or moved file");
        // the action menu offers it on a renamed row
        select(&mut app, "new.txt");
        keys(&mut app, ".");
        assert!(action_names(&app).contains(&"rename_apply"));
        keys(&mut app, "q");
        // the picker shows the new path
        assert!(app.files().iter().any(|f| f.label() == "b/new.txt"));
        // switching detection off in the prefs splits it back into two one-sided files
        app.prefs.renames = false;
        app.apply_prefs();
        assert!(app.rows.iter().all(|r| r.rename.is_none()));
        assert!(app.rows.iter().any(|r| r.name == "a" && r.status == Status::LeftOnly));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn folding_here_and_shift_keys() {
        let root = tmp();
        let (l, r) = (root.join("l"), root.join("r"));
        put(l.join("a/b/x.txt"), "1\n");
        put(r.join("a/b/x.txt"), "2\n");
        put(l.join("a/y.txt"), "1\n");
        put(r.join("a/y.txt"), "2\n");
        let mut app = build(&root, None);
        assert_eq!(names(&app), vec!["a", "b", "x.txt", "y.txt"]);
        select(&mut app, "x.txt");
        keys(&mut app, "zc"); // closes the parent of a file: b
        assert_eq!(names(&app), vec!["a", "b", "y.txt"]);
        assert_eq!(app.rows[app.cursor].name, "b");
        keys(&mut app, "zo");
        assert_eq!(app.rows.len(), 4);
        select(&mut app, "x.txt");
        keys(&mut app, "zC"); // recursive close of b
        assert_eq!(names(&app), vec!["a", "b", "y.txt"]);
        select(&mut app, "a");
        keys(&mut app, "zC");
        assert_eq!(names(&app), vec!["a"]);
        keys(&mut app, "zO"); // recursive open
        assert_eq!(names(&app), vec!["a", "b", "x.txt", "y.txt"]);
        keys(&mut app, "za");
        assert_eq!(names(&app), vec!["a"]);
        // Shift+r reported as 'r' + SHIFT still means R
        keys(&mut app, "z");
        app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::SHIFT), &mut Rec::default()).unwrap();
        assert_eq!(app.rows.len(), 4);
        keys(&mut app, "z");
        app.on_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::SHIFT), &mut Rec::default()).unwrap();
        assert_eq!(names(&app), vec!["a"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn navigation_and_tree() {
        let (root, mut app) = basic();
        // modified directories start expanded, one-sided ones collapsed, ignored are hidden
        assert_eq!(names(&app), vec!["a", "same.txt", "x.txt", "gone", "new.txt"]);

        keys(&mut app, "j");
        assert_eq!(app.cursor, 1);
        keys(&mut app, "]c");
        assert_eq!(app.cursor, 2);
        keys(&mut app, "]c");
        assert_eq!(app.cursor, 4);
        keys(&mut app, "]c");
        assert_eq!((app.cursor, app.msg.as_str()), (4, "no more differences"));
        keys(&mut app, "[c");
        assert_eq!(app.cursor, 2);
        keys(&mut app, "G");
        assert_eq!(app.cursor, 4);
        keys(&mut app, "gg");
        assert_eq!(app.cursor, 0);

        keys(&mut app, "h"); // collapse a
        assert_eq!(names(&app), vec!["a", "gone", "new.txt"]);
        keys(&mut app, "l");
        assert_eq!(app.rows.len(), 5);
        select(&mut app, "gone");
        keys(&mut app, "l"); // expand a one-sided directory
        assert_eq!(app.rows.len(), 6);
        keys(&mut app, "j");
        keys(&mut app, "h"); // child -> parent
        assert_eq!(app.rows[app.cursor].name, "gone");
        keys(&mut app, "h");
        assert_eq!(app.rows.len(), 5);
        keys(&mut app, "zR");
        assert_eq!(app.rows.len(), 6);
        keys(&mut app, "zM");
        assert_eq!(names(&app), vec!["a", "gone", "new.txt"]);

        keys(&mut app, "?");
        assert!(matches!(app.mode, Mode::Help));
        keys(&mut app, "x");
        assert!(matches!(app.mode, Mode::Normal));
        assert!(key(&mut app, &mut Rec::default(), KeyCode::Char('q')));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn filtering_and_sorting() {
        let (root, mut app) = basic();
        keys(&mut app, "/");
        assert!(matches!(app.mode, Mode::Filter(_)));
        keys(&mut app, "x.t");
        assert_eq!(names(&app), vec!["a", "x.txt"]);
        code(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.rows.len(), 2);
        code(&mut app, KeyCode::Esc);
        assert_eq!(app.rows.len(), 5);

        keys(&mut app, "f"); // different only
        assert_eq!(names(&app), vec!["a", "x.txt", "gone", "new.txt"]);
        keys(&mut app, "ffff");
        assert!(app.status_filter == StatusFilter::All);

        keys(&mut app, "s");
        assert!(app.sort == SortKey::Size);
        keys(&mut app, "S"); // swap panels
        assert!(app.left.ends_with("r") && app.right.ends_with("l"));
        let _ = fs::remove_dir_all(&root);
    }

    fn text_of(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn unified_view_is_highlighted_from_the_whole_files() {
        let root = tmp();
        let (l, r) = (root.join("l"), root.join("r"));
        put(l.join("m.rs"), "/*\nfn old() {}\n*/\nlet a = \"x\";\n");
        put(r.join("m.rs"), "/*\nfn old() {}\n*/\n// y\n");
        put(l.join("n.txt"), "fn a() {}\n");
        put(r.join("n.txt"), "fn b() {}\n");
        let mut app = build(&root, None);
        select(&mut app, "m.rs");
        keys(&mut app, "D");
        let Mode::Diff(dv) = &app.mode else { panic!("expected the unified diff") };
        let syn = dv.syntax.as_ref().expect("highlighted");
        let del = dv.lines.iter().find(|x| x.kind == LineKind::Del).unwrap();
        let add = dv.lines.iter().find(|x| x.kind == LineKind::Add).unwrap();
        let ctx = dv.lines.iter().find(|x| x.kind == LineKind::Context && x.text.contains("fn old")).unwrap();
        // the string differs, and each side's line is looked up in its own file
        assert_eq!(syn.kinds(del).unwrap()[8], Kind::Str);
        assert_eq!(syn.kinds(add).unwrap()[0], Kind::Comment);
        assert_eq!(syn.kinds(del).unwrap()[0], Kind::Keyword);
        // context inside a multi-line comment is a comment although the line alone looks like code
        assert_eq!(syn.kinds(ctx).unwrap()[0], Kind::Comment);
        let hunk = dv.lines.iter().find(|x| x.kind == LineKind::Hunk).unwrap();
        assert!(syn.kinds(hunk).is_none() || diff_line(hunk, Some(syn)).spans.len() == 1);

        let line = diff_line(del, Some(syn));
        assert_eq!(text_of(&line), diff_line(del, None).spans.iter().map(|s| s.content.to_string()).collect::<String>());
        let kw = line.spans.iter().find(|s| s.content == "let").expect("keyword span");
        assert_eq!(kw.style.fg, Some(Color::Magenta));
        assert!(diff_line(add, Some(syn)).spans.iter().any(|s| s.style.fg == Some(Color::DarkGray)));
        let sign = line.spans.iter().find(|s| s.content == "- ").expect("sign span");
        assert_eq!(sign.style.fg, Some(Color::Red));
        let sign = diff_line(add, Some(syn)).spans.iter().find(|s| s.content == "+ ").unwrap().style.fg;
        assert_eq!(sign, Some(Color::Green));
        keys(&mut app, "q");

        // a file type without a language is shown as before
        select(&mut app, "n.txt");
        keys(&mut app, "D");
        assert!(matches!(&app.mode, Mode::Diff(dv) if dv.syntax.is_none()));
        keys(&mut app, "q");

        // switching it off in the preferences
        app.prefs.syntax = false;
        app.apply_prefs();
        select(&mut app, "m.rs");
        keys(&mut app, "D");
        assert!(matches!(&app.mode, Mode::Diff(dv) if dv.syntax.is_none()));
        keys(&mut app, "q");
        // and --no-syntax wins over the preference
        app.prefs.syntax = true;
        app.cli_no_syntax = true;
        app.apply_prefs();
        assert!(!app.syntax);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unified_view_tints_added_and_removed_lines() {
        let (root, mut app) = basic();
        select(&mut app, "x.txt");
        keys(&mut app, "D");
        let Mode::Diff(dv) = &app.mode else { panic!("unified diff") };
        assert!(dv.chunk_bg);
        let add = dv.lines.iter().find(|x| x.kind == LineKind::Add).unwrap();
        let del = dv.lines.iter().find(|x| x.kind == LineKind::Del).unwrap();
        let ctx = dv.lines.iter().find(|x| x.kind == LineKind::Context);
        let a = tinted_line(add, None, 40);
        let d = tinted_line(del, None, 40);
        let width = |l: &Line| l.spans.iter().map(|s| s.content.chars().count()).sum::<usize>();
        assert_eq!(width(&a), 40, "padded to the full width");
        assert!(a.spans.iter().all(|s| s.style.bg == Some(Color::Indexed(22))));
        assert!(d.spans.iter().all(|s| s.style.bg == Some(Color::Indexed(52))));
        if let Some(c) = ctx {
            assert!(tinted_line(c, None, 40).spans.iter().all(|s| s.style.bg.is_none()));
        }
        // text is unchanged apart from the padding
        assert_eq!(text_of(&a).trim_end(), text_of(&diff_line(add, None)).trim_end());
        keys(&mut app, "q");
        app.prefs.chunk_bg = false;
        app.apply_prefs();
        select(&mut app, "x.txt");
        keys(&mut app, "D");
        assert!(matches!(&app.mode, Mode::Diff(dv) if !dv.chunk_bg));
        let _ = fs::remove_dir_all(&root);
    }

    fn ctrl_key(app: &mut App, c: char) {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL), &mut Rec::default()).unwrap();
    }

    #[test]
    fn page_keys_scroll_smoothly_in_the_directory_view() {
        let root = tmp();
        for i in 0..60 {
            put(root.join(format!("l/f{i:02}")), "x");
        }
        fs::create_dir_all(root.join("r")).unwrap();
        let mut app = build(&root, None);
        app.view_h = 20;
        assert!(app.smooth);
        // the key only starts the animation
        ctrl_key(&mut app, 'd');
        assert_eq!(app.cursor, 0);
        assert!(app.scroll_active());
        assert_eq!(app.scroller.remaining(), 10);
        // more of the same adds up, the opposite direction cancels
        ctrl_key(&mut app, 'd');
        assert_eq!(app.scroller.remaining(), 20);
        ctrl_key(&mut app, 'u');
        assert_eq!(app.scroller.remaining(), -10);
        ctrl_key(&mut app, 'd');
        assert_eq!(app.scroller.remaining(), 10);
        // any other key finishes the animation first, then acts
        keys(&mut app, "j");
        assert!(!app.scroll_active());
        assert_eq!(app.cursor, 11);
        // time does the same without a key
        code(&mut app, KeyCode::PageDown);
        assert!(app.scroll_active());
        std::thread::sleep(Duration::from_millis(700));
        app.tick_scroll();
        assert_eq!(app.cursor, 31);
        assert!(!app.scroll_active());
        // clamped at the end, and no animation left behind
        for _ in 0..5 {
            ctrl_key(&mut app, 'f');
        }
        keys(&mut app, "k");
        assert_eq!(app.cursor, app.rows.len() - 2);
        // off: immediate
        app.prefs.smooth = false;
        app.apply_prefs();
        app.cursor = 0;
        ctrl_key(&mut app, 'd');
        assert_eq!(app.cursor, 10);
        assert!(!app.scroll_active());
        // turning it off mid-animation applies what was left
        app.prefs.smooth = true;
        app.apply_prefs();
        ctrl_key(&mut app, 'd');
        app.prefs.smooth = false;
        app.apply_prefs();
        assert_eq!(app.cursor, 20);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn page_keys_scroll_smoothly_in_the_unified_view() {
        let root = tmp();
        let a: String = (0..100).map(|i| format!("a{i}\n")).collect();
        let b: String = (0..100).map(|i| format!("b{i}\n")).collect();
        put(root.join("l/big.txt"), &a);
        put(root.join("r/big.txt"), &b);
        let mut app = build(&root, None);
        select(&mut app, "big.txt");
        keys(&mut app, "D");
        app.view_h = 20;
        ctrl_key(&mut app, 'd');
        let Mode::Diff(dv) = &app.mode else { panic!("unified diff") };
        assert_eq!(dv.scroll, 0);
        assert!(app.scroll_active());
        keys(&mut app, "j");
        let Mode::Diff(dv) = &app.mode else { panic!("unified diff") };
        assert_eq!(dv.scroll, 11);
        assert!(!dv.scroller.active());
        ctrl_key(&mut app, 'f');
        std::thread::sleep(Duration::from_millis(700));
        app.tick_scroll();
        let Mode::Diff(dv) = &app.mode else { panic!("unified diff") };
        assert_eq!(dv.scroll, 31);
        // leaving the view with an animation pending is harmless
        ctrl_key(&mut app, 'd');
        keys(&mut app, "q");
        assert!(matches!(app.mode, Mode::Normal) && !app.scroll_active());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cli_no_syntax_beats_the_preference() {
        let root = tmp();
        put(root.join("l/m.rs"), "fn a() {}\n");
        put(root.join("r/m.rs"), "fn b() {}\n");
        let mut cfg = cfg_for(&root, None, Keymap::default(), false);
        cfg.cli_no_syntax = true;
        assert!(cfg.prefs.syntax);
        assert!(!App::new(cfg).syntax);
        assert!(App::new(cfg_for(&root, None, Keymap::default(), false)).syntax);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn side_view_follows_the_syntax_setting() {
        let root = tmp();
        put(root.join("l/m.rs"), "fn a() {}\n");
        put(root.join("r/m.rs"), "fn b() {}\n");
        let mut app = build(&root, None);
        select(&mut app, "m.rs");
        code(&mut app, KeyCode::Enter);
        let Mode::Side(sv) = &app.mode else { panic!("side view") };
        assert!(sv.syntax_enabled());
        keys(&mut app, "q");
        app.prefs.syntax = false;
        app.apply_prefs();
        select(&mut app, "m.rs");
        code(&mut app, KeyCode::Enter);
        let Mode::Side(sv) = &app.mode else { panic!("side view") };
        assert!(!sv.syntax_enabled());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unified_and_side_by_side_views() {
        let (root, mut app) = basic();
        select(&mut app, "x.txt");
        keys(&mut app, "D");
        match &app.mode {
            Mode::Diff(dv) => assert_eq!((dv.added, dv.removed), (1, 1)),
            _ => panic!("expected the unified diff"),
        }
        keys(&mut app, "q");
        assert!(matches!(app.mode, Mode::Normal));

        // Enter opens the side-by-side editor; copy the hunk, save and close
        code(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Side(_)));
        keys(&mut app, ">"); // right gets the left version
        keys(&mut app, "q");
        assert!(matches!(app.mode, Mode::Side(_)), "unsaved edits: asks before closing");
        keys(&mut app, "s");
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(fs::read_to_string(root.join("r/a/x.txt")).unwrap(), "1\n");
        // the directory view refreshed after saving
        assert_eq!(app.rows.iter().find(|r| r.name == "a").unwrap().status, Status::Same);

        // a clean view closes straight away; a one-sided file is shown against nothing
        select(&mut app, "new.txt");
        code(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Side(_)));
        keys(&mut app, "q");
        assert!(matches!(app.mode, Mode::Normal));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_delete_and_undo() {
        let (root, mut app) = basic();
        select(&mut app, "new.txt");
        keys(&mut app, ">"); // nothing on the left to copy right
        assert!(app.msg.contains("missing on the left"), "{}", app.msg);
        assert!(!root.join("l/new.txt").exists());
        keys(&mut app, "<");
        assert_eq!(fs::read_to_string(root.join("l/new.txt")).unwrap(), "n");
        assert!(app.msg.contains("copied new.txt") && app.msg.contains("u to undo"), "{}", app.msg);
        keys(&mut app, "u");
        assert!(!root.join("l/new.txt").exists());
        keys(&mut app, "U");
        assert!(root.join("l/new.txt").exists());
        keys(&mut app, "<");
        assert_eq!(app.msg, "already identical");

        // overwriting asks first
        select(&mut app, "x.txt");
        keys(&mut app, ">");
        assert!(matches!(app.mode, Mode::Confirm(_)));
        keys(&mut app, "n");
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(fs::read_to_string(root.join("r/a/x.txt")).unwrap(), "2\n");
        keys(&mut app, ">");
        keys(&mut app, "y");
        assert_eq!(fs::read_to_string(root.join("r/a/x.txt")).unwrap(), "1\n");
        keys(&mut app, "u");
        assert_eq!(fs::read_to_string(root.join("r/a/x.txt")).unwrap(), "2\n");

        // Space copies the focused side across; Tab switches focus
        fs::write(root.join("l/a/same.txt"), "changed").unwrap();
        keys(&mut app, "r");
        select(&mut app, "same.txt");
        keys(&mut app, " ");
        keys(&mut app, "y");
        assert_eq!(fs::read_to_string(root.join("r/a/same.txt")).unwrap(), "changed");
        code(&mut app, KeyCode::Tab);
        assert!(app.focus == Side::Right);

        // delete acts on the focused side and refuses a missing one
        select(&mut app, "gone");
        keys(&mut app, "d");
        assert!(matches!(app.mode, Mode::Normal) && app.msg.contains("missing"));
        code(&mut app, KeyCode::Tab);
        keys(&mut app, "d");
        assert!(matches!(app.mode, Mode::Confirm(_)));
        keys(&mut app, "y");
        assert!(!root.join("l/gone").exists());
        assert!(!app.rows.iter().any(|r| r.name == "gone"));
        keys(&mut app, "u");
        assert!(root.join("l/gone/f").exists());
        assert!(app.rows.iter().any(|r| r.name == "gone"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn external_diff_command_gets_both_paths() {
        let (root, _) = basic();
        let mut app = build(&root, Some("mydiff -x"));
        let mut host = Rec::default();
        select(&mut app, "x.txt");
        key(&mut app, &mut host, KeyCode::Enter);
        assert_eq!(host.calls.len(), 1);
        assert_eq!(host.calls[0].0, "mydiff -x");
        assert_eq!(host.calls[0].1, vec![root.join("l/a/x.txt"), root.join("r/a/x.txt")]);
        assert_eq!(app.msg, "external done");
        assert!(matches!(app.mode, Mode::Normal));
        // a one-sided file has nothing to diff against: the built-in view opens instead
        select(&mut app, "new.txt");
        key(&mut app, &mut host, KeyCode::Enter);
        assert_eq!(host.calls.len(), 1);
        assert!(matches!(app.mode, Mode::Side(_)));
        let _ = fs::remove_dir_all(&root);
    }

    fn paired_fixture() -> (PathBuf, App) {
        let root = tmp();
        let (l, r) = (root.join("l"), root.join("r"));
        put(l.join("old/a"), "same");
        put(r.join("new/a"), "same");
        put(l.join("old/b"), "L");
        put(r.join("new/b"), "RR");
        put(l.join("old/c"), "only left");
        put(r.join("new/d"), "only right");
        put(l.join("other/f"), "o");
        put(r.join("other/f"), "o");
        put(l.join("p/sub1/f"), "1");
        put(r.join("p/sub2/f"), "1");
        put(l.join("top1/f"), "t");
        let app = build(&root, Some("dd"));
        (root, app)
    }

    fn top(app: &App) -> Vec<String> {
        app.rows.iter().filter(|r| r.depth == 0).map(|r| r.name.clone()).collect()
    }

    #[test]
    fn pairing_renamed_files() {
        let root = tmp();
        let (l, r) = (root.join("l"), root.join("r"));
        put(l.join("report.txt"), "one\ntwo\n");
        put(r.join("report-final.txt"), "one\nTWO\n");
        put(l.join("keep.txt"), "k");
        put(r.join("keep.txt"), "k");
        put(r.join("newdir/z"), "z");
        let mut app = build(&root, Some("dd"));
        assert_eq!(top(&app), vec!["newdir", "keep.txt", "report-final.txt", "report.txt"]);

        select(&mut app, "report.txt");
        keys(&mut app, "m");
        assert!(app.msg.starts_with("marked 'report.txt'"), "{}", app.msg);
        // a directory is not a counterpart for a file
        select(&mut app, "newdir");
        keys(&mut app, "m");
        assert!(app.msg.contains("pick a file that exists only on the right"), "{}", app.msg);
        assert!(app.mark.is_some(), "a bad pick keeps the mark");
        select(&mut app, "report-final.txt");
        keys(&mut app, "m");
        assert!(app.msg.contains("paired 'report.txt' with 'report-final.txt'"), "{}", app.msg);

        let name = "report.txt \u{21c4} report-final.txt";
        assert_eq!(top(&app), vec!["newdir", "keep.txt", name]);
        let row = app.rows.iter().find(|r| r.name == name).unwrap().clone();
        assert!(row.paired && !row.is_dir && row.status == Status::Modified);
        assert_eq!(own_name(&row, Side::Left), "report.txt");
        assert_eq!(own_name(&row, Side::Right), "report-final.txt");

        // the external diff gets the two real files
        let mut host = Rec::default();
        select(&mut app, name);
        key(&mut app, &mut host, KeyCode::Enter);
        assert_eq!(host.calls[0].1, vec![root.join("l/report.txt"), root.join("r/report-final.txt")]);

        // copying overwrites the right-hand file under its own name
        keys(&mut app, ">");
        keys(&mut app, "y");
        assert_eq!(fs::read_to_string(root.join("r/report-final.txt")).unwrap(), "one\ntwo\n");
        assert!(!root.join("r/report.txt").exists(), "nothing is created under the left-hand name");
        assert_eq!(app.rows.iter().find(|r| r.name == name).unwrap().status, Status::Same);

        // the actions menu offers to unpair, and M does it
        keys(&mut app, ".");
        assert!(action_names(&app).contains(&"unpair"));
        keys(&mut app, "q");
        keys(&mut app, "M");
        assert_eq!(app.msg, "unpaired");
        assert_eq!(top(&app), vec!["newdir", "keep.txt", "report-final.txt", "report.txt"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn pairing_renamed_directories() {
        let (root, mut app) = paired_fixture();
        assert_eq!(top(&app), vec!["new", "old", "other", "p", "top1"]);

        // marking, cancelling and bad picks
        select(&mut app, "old");
        keys(&mut app, "m");
        assert!(app.mark.is_some() && app.msg.starts_with("marked 'old'"), "{}", app.msg);
        keys(&mut app, "m"); // again on the same one cancels
        assert!(app.mark.is_none());
        keys(&mut app, "m");
        select(&mut app, "other");
        keys(&mut app, "m"); // exists on both sides
        assert!(app.msg.contains("one side only"), "{}", app.msg);
        assert!(app.mark.is_some(), "a bad pick keeps the mark");
        select(&mut app, "top1");
        keys(&mut app, "m"); // same side as the mark
        assert!(app.msg.contains("only on the right"), "{}", app.msg);
        code(&mut app, KeyCode::Esc);
        assert!(app.mark.is_none());

        // different parents are refused
        select(&mut app, "top1");
        keys(&mut app, "m");
        select(&mut app, "p");
        keys(&mut app, "l"); // expand p
        select(&mut app, "sub2");
        keys(&mut app, "m");
        assert!(app.msg.contains("same parent"), "{}", app.msg);
        keys(&mut app, "M"); // M cancels a pending mark
        assert!(app.mark.is_none());

        // the real pairing: old (left only) with new (right only)
        select(&mut app, "old");
        keys(&mut app, "m");
        select(&mut app, "new");
        keys(&mut app, "m");
        assert!(app.msg.contains("paired 'old' with 'new'"), "{}", app.msg);
        assert!(app.mark.is_none());
        assert_eq!(top(&app), vec!["old \u{21c4} new", "other", "p", "top1"]);
        select(&mut app, "old \u{21c4} new");
        keys(&mut app, "l");
        let kids: Vec<(String, Status)> = app
            .rows
            .iter()
            .filter(|r| r.depth == 1 && r.rel.starts_with("old"))
            .map(|r| (r.name.clone(), r.status))
            .collect();
        assert_eq!(
            kids,
            vec![
                ("a".to_string(), Status::Same),
                ("b".to_string(), Status::Modified),
                ("c".to_string(), Status::LeftOnly),
                ("d".to_string(), Status::RightOnly),
            ]
        );

        // paths on the right side use the right-hand name
        let mut host = Rec::default();
        select(&mut app, "b");
        key(&mut app, &mut host, KeyCode::Enter);
        assert_eq!(host.calls[0].1, vec![root.join("l/old/b"), root.join("r/new/b")]);

        // copy a one-sided file across the pair, then overwrite another
        select(&mut app, "c");
        keys(&mut app, ">");
        assert_eq!(fs::read_to_string(root.join("r/new/c")).unwrap(), "only left");
        assert!(!root.join("r/old").exists(), "nothing is created under the left-hand name");
        select(&mut app, "d");
        keys(&mut app, "<");
        assert_eq!(fs::read_to_string(root.join("l/old/d")).unwrap(), "only right");
        select(&mut app, "b");
        keys(&mut app, ">");
        keys(&mut app, "y");
        assert_eq!(fs::read_to_string(root.join("r/new/b")).unwrap(), "L");
        assert_eq!(app.rows.iter().find(|r| r.name == "old \u{21c4} new").unwrap().status, Status::Same);
        keys(&mut app, "u"); // undo works through the paired path too
        assert_eq!(fs::read_to_string(root.join("r/new/b")).unwrap(), "RR");

        // each panel shows its own directory name
        let row = app.rows.iter().find(|r| r.name == "old \u{21c4} new").unwrap().clone();
        assert_eq!(own_name(&row, Side::Left), "old");
        assert_eq!(own_name(&row, Side::Right), "new");
        let plain = app.rows.iter().find(|r| r.name == "other").unwrap();
        assert_eq!(own_name(plain, Side::Left), "other");

        // unpairing
        select(&mut app, "old \u{21c4} new");
        keys(&mut app, "M");
        assert_eq!(app.msg, "unpaired");
        assert_eq!(top(&app), vec!["new", "old", "other", "p", "top1"]);
        select(&mut app, "other");
        keys(&mut app, "M");
        assert_eq!(app.msg, "not a manually paired entry");

        // a pair nested in a directory that exists on both sides
        select(&mut app, "sub1");
        keys(&mut app, "m");
        select(&mut app, "sub2");
        keys(&mut app, "m");
        assert!(app.msg.contains("paired 'sub1' with 'sub2'"), "{}", app.msg);
        assert!(app.rows.iter().any(|r| r.name == "sub1 \u{21c4} sub2"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ignore_patterns_are_saved_per_pair() {
        let (root, mut app) = basic();
        let file = root.join("data/pair.ignore");
        select(&mut app, "new.txt");
        keys(&mut app, "i");
        assert!(app.msg.contains("saved for this pair"), "{}", app.msg);
        assert!(!names(&app).contains(&"new.txt".to_string()));
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("# dirrcmp ignore patterns for "), "{text}");
        assert_eq!(cli::read_pair_ignore(&file), vec!["/new.txt".to_string()]);

        // a fresh start loads them
        let again = build(&root, None);
        assert!(!names(&again).contains(&"new.txt".to_string()));
        assert_eq!(again.pair_lines, vec!["/new.txt".to_string()]);

        // the manager: add, select, delete, cancel
        keys(&mut app, "I");
        assert!(matches!(app.mode, Mode::Ignores(_)));
        keys(&mut app, "a*.tmp");
        code(&mut app, KeyCode::Enter);
        assert_eq!(app.pair_lines, vec!["/new.txt".to_string(), "*.tmp".to_string()]);
        assert_eq!(cli::read_pair_ignore(&file).len(), 2);
        keys(&mut app, "ax");
        code(&mut app, KeyCode::Esc); // cancels typing, keeps the manager open
        assert!(matches!(app.mode, Mode::Ignores(_)));
        assert_eq!(app.pair_lines.len(), 2);
        keys(&mut app, "d"); // selection moved to the added pattern
        assert_eq!(app.pair_lines, vec!["/new.txt".to_string()]);
        keys(&mut app, "k");
        keys(&mut app, "d");
        assert!(app.pair_lines.is_empty());
        assert!(!file.exists(), "no patterns left: the file is removed");
        assert!(names(&app).contains(&"new.txt".to_string()), "the entry is back");
        keys(&mut app, "d"); // nothing to delete
        keys(&mut app, "q");
        assert!(matches!(app.mode, Mode::Normal));

        // zi switches all patterns off and on again without touching the file
        select(&mut app, "new.txt");
        keys(&mut app, "i");
        keys(&mut app, "zi");
        assert!(!app.ignore_on && names(&app).contains(&"new.txt".to_string()));
        assert!(names(&app).contains(&"node_modules".to_string()));
        assert!(file.exists());
        keys(&mut app, "zi");
        assert!(!names(&app).contains(&"new.txt".to_string()));
        let _ = fs::remove_dir_all(&root);
    }

    fn keymap_of(entries: &[(&str, &[&str])]) -> Keymap {
        let e: Vec<(String, Vec<String>)> = entries
            .iter()
            .map(|(a, k)| (a.to_string(), k.iter().map(|s| s.to_string()).collect()))
            .collect();
        let (km, w) = Keymap::from_entries(&e);
        assert!(w.is_empty(), "{w:?}");
        km
    }

    fn ctrl(app: &mut App, c: char) -> bool {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL), &mut Rec::default()).unwrap()
    }

    fn action_names(app: &App) -> Vec<&'static str> {
        match &app.mode {
            Mode::Actions(ui) => ui.items.iter().map(|(a, _)| keys::ACTIONS[*a].name).collect(),
            _ => panic!("actions menu is not open"),
        }
    }

    #[test]
    fn chords_round_trip_through_key_events() {
        let all = [
            Chord::plain(Key::Char('j')),
            Chord::plain(Key::Char('G')),
            Chord::plain(Key::Char(' ')),
            Chord { key: Key::Char('d'), ctrl: true },
            Chord::plain(Key::Enter),
            Chord::plain(Key::Esc),
            Chord::plain(Key::Tab),
            Chord::plain(Key::Backspace),
            Chord::plain(Key::Up),
            Chord::plain(Key::Down),
            Chord::plain(Key::Left),
            Chord::plain(Key::Right),
            Chord::plain(Key::Home),
            Chord::plain(Key::End),
            Chord::plain(Key::PageUp),
            Chord::plain(Key::PageDown),
        ];
        for c in all {
            assert_eq!(chord_of(&event_of(c)), Some(c), "{c:?}");
        }
        // terminals report ctrl+shift+d as 'D' + CONTROL: treated as ctrl+d
        let e = KeyEvent::new(KeyCode::Char('D'), KeyModifiers::CONTROL);
        assert_eq!(chord_of(&e), Some(Chord { key: Key::Char('d'), ctrl: true }));
    }

    #[test]
    fn custom_bindings_drive_the_handler() {
        let (root, _) = basic();
        let km = keymap_of(&[("move_down", &["n"]), ("first", &["F"]), ("quit", &["Q"])]);
        let mut app = build_with(&root, None, km, false);
        let mut host = Rec::default();
        assert_eq!(app.cursor, 0);
        assert!(!key(&mut app, &mut host, KeyCode::Char('n')));
        assert_eq!(app.cursor, 1);
        // the old defaults of a rebound action do nothing
        keys(&mut app, "j");
        code(&mut app, KeyCode::Down);
        assert_eq!(app.cursor, 1);
        // untouched actions keep their keys
        keys(&mut app, "k");
        assert_eq!(app.cursor, 0);
        keys(&mut app, "nn");
        assert_eq!(app.cursor, 2);
        // a one-key binding for a multi-key action
        keys(&mut app, "F");
        assert_eq!(app.cursor, 0);
        keys(&mut app, "nn");
        keys(&mut app, "gg"); // the default sequence still works
        assert_eq!(app.cursor, 0);
        // quit moved from q to Q
        assert!(!key(&mut app, &mut host, KeyCode::Char('q')));
        assert!(key(&mut app, &mut host, KeyCode::Char('Q')));
        // ctrl-c always quits
        assert!(ctrl(&mut app, 'c'));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ctrl_bindings_and_named_keys() {
        let (root, _) = basic();
        let km = keymap_of(&[("move_down", &["ctrl+n"]), ("move_up", &["ctrl+o", "ctrl+k"])]);
        let mut app = build_with(&root, None, km, false);
        assert!(!ctrl(&mut app, 'n'));
        assert_eq!(app.cursor, 1);
        ctrl(&mut app, 'o');
        assert_eq!(app.cursor, 0);
        keys(&mut app, "j");
        assert_eq!(app.cursor, 0);
        keys(&mut app, "n");
        assert_eq!(app.cursor, 0, "plain n is not bound");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn second_key_of_a_sequence_is_never_remapped() {
        let (root, _) = basic();
        // `c` is the second key of `]c`; binding it must not break that sequence
        let km = keymap_of(&[("move_down", &["c"])]);
        let mut app = build_with(&root, None, km, false);
        keys(&mut app, "]c");
        assert_eq!(app.cursor, 2, "]c still jumps to the next difference");
        keys(&mut app, "]c");
        assert_eq!(app.cursor, 4);
        keys(&mut app, "[c");
        assert_eq!(app.cursor, 2, "[c still jumps back");
        keys(&mut app, "c"); // on its own it is the bound key
        assert_eq!(app.cursor, 3);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn startup_warnings_are_shown() {
        let (root, _) = basic();
        let mut cfg = cfg_for(&root, None, Keymap::default(), false);
        cfg.key_warnings = vec!["first problem".to_string(), "second".to_string()];
        let app = App::new(cfg);
        assert_eq!(app.msg, "keybindings: first problem (+1 more)");
        let mut cfg = cfg_for(&root, None, Keymap::default(), false);
        cfg.key_warnings = vec!["only".to_string()];
        assert_eq!(App::new(cfg).msg, "keybindings: only");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn actions_menu_is_contextual_and_runs_actions() {
        let (root, mut app) = basic();
        // a modified file
        select(&mut app, "x.txt");
        keys(&mut app, ".");
        let names_f = action_names(&app);
        for want in ["open", "unified_diff", "edit", "copy_right", "copy_left", "delete", "yank", "preferences", "quit"] {
            assert!(names_f.contains(&want), "{want} missing from {names_f:?}");
        }
        // pick the unified diff and run it
        let pos = names_f.iter().position(|n| *n == "unified_diff").unwrap();
        for _ in 0..pos {
            keys(&mut app, "j");
        }
        code(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Diff(_)), "menu item must run the action");
        keys(&mut app, "q");

        // a directory that exists on the left only
        select(&mut app, "gone");
        keys(&mut app, ".");
        let names_d = action_names(&app);
        assert!(names_d.contains(&"copy_right") && !names_d.contains(&"copy_left"));
        assert!(names_d.contains(&"pair") && !names_d.contains(&"unified_diff") && !names_d.contains(&"edit"));
        // an identical file offers no copy
        keys(&mut app, "q");
        select(&mut app, "same.txt");
        keys(&mut app, ".");
        let names_s = action_names(&app);
        assert!(!names_s.contains(&"copy_right") && !names_s.contains(&"copy_left"));
        // Esc and '.' close it without doing anything
        code(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        keys(&mut app, ".");
        keys(&mut app, ".");
        assert!(matches!(app.mode, Mode::Normal));

        // running "quit" from the menu quits
        keys(&mut app, ".");
        let n = action_names(&app).len();
        for _ in 0..n - 1 {
            keys(&mut app, "j");
        }
        assert!(key(&mut app, &mut Rec::default(), KeyCode::Enter));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn menu_actions_use_the_handlers_keys_even_when_rebound() {
        let (root, _) = basic();
        // `j` is no longer move_down, but the menu must still be able to run it
        let km = keymap_of(&[("move_down", &["n"]), ("refresh", &["X"])]);
        let mut app = build_with(&root, None, km, false);
        keys(&mut app, ".");
        let names = action_names(&app);
        let pos = names.iter().position(|n| *n == "refresh").unwrap();
        for _ in 0..pos {
            keys(&mut app, "j"); // `j` navigates the menu itself
        }
        fs::write(root.join("r/late.txt"), "x").unwrap();
        code(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.rows.iter().any(|r| r.name == "late.txt"), "refresh ran");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn preferences_apply_live_and_are_saved() {
        let (root, mut app) = basic();
        let file = root.join("cfg/config.json");
        assert!(!file.exists());
        keys(&mut app, ",");
        assert!(matches!(app.mode, Mode::Prefs(_)));

        keys(&mut app, " "); // compare contents off
        assert!(!app.prefs.compare_contents && !app.opts.compare_contents);
        assert!(app.msg.starts_with("saved to "), "{}", app.msg);
        assert!(!config::load_from(&file).0.compare_contents);

        keys(&mut app, "j ");
        assert!(app.prefs.compare_dates && app.opts.compare_dates);
        keys(&mut app, "j ");
        assert!(!app.dirs_first);
        keys(&mut app, "j ");
        assert!(!app.show_hints);
        keys(&mut app, "j ");
        assert!(app.nerd_font);
        let saved = config::load_from(&file).0;
        assert!(saved.compare_dates && !saved.dirs_first && !saved.show_hints && saved.nerd_font);

        // syntax highlighting
        assert!(app.syntax);
        keys(&mut app, "j ");
        assert!(!app.syntax && !app.prefs.syntax);
        assert!(!config::load_from(&file).0.syntax);
        assert!(app.chunk_bg);
        keys(&mut app, "j ");
        assert!(!app.chunk_bg && !app.prefs.chunk_bg);
        assert!(!config::load_from(&file).0.chunk_bg);
        assert!(app.smooth);
        keys(&mut app, "j ");
        assert!(!app.smooth && !app.prefs.smooth);
        assert!(!config::load_from(&file).0.smooth);

        // rename detection (off in these tests' base config, so this turns it on)
        assert!(!app.opts.detect_renames);
        keys(&mut app, "j ");
        assert!(app.opts.detect_renames && app.prefs.renames);
        assert!(config::load_from(&file).0.renames);

        // external diff command: Enter edits, typing, Enter saves
        keys(&mut app, "j");
        code(&mut app, KeyCode::Enter);
        assert!(matches!(&app.mode, Mode::Prefs(ui) if ui.editing.is_some()));
        keys(&mut app, "meld -x");
        code(&mut app, KeyCode::Backspace);
        code(&mut app, KeyCode::Enter);
        assert_eq!(app.diff_cmd.as_deref(), Some("meld -"));
        assert_eq!(config::load_from(&file).0.diff_command.as_deref(), Some("meld -"));
        // Esc while editing cancels the edit but keeps the menu open
        code(&mut app, KeyCode::Enter);
        keys(&mut app, "zzz");
        code(&mut app, KeyCode::Esc);
        assert!(matches!(&app.mode, Mode::Prefs(ui) if ui.editing.is_none()));
        assert_eq!(app.diff_cmd.as_deref(), Some("meld -"));
        // an empty command clears it
        code(&mut app, KeyCode::Enter);
        for _ in 0..10 {
            code(&mut app, KeyCode::Backspace);
        }
        code(&mut app, KeyCode::Enter);
        assert_eq!(app.diff_cmd, None);
        assert_eq!(config::load_from(&file).0.diff_command, None);

        code(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn comparison_preferences_change_results() {
        let root = tmp();
        let (l, r) = (root.join("l"), root.join("r"));
        put(l.join("f.txt"), "same");
        put(r.join("f.txt"), "same");
        // identical content, very different mtime
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
        fs::OpenOptions::new().write(true).open(l.join("f.txt")).unwrap().set_modified(old).unwrap();
        let mut app = build(&root, None);
        assert_eq!(app.rows[0].status, Status::Same);
        keys(&mut app, ",j "); // compare dates on
        assert_eq!(app.rows[0].status, Status::Modified, "different date now counts");
        keys(&mut app, " "); // and off again
        assert_eq!(app.rows[0].status, Status::Same);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn command_line_overrides_win_and_are_reported() {
        let (root, _) = basic();
        let mut app = build_with(&root, Some("nvim -d"), Keymap::default(), true);
        assert!(app.opts.compare_dates);
        keys(&mut app, ",j "); // prefs.compare_dates -> true
        keys(&mut app, " "); // -> false, but --dates still forces it
        assert!(!app.prefs.compare_dates);
        assert!(app.opts.compare_dates, "--dates still applies");
        assert!(app.msg.contains("--dates"), "{}", app.msg);
        // the saved file must not contain the command-line-only values
        let saved = config::load_from(&root.join("cfg/config.json")).0;
        assert!(!saved.compare_dates);
        assert_eq!(saved.diff_command, None);
        // diff command from the command line is not replaced by the file's
        assert_eq!(app.diff_cmd.as_deref(), Some("nvim -d"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unsaveable_preferences_say_so() {
        let (root, _) = basic();
        let mut cfg = cfg_for(&root, None, Keymap::default(), false);
        cfg.config_file = None;
        let mut app = App::new(cfg);
        keys(&mut app, ", ");
        assert!(!app.prefs.compare_contents, "the change still applies this run");
        assert!(app.msg.starts_with("not saved"), "{}", app.msg);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn key_editor_rebinds_and_saves() {
        let (root, mut app) = basic();
        let file = root.join("cfg/keybindings.json");
        keys(&mut app, "K");
        assert!(matches!(&app.mode, Mode::Keys(ui) if ui.sel == 0)); // move_down

        code(&mut app, KeyCode::Enter);
        assert!(matches!(&app.mode, Mode::Keys(ui) if ui.capture == Some(false)));
        keys(&mut app, "n");
        assert!(matches!(&app.mode, Mode::Keys(ui) if ui.capture.is_none()));
        assert_eq!(app.msg, "saved");
        let on_disk = config::parse_string_map(&fs::read_to_string(&file).unwrap()).unwrap();
        // `j` is `move_down`'s old key: editor stole nothing, `n` was free
        assert_eq!(on_disk, vec![("move_down".to_string(), vec!["n".to_string()])]);

        // live: close the editor and use it
        code(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        keys(&mut app, "n");
        assert_eq!(app.cursor, 1);
        keys(&mut app, "j");
        assert_eq!(app.cursor, 1, "old key no longer works");

        // add a second key; taking `m` from `pair` is reported
        keys(&mut app, "K");
        keys(&mut app, "a");
        assert!(matches!(&app.mode, Mode::Keys(ui) if ui.capture == Some(true)));
        keys(&mut app, "m");
        assert!(app.msg.contains("'m' taken from pair"), "{}", app.msg);
        assert_eq!(app.keymap.keys_for(0), vec!["n".to_string(), "m".to_string()]);
        let reloaded = keys::load(Some(&file)).0;
        assert_eq!(reloaded.keys_for(0), app.keymap.keys_for(0));
        assert_eq!(reloaded.keys_for(keys::action_index("pair").unwrap()), Vec::<String>::new());

        // Esc while waiting cancels and changes nothing
        let before = app.keymap.entries();
        code(&mut app, KeyCode::Enter);
        code(&mut app, KeyCode::Esc);
        assert!(matches!(&app.mode, Mode::Keys(ui) if ui.capture.is_none()));
        assert_eq!(app.keymap.entries(), before);

        // navigate, reset one action, reset all
        keys(&mut app, "jjj");
        assert!(matches!(&app.mode, Mode::Keys(ui) if ui.sel == 3));
        keys(&mut app, "kkk");
        keys(&mut app, "x");
        assert!(!app.keymap.is_overridden(0));
        assert!(app.keymap.is_overridden(keys::action_index("pair").unwrap()));
        keys(&mut app, "R");
        assert!(app.keymap.is_default());
        assert_eq!(fs::read_to_string(&file).unwrap(), "{}\n");
        keys(&mut app, "q");
        assert!(matches!(app.mode, Mode::Normal));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn key_editor_can_bind_special_keys() {
        let (root, mut app) = basic();
        keys(&mut app, "K");
        code(&mut app, KeyCode::Enter);
        code(&mut app, KeyCode::PageDown); // move_down on PageDown (and only that)
        assert_eq!(app.keymap.keys_for(0), vec!["pagedown".to_string()]);
        ctrl(&mut app, 'x'); // ctrl-x outside capture does nothing
        code(&mut app, KeyCode::Esc);
        code(&mut app, KeyCode::PageDown);
        assert_eq!(app.cursor, 1);
        // an unbindable key is refused
        keys(&mut app, "K");
        code(&mut app, KeyCode::Enter);
        code(&mut app, KeyCode::F(5));
        assert_eq!(app.msg, "that key can't be bound");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn key_editor_without_a_config_dir_says_so() {
        let (root, _) = basic();
        let mut cfg = cfg_for(&root, None, Keymap::default(), false);
        cfg.keys_file = None;
        let mut app = App::new(cfg);
        keys(&mut app, "K");
        code(&mut app, KeyCode::Enter);
        keys(&mut app, "n");
        assert!(app.msg.starts_with("not saved"), "{}", app.msg);
        assert_eq!(app.keymap.keys_for(0), vec!["n".to_string()], "still applies this run");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_only_snapshot_sides_refuse_changes() {
        let (root, mut app) = basic();
        app.readonly = (true, false);
        app.labels = Some(("HEAD".to_string(), "working tree".to_string()));
        select(&mut app, "x.txt");
        let l = root.join("l/a/x.txt");
        let r = root.join("r/a/x.txt");
        // copy into the snapshot, delete on it, edit it: all refused, nothing touched
        keys(&mut app, "<");
        assert!(app.msg.contains("read-only git snapshot") && app.msg.contains("HEAD"), "{}", app.msg);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(fs::read_to_string(&l).unwrap(), "1\n");
        app.focus = Side::Left;
        keys(&mut app, "d");
        assert!(matches!(app.mode, Mode::Normal), "no delete prompt on the snapshot");
        assert!(l.exists());
        keys(&mut app, "e");
        assert!(app.msg.contains("read-only"), "{}", app.msg);
        // the working tree side can be changed: copy HEAD's version over it (a revert)
        keys(&mut app, ">");
        assert!(matches!(app.mode, Mode::Confirm(_)));
        keys(&mut app, "y");
        assert_eq!(fs::read_to_string(&r).unwrap(), "1\n");
        // deleting on the working tree side still asks
        app.focus = Side::Right;
        select(&mut app, "new.txt");
        keys(&mut app, "d");
        assert!(matches!(app.mode, Mode::Confirm(_)));
        keys(&mut app, "n");
        // swapping the panels moves the flag with the side
        keys(&mut app, "S");
        assert_eq!(app.readonly, (false, true));
        assert_eq!(app.labels.as_ref().unwrap().0, "working tree");
        keys(&mut app, "S");
        assert_eq!(app.readonly, (true, false));
        // and the side-by-side editor gets the same rule
        select(&mut app, "x.txt");
        put(root.join("l/a/x.txt"), "1\n2\n");
        app.refresh();
        select(&mut app, "x.txt");
        code(&mut app, KeyCode::Enter);
        let Mode::Side(sv) = &app.mode else { panic!("side view") };
        assert_eq!(sv.readonly(), (true, false));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nothing_is_read_only_without_git_mode() {
        let (root, mut app) = basic();
        assert_eq!(app.readonly, (false, false));
        select(&mut app, "x.txt");
        keys(&mut app, "<");
        assert!(matches!(app.mode, Mode::Confirm(_)));
        let _ = fs::remove_dir_all(&root);
    }

    // ── picker, file switching, sidebar ──

    fn project() -> (PathBuf, App) {
        let root = tmp();
        let (l, r) = (root.join("l"), root.join("r"));
        put(l.join("docs/readme.md"), "a\n");
        put(r.join("docs/readme.md"), "b\n");
        put(l.join("gone/old.txt"), "old\n");
        put(l.join("src/util/deep.rs"), "same\n");
        put(r.join("src/util/deep.rs"), "same\n");
        put(l.join("src/main.rs"), "fn main() {}\n");
        put(r.join("src/main.rs"), "fn main() { x }\n");
        put(l.join("src/side.rs"), "1\n");
        put(r.join("src/side.rs"), "2\n");
        put(l.join("z.txt"), "z\n");
        put(r.join("z.txt"), "z\n");
        let app = build(&root, None);
        (root, app)
    }

    fn open_file(app: &mut App, name: &str) {
        select(app, name);
        code(app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Side(_)), "{name} opens in the editor");
    }

    fn typed_picker(app: &mut App, text: &str) {
        for c in text.chars() {
            code(app, KeyCode::Char(c));
        }
    }

    fn picker(app: &App) -> &PickerUi {
        match &app.mode {
            Mode::Picker(p) => p,
            _ => panic!("expected the picker"),
        }
    }

    fn shown_labels(app: &App) -> Vec<String> {
        let p = picker(app);
        p.shown.iter().map(|(i, _)| p.items[*i].label()).collect()
    }

    #[test]
    fn collects_every_file_even_in_collapsed_directories() {
        let (root, app) = project();
        let labels: Vec<String> = app.files().iter().map(|f| f.label()).collect();
        assert_eq!(
            labels,
            ["docs/readme.md", "gone/old.txt", "src/util/deep.rs", "src/main.rs", "src/side.rs", "z.txt"]
        );
        assert!(!names(&app).contains(&"old.txt".to_string()), "gone/ is collapsed in the list");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn picker_filters_ranks_and_opens_files() {
        let (root, mut app) = project();
        ctrl(&mut app, 'p');
        assert_eq!(picker(&app).items.len(), 6);
        assert_eq!(picker(&app).shown.len(), 6, "empty query shows everything in tree order");
        typed_picker(&mut app, "side");
        assert_eq!(shown_labels(&app)[0], "src/side.rs");
        assert!(picker(&app).shown[0].1.len() == 4, "matched positions are kept for highlighting");
        // editing the query
        code(&mut app, KeyCode::Backspace);
        code(&mut app, KeyCode::Backspace);
        code(&mut app, KeyCode::Backspace);
        code(&mut app, KeyCode::Backspace);
        assert_eq!(picker(&app).query, "");
        typed_picker(&mut app, "zzz");
        assert!(picker(&app).shown.is_empty());
        code(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Picker(_)), "Enter with no match does nothing");
        ctrl(&mut app, 'u');
        assert_eq!(picker(&app).query, "");
        typed_picker(&mut app, "src/ma");
        ctrl(&mut app, 'w');
        assert_eq!(picker(&app).query, "src/");
        // changed only drops the identical files
        code(&mut app, KeyCode::Tab);
        assert!(!shown_labels(&app).contains(&"src/util/deep.rs".to_string()));
        assert!(shown_labels(&app).contains(&"src/main.rs".to_string()));
        code(&mut app, KeyCode::Tab);
        assert!(shown_labels(&app).contains(&"src/util/deep.rs".to_string()));
        // moving the selection, then opening
        ctrl(&mut app, 'u');
        code(&mut app, KeyCode::Down);
        code(&mut app, KeyCode::Down);
        assert_eq!(picker(&app).sel, 2);
        ctrl(&mut app, 'p');
        assert_eq!(picker(&app).sel, 1);
        code(&mut app, KeyCode::Up);
        code(&mut app, KeyCode::Up);
        assert_eq!(picker(&app).sel, 0, "stops at the top");
        ctrl(&mut app, 'u');
        typed_picker(&mut app, "old");
        code(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Side(_)));
        assert_eq!(app.side_rel, Some(PathBuf::from("gone/old.txt")));
        assert!(app.expanded.contains(Path::new("gone")), "the list reveals the file");
        assert_eq!(app.rows[app.cursor].name, "old.txt");
        keys(&mut app, "q");
        // Esc closes it without opening anything
        ctrl(&mut app, 'p');
        code(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        // Ctrl-C quits, as in the other popups (only an editor underneath protects it)
        ctrl(&mut app, 'p');
        assert!(ctrl(&mut app, 'c'));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn picker_inside_the_editor_returns_or_switches() {
        let (root, mut app) = project();
        open_file(&mut app, "main.rs");
        assert_eq!(app.side_rel, Some(PathBuf::from("src/main.rs")));
        ctrl(&mut app, 'p');
        assert!(picker(&app).back.is_some());
        assert!(!ctrl(&mut app, 'c'), "Ctrl-C must not quit with an editor underneath");
        assert!(matches!(app.mode, Mode::Side(_)), "back in the editor");
        // switch
        ctrl(&mut app, 'p');
        typed_picker(&mut app, "readme");
        code(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Side(_)));
        assert_eq!(app.side_rel, Some(PathBuf::from("docs/readme.md")));
        // with unsaved edits the switch is refused and the edits stay
        keys(&mut app, ">");
        let Mode::Side(sv) = &app.mode else { panic!() };
        assert!(sv.has_unsaved());
        ctrl(&mut app, 'p');
        typed_picker(&mut app, "side");
        code(&mut app, KeyCode::Enter);
        let Mode::Side(sv) = &app.mode else { panic!("still in the editor") };
        assert!(sv.has_unsaved(), "edits were not thrown away");
        assert!(sv.status().contains("unsaved"), "{}", sv.status());
        assert_eq!(app.side_rel, Some(PathBuf::from("docs/readme.md")));
        // save, then switching works and the list is refreshed
        keys(&mut app, "w");
        ctrl(&mut app, 'p');
        typed_picker(&mut app, "side");
        code(&mut app, KeyCode::Enter);
        assert_eq!(app.side_rel, Some(PathBuf::from("src/side.rs")));
        let readme = app.files().into_iter().find(|f| f.name == "readme.md").unwrap();
        assert_eq!(readme.status, Status::Same, "saved: the tree was refreshed");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn next_and_previous_changed_file() {
        let (root, mut app) = project();
        open_file(&mut app, "main.rs");
        keys(&mut app, "]f");
        assert_eq!(app.side_rel, Some(PathBuf::from("src/side.rs")));
        keys(&mut app, "]f");
        let Mode::Side(sv) = &app.mode else { panic!() };
        assert!(sv.status().contains("no more changed files"), "{}", sv.status());
        assert_eq!(app.side_rel, Some(PathBuf::from("src/side.rs")), "z.txt is identical, so we stay");
        keys(&mut app, "[f");
        assert_eq!(app.side_rel, Some(PathBuf::from("src/main.rs")));
        keys(&mut app, "[f");
        assert_eq!(app.side_rel, Some(PathBuf::from("gone/old.txt")), "skips the identical deep.rs");
        keys(&mut app, "[f");
        assert_eq!(app.side_rel, Some(PathBuf::from("docs/readme.md")));
        keys(&mut app, "[f");
        let Mode::Side(sv) = &app.mode else { panic!() };
        assert!(sv.status().contains("no earlier"), "{}", sv.status());
        // closing forgets the file
        keys(&mut app, "q");
        assert!(matches!(app.mode, Mode::Normal) && app.side_rel.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sidebar_shows_the_tree_and_switches_files() {
        let (root, mut app) = project();
        open_file(&mut app, "main.rs");
        assert!(!app.sidebar);
        keys(&mut app, "t");
        assert!(app.sidebar && !app.sb_focus && app.sb_active());
        keys(&mut app, "T");
        assert!(app.sb_focus);
        // keys now go to the sidebar: j moves in the list, the editor does not scroll
        let start = app.cursor;
        keys(&mut app, "j");
        assert_eq!(app.cursor, start + 1);
        // Enter on a file opens it
        select(&mut app, "side.rs");
        code(&mut app, KeyCode::Enter);
        assert_eq!(app.side_rel, Some(PathBuf::from("src/side.rs")));
        assert!(app.sb_focus, "focus stays in the sidebar for the next file");
        // on a directory it folds / unfolds
        select(&mut app, "docs");
        let before = app.rows.len();
        code(&mut app, KeyCode::Enter);
        assert!(app.rows.len() < before, "docs collapsed");
        keys(&mut app, "l");
        assert_eq!(app.rows.len(), before, "and expanded again");
        keys(&mut app, "h");
        assert!(app.rows.len() < before, "h collapses");
        keys(&mut app, "l");
        // g / G
        keys(&mut app, "G");
        assert_eq!(app.cursor, app.rows.len() - 1);
        keys(&mut app, "g");
        assert_eq!(app.cursor, 0);
        // the picker works from the sidebar and returns to it
        ctrl(&mut app, 'p');
        assert!(picker(&app).back.is_some());
        code(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Side(_)) && app.sb_focus);
        // Esc hands the keyboard back to the editor
        code(&mut app, KeyCode::Esc);
        assert!(!app.sb_focus && app.sidebar && matches!(app.mode, Mode::Side(_)));
        keys(&mut app, "j");
        assert_eq!(app.side_rel, Some(PathBuf::from("src/side.rs")), "j scrolls the editor now");
        // T focuses again, t inside the sidebar hides it
        keys(&mut app, "T");
        assert!(app.sb_focus);
        keys(&mut app, "t");
        assert!(!app.sidebar && !app.sb_focus);
        keys(&mut app, "t");
        assert!(app.sidebar);
        // closing the editor drops the focus; the sidebar setting stays for the next file
        keys(&mut app, "T");
        code(&mut app, KeyCode::Esc);
        keys(&mut app, "q");
        assert!(matches!(app.mode, Mode::Normal) && !app.sb_focus && app.sidebar);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sidebar_needs_a_wide_terminal() {
        let (root, mut app) = project();
        app.term_w = 60;
        open_file(&mut app, "main.rs");
        keys(&mut app, "t");
        assert!(app.sidebar && !app.sb_active());
        let Mode::Side(sv) = &app.mode else { panic!() };
        assert!(sv.status().contains("80 columns"), "{}", sv.status());
        keys(&mut app, "T");
        assert!(!app.sb_focus, "no hidden keyboard focus when nothing is drawn");
        keys(&mut app, "j");
        assert_eq!(app.side_rel, Some(PathBuf::from("src/main.rs")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sidebar_scrolls_to_keep_the_cursor_visible() {
        assert_eq!(sidebar_top(0, 3, 10), 0);
        assert_eq!(sidebar_top(0, 10, 10), 1);
        assert_eq!(sidebar_top(5, 2, 10), 2);
        assert_eq!(sidebar_top(5, 8, 10), 5);
        assert_eq!(sidebar_top(0, 0, 0), 0);
    }
}
