//! Key bindings for the directory view (std only).
//!
//! Every action has default keys. `keybindings.json` (or the in-app editor)
//! can bind other keys to an action. A pressed key is *translated* into the
//! action's canonical default key sequence, which the existing key handler
//! then runs, so rebinding never touches the handler itself.
//!
//! Rules:
//! * Binding keys to an action replaces its single-key defaults (`j`, `Down`, ...).
//! * Multi-key defaults (`gg`, `zR`, `zM`, `zi`, `]c`, `[c`) always keep working;
//!   bindings for those actions are additional single-key shortcuts.
//! * Ctrl-C always quits.

use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Tab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub key: Key,
    pub ctrl: bool,
}

impl Chord {
    pub fn plain(key: Key) -> Chord {
        Chord { key, ctrl: false }
    }
}

pub fn parse_chord(spec: &str) -> Result<Chord, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("empty key".to_string());
    }
    let lower = spec.to_lowercase();
    for bad in ["shift+", "alt+", "meta+", "super+", "cmd+"] {
        if lower.starts_with(bad) {
            return Err(format!("'{spec}': only ctrl+<char> and plain keys are supported"));
        }
    }
    let (ctrl, rest) = match lower.strip_prefix("ctrl+") {
        Some(_) => (true, &spec[5..]),
        None => (false, spec),
    };
    if rest.is_empty() {
        return Err(format!("'{spec}': missing key after ctrl+"));
    }
    let mut it = rest.chars();
    let first = it.next().unwrap();
    if it.next().is_none() {
        let c = if ctrl { first.to_ascii_lowercase() } else { first };
        return Ok(Chord { key: Key::Char(c), ctrl });
    }
    if ctrl {
        return Err(format!("'{spec}': ctrl+ only combines with a single character"));
    }
    let key = match rest.to_lowercase().as_str() {
        "enter" | "return" => Key::Enter,
        "esc" | "escape" => Key::Esc,
        "tab" => Key::Tab,
        "space" => Key::Char(' '),
        "backspace" => Key::Backspace,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        _ => return Err(format!("unknown key '{spec}'")),
    };
    Ok(Chord::plain(key))
}

pub fn chord_name(c: Chord) -> String {
    let base = match c.key {
        Key::Char(' ') => "space".to_string(),
        Key::Char(ch) => ch.to_string(),
        Key::Enter => "enter".to_string(),
        Key::Esc => "esc".to_string(),
        Key::Tab => "tab".to_string(),
        Key::Backspace => "backspace".to_string(),
        Key::Up => "up".to_string(),
        Key::Down => "down".to_string(),
        Key::Left => "left".to_string(),
        Key::Right => "right".to_string(),
        Key::Home => "home".to_string(),
        Key::End => "end".to_string(),
        Key::PageUp => "pageup".to_string(),
        Key::PageDown => "pagedown".to_string(),
    };
    if c.ctrl {
        format!("ctrl+{base}")
    } else {
        base
    }
}

// ───────────────────────────── actions ─────────────────────────────

pub struct ActionDef {
    pub name: &'static str,
    pub desc: &'static str,
    /// Single-key defaults (replaced when the action is rebound).
    pub keys: &'static [&'static str],
    /// Multi-key default, space separated (always kept), or "".
    pub seq: &'static str,
    /// The key sequence the handler understands for this action.
    pub canon: &'static str,
}

macro_rules! act {
    ($n:expr, $d:expr, [$($k:expr),*], $s:expr, $c:expr) => {
        ActionDef { name: $n, desc: $d, keys: &[$($k),*], seq: $s, canon: $c }
    };
}

pub const ACTIONS: &[ActionDef] = &[
    act!("move_down", "Move down", ["j", "down"], "", "j"),
    act!("move_up", "Move up", ["k", "up"], "", "k"),
    act!("page_down", "Page down", ["pagedown"], "", "pagedown"),
    act!("page_up", "Page up", ["pageup"], "", "pageup"),
    act!("half_page_down", "Half page down", ["ctrl+d"], "", "ctrl+d"),
    act!("half_page_up", "Half page up", ["ctrl+u"], "", "ctrl+u"),
    act!("full_page_down", "Full page down", ["ctrl+f"], "", "ctrl+f"),
    act!("full_page_up", "Full page up", ["ctrl+b"], "", "ctrl+b"),
    act!("scroll_down", "Scroll view down", ["ctrl+e"], "", "ctrl+e"),
    act!("scroll_up", "Scroll view up", ["ctrl+y"], "", "ctrl+y"),
    act!("first", "First entry", ["home"], "g g", "g g"),
    act!("last", "Last entry", ["end", "G"], "", "G"),
    act!("next_diff", "Next difference", [], "] c", "] c"),
    act!("prev_diff", "Previous difference", [], "[ c", "[ c"),
    act!("expand", "Expand directory / open file", ["l", "right"], "", "l"),
    act!("collapse", "Collapse / go to parent", ["h", "left"], "", "h"),
    act!("expand_all", "Expand all directories", [], "z R", "z R"),
    act!("collapse_all", "Collapse all directories", [], "z M", "z M"),
    act!("switch_panel", "Switch panel focus", ["tab", "%"], "", "tab"),
    act!("focus_left", "Focus left panel", ["H"], "", "H"),
    act!("focus_right", "Focus right panel", ["L"], "", "L"),
    act!("open", "Open side-by-side editor", ["enter"], "", "enter"),
    act!("unified_diff", "Read-only unified diff", ["D"], "", "D"),
    act!("copy_right", "Copy to right", [">"], "", ">"),
    act!("copy_left", "Copy to left", ["<"], "", "<"),
    act!("copy_across", "Copy focused side across", ["space"], "", "space"),
    act!("delete", "Delete on focused side", ["d"], "", "d"),
    act!("undo", "Undo copy / delete", ["u"], "", "u"),
    act!("redo", "Redo", ["U"], "", "U"),
    act!("yank", "Yank path", ["y"], "", "y"),
    act!("edit", "Open in $EDITOR", ["e"], "", "e"),
    act!("refresh", "Refresh", ["r"], "", "r"),
    act!("swap", "Swap panels", ["S"], "", "S"),
    act!("sort", "Cycle sort", ["s"], "", "s"),
    act!("find_file", "Find file (fuzzy picker)", ["ctrl+p"], "", "ctrl+p"),
    act!("filter", "Filter by name", ["/"], "", "/"),
    act!("status_filter", "Cycle status filter", ["f"], "", "f"),
    act!("ignore_entry", "Ignore this entry", ["i"], "", "i"),
    act!("manage_ignores", "Manage ignore patterns", ["I"], "", "I"),
    act!("toggle_ignore", "Toggle ignore patterns", [], "z i", "z i"),
    act!("pair", "Pair renamed directory", ["m"], "", "m"),
    act!("unpair", "Remove pairing", ["M"], "", "M"),
    act!("preferences", "Preferences", [","], "", ","),
    act!("actions", "Actions menu", ["."], "", "."),
    act!("keybindings", "Keybinding editor", ["K"], "", "K"),
    act!("help", "Help", ["?"], "", "?"),
    act!("quit", "Quit", ["q"], "", "q"),
];

pub fn action_index(name: &str) -> Option<usize> {
    ACTIONS.iter().position(|a| a.name == name)
}

fn parse_seq(s: &str) -> Vec<Chord> {
    s.split_whitespace().filter_map(|p| parse_chord(p).ok()).collect()
}

/// The key sequence the handler runs for `action`.
pub fn canon(action: usize) -> Vec<Chord> {
    parse_seq(ACTIONS[action].canon)
}

fn default_keys(action: usize) -> Vec<Chord> {
    ACTIONS[action].keys.iter().filter_map(|k| parse_chord(k).ok()).collect()
}

/// Display form of a multi-key default: `g g` -> `gg`.
fn seq_name(seq: &str) -> String {
    let parts: Vec<&str> = seq.split_whitespace().collect();
    if parts.iter().all(|p| p.chars().count() == 1) {
        parts.concat()
    } else {
        parts.join(" ")
    }
}

// ───────────────────────────── keymap ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tr {
    /// Not remapped: handle the key as pressed.
    Pass,
    /// A default key of a rebound action: do nothing.
    Drop,
    /// Run this sequence instead.
    Feed(Vec<Chord>),
}

#[derive(Clone, Debug)]
pub struct Keymap {
    /// Per action: `None` = defaults, `Some(keys)` = user keys.
    over: Vec<Option<Vec<Chord>>>,
    user: HashMap<Chord, usize>,
    dropped: HashSet<Chord>,
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap { over: vec![None; ACTIONS.len()], user: HashMap::new(), dropped: HashSet::new() }
    }
}

impl Keymap {
    pub fn is_default(&self) -> bool {
        self.over.iter().all(|o| o.is_none())
    }

    fn rebuild(&mut self) {
        self.user.clear();
        self.dropped.clear();
        for (i, o) in self.over.iter().enumerate() {
            if let Some(keys) = o {
                for k in keys {
                    self.user.entry(*k).or_insert(i);
                }
                for d in default_keys(i) {
                    self.dropped.insert(d);
                }
            }
        }
    }

    /// Build from `(action, keys)` pairs as read from the file. Problems are
    /// reported as warnings; valid entries still apply.
    pub fn from_entries(entries: &[(String, Vec<String>)]) -> (Keymap, Vec<String>) {
        let mut km = Keymap::default();
        let mut warn = Vec::new();
        let mut seen: HashMap<Chord, usize> = HashMap::new();
        for (name, specs) in entries {
            let Some(ai) = action_index(name) else {
                warn.push(format!("unknown action '{name}'"));
                continue;
            };
            let mut keys: Vec<Chord> = Vec::new();
            let mut bad = false;
            for s in specs {
                match parse_chord(s) {
                    Ok(c) if c == (Chord { key: Key::Char('c'), ctrl: true }) => {
                        warn.push(format!("{name}: ctrl+c always quits and can't be rebound"));
                        bad = true;
                    }
                    Ok(c) => {
                        if let Some(&other) = seen.get(&c) {
                            warn.push(format!(
                                "{name}: '{}' is already bound to {}",
                                chord_name(c),
                                ACTIONS[other].name
                            ));
                        } else if !keys.contains(&c) {
                            keys.push(c);
                        }
                    }
                    Err(e) => {
                        warn.push(format!("{name}: {e}"));
                        bad = true;
                    }
                }
            }
            if bad && keys.is_empty() && !specs.is_empty() {
                continue; // nothing usable: keep the defaults
            }
            for k in &keys {
                seen.insert(*k, ai);
            }
            km.over[ai] = Some(keys);
        }
        km.rebuild();
        // A user key that shadows another action's default key.
        for (c, &ai) in &km.user {
            for (j, a) in ACTIONS.iter().enumerate() {
                if j != ai && km.over[j].is_none() && default_keys(j).contains(c) {
                    warn.push(format!(
                        "'{}' now runs {} instead of {}",
                        chord_name(*c),
                        ACTIONS[ai].name,
                        a.name
                    ));
                }
            }
        }
        // A user key that is the first key of a multi-key default breaks that sequence.
        for c in km.user.keys() {
            for a in ACTIONS.iter().filter(|a| !a.seq.is_empty()) {
                if parse_seq(a.seq).first() == Some(c) {
                    warn.push(format!(
                        "'{}' starts the default sequence '{}' ({}), which stops working",
                        chord_name(*c),
                        seq_name(a.seq),
                        a.name
                    ));
                }
            }
        }
        warn.sort();
        (km, warn)
    }

    pub fn translate(&self, c: Chord) -> Tr {
        if let Some(&ai) = self.user.get(&c) {
            return Tr::Feed(canon(ai));
        }
        if self.dropped.contains(&c) {
            return Tr::Drop;
        }
        Tr::Pass
    }

    /// Keys currently bound to `action`, for display.
    pub fn keys_for(&self, action: usize) -> Vec<String> {
        match &self.over[action] {
            Some(keys) => {
                let mut v: Vec<String> = keys.iter().map(|k| chord_name(*k)).collect();
                if !ACTIONS[action].seq.is_empty() {
                    v.push(seq_name(ACTIONS[action].seq));
                }
                v
            }
            None => {
                let mut v: Vec<String> = ACTIONS[action].keys.iter().map(|s| s.to_string()).collect();
                if !ACTIONS[action].seq.is_empty() {
                    v.push(seq_name(ACTIONS[action].seq));
                }
                v
            }
        }
    }

    pub fn keys_label(&self, action: usize) -> String {
        let v = self.keys_for(action);
        if v.is_empty() {
            "(unbound)".to_string()
        } else {
            v.join(" / ")
        }
    }

    /// Keys currently bound to `action` (single keys only).
    pub fn chords_for(&self, action: usize) -> Vec<Chord> {
        match &self.over[action] {
            Some(v) => v.clone(),
            None => default_keys(action),
        }
    }

    pub fn is_overridden(&self, action: usize) -> bool {
        self.over[action].is_some()
    }

    /// Bind exactly `keys` to `action`. Keys already used by another action are
    /// taken from it (the other action keeps its remaining keys). Returns a note
    /// for each action that lost a key.
    pub fn set(&mut self, action: usize, keys: Vec<Chord>) -> Vec<String> {
        let mut notes = Vec::new();
        for k in &keys {
            for j in 0..ACTIONS.len() {
                if j == action {
                    continue;
                }
                let current: Vec<Chord> = match &self.over[j] {
                    Some(v) => v.clone(),
                    None => default_keys(j),
                };
                if current.contains(k) {
                    let left: Vec<Chord> = current.into_iter().filter(|c| c != k).collect();
                    self.over[j] = Some(left);
                    notes.push(format!("'{}' taken from {}", chord_name(*k), ACTIONS[j].name));
                }
            }
        }
        self.over[action] = Some(keys);
        self.rebuild();
        notes
    }

    /// Back to the defaults for `action`.
    pub fn reset(&mut self, action: usize) {
        self.over[action] = None;
        self.rebuild();
    }

    pub fn reset_all(&mut self) {
        *self = Keymap::default();
    }

    /// Overridden actions as `(name, key names)`, ready to write to the file.
    pub fn entries(&self) -> Vec<(String, Vec<String>)> {
        self.over
            .iter()
            .enumerate()
            .filter_map(|(i, o)| {
                o.as_ref().map(|keys| {
                    (ACTIONS[i].name.to_string(), keys.iter().map(|k| chord_name(*k)).collect())
                })
            })
            .collect()
    }
}

/// Read `keybindings.json`. A missing file is normal; a broken one gives the
/// default keys plus a warning.
pub fn load(path: Option<&std::path::Path>) -> (Keymap, Vec<String>) {
    let Some(path) = path else {
        return (Keymap::default(), Vec::new());
    };
    match std::fs::read_to_string(path) {
        Ok(text) => match crate::config::parse_string_map(&text) {
            Ok(entries) => Keymap::from_entries(&entries),
            Err(e) => (
                Keymap::default(),
                vec![format!("{}: {e} (using default keys)", path.display())],
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Keymap::default(), Vec::new()),
        Err(e) => (
            Keymap::default(),
            vec![format!("{}: {e} (using default keys)", path.display())],
        ),
    }
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(s: &str) -> Chord {
        parse_chord(s).unwrap()
    }
    fn e(a: &str, k: &[&str]) -> (String, Vec<String>) {
        (a.to_string(), k.iter().map(|s| s.to_string()).collect())
    }
    fn idx(a: &str) -> usize {
        action_index(a).unwrap()
    }

    #[test]
    fn chords_parse_and_print() {
        assert_eq!(ch("j"), Chord::plain(Key::Char('j')));
        assert_eq!(ch("G"), Chord::plain(Key::Char('G')));
        assert_eq!(ch("Enter"), Chord::plain(Key::Enter));
        assert_eq!(ch("space"), Chord::plain(Key::Char(' ')));
        assert_eq!(ch("PgDn"), Chord::plain(Key::PageDown));
        assert_eq!(ch("ctrl+D"), Chord { key: Key::Char('d'), ctrl: true });
        assert_eq!(ch("Ctrl+x"), Chord { key: Key::Char('x'), ctrl: true });
        assert_eq!(ch("+"), Chord::plain(Key::Char('+')));
        assert_eq!(ch("ctrl++"), Chord { key: Key::Char('+'), ctrl: true });
        assert_eq!(ch(" > "), Chord::plain(Key::Char('>')));
        for bad in ["", "ctrl+", "ctrl+enter", "shift+a", "alt+x", "banana"] {
            assert!(parse_chord(bad).is_err(), "{bad}");
        }
        for s in ["j", "G", "enter", "space", "ctrl+d", "pagedown", "esc", "tab", "%", "ctrl++"] {
            assert_eq!(chord_name(ch(s)), s);
        }
    }

    #[test]
    fn table_is_consistent() {
        let mut owner: HashMap<Chord, &str> = HashMap::new();
        let mut names = HashSet::new();
        for a in ACTIONS {
            assert!(names.insert(a.name), "duplicate action {}", a.name);
            assert!(!canon(action_index(a.name).unwrap()).is_empty(), "{}", a.name);
            for k in a.keys {
                let c = parse_chord(k).unwrap_or_else(|e| panic!("{}: {e}", a.name));
                assert!(owner.insert(c, a.name).is_none(), "{k} used twice");
            }
            for part in a.seq.split_whitespace().chain(a.canon.split_whitespace()) {
                parse_chord(part).unwrap_or_else(|e| panic!("{}: {e}", a.name));
            }
            // every action must be reachable by default
            assert!(!a.keys.is_empty() || !a.seq.is_empty(), "{} has no default key", a.name);
            // the canonical sequence is one of the action's own defaults
            let cn = canon(action_index(a.name).unwrap());
            if a.seq.is_empty() {
                assert!(
                    cn.len() == 1 && default_keys(action_index(a.name).unwrap()).contains(&cn[0]),
                    "{}: canon must be one of its default keys",
                    a.name
                );
            } else {
                assert_eq!(cn, parse_seq(a.seq), "{}: canon must be the default sequence", a.name);
            }
        }
        // a multi-key default's first key must not also be a single-key default
        for a in ACTIONS.iter().filter(|a| !a.seq.is_empty()) {
            let first = parse_chord(a.seq.split_whitespace().next().unwrap()).unwrap();
            assert!(!owner.contains_key(&first), "{} prefix clashes", a.name);
        }
    }

    #[test]
    fn default_map_passes_everything() {
        let km = Keymap::default();
        assert!(km.is_default());
        for c in ["j", "q", "ctrl+d", "enter", "x"] {
            assert_eq!(km.translate(ch(c)), Tr::Pass);
        }
        assert_eq!(km.keys_label(idx("move_down")), "j / down");
        assert_eq!(km.keys_label(idx("first")), "home / gg");
        assert_eq!(km.keys_label(idx("next_diff")), "]c");
        assert!(km.entries().is_empty());
    }

    #[test]
    fn rebinding_replaces_single_key_defaults() {
        let (km, w) = Keymap::from_entries(&[e("move_down", &["n"]), e("quit", &["Q", "ctrl+q"])]);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(km.translate(ch("n")), Tr::Feed(vec![ch("j")]));
        assert_eq!(km.translate(ch("j")), Tr::Drop);
        assert_eq!(km.translate(ch("down")), Tr::Drop);
        assert_eq!(km.translate(ch("k")), Tr::Pass);
        assert_eq!(km.translate(ch("Q")), Tr::Feed(vec![ch("q")]));
        assert_eq!(km.translate(ch("ctrl+q")), Tr::Feed(vec![ch("q")]));
        assert_eq!(km.translate(ch("q")), Tr::Drop);
        assert_eq!(km.keys_label(idx("move_down")), "n");
    }

    #[test]
    fn multi_key_actions_keep_their_default() {
        let (km, w) = Keymap::from_entries(&[
            e("first", &["F"]),
            e("expand_all", &["X"]),
            e("next_diff", &["n", "N"]),
        ]);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(km.translate(ch("F")), Tr::Feed(vec![ch("g"), ch("g")]));
        assert_eq!(km.translate(ch("X")), Tr::Feed(vec![ch("z"), ch("R")]));
        assert_eq!(km.translate(ch("n")), Tr::Feed(vec![ch("]"), ch("c")]));
        // `first` also had a plain `home` default which is replaced
        assert_eq!(km.translate(ch("home")), Tr::Drop);
        // the prefix keys themselves are untouched
        assert_eq!(km.translate(ch("g")), Tr::Pass);
        assert_eq!(km.translate(ch("z")), Tr::Pass);
        assert_eq!(km.translate(ch("]")), Tr::Pass);
        assert_eq!(km.keys_label(idx("first")), "F / gg");
    }

    #[test]
    fn unbinding_with_an_empty_list() {
        let (km, w) = Keymap::from_entries(&[e("delete", &[])]);
        assert!(w.is_empty());
        assert_eq!(km.translate(ch("d")), Tr::Drop);
        assert_eq!(km.keys_label(idx("delete")), "(unbound)");
        assert_eq!(km.entries(), vec![e("delete", &[])]);
    }

    #[test]
    fn bad_entries_warn_and_keep_defaults() {
        let (km, w) = Keymap::from_entries(&[
            e("nope", &["x"]),
            e("move_up", &["banana"]),
            e("refresh", &["ctrl+c"]),
            e("sort", &["z", "shift+a"]),
        ]);
        assert!(w.iter().any(|m| m.contains("unknown action 'nope'")), "{w:?}");
        assert!(w.iter().any(|m| m.contains("unknown key 'banana'")));
        assert!(w.iter().any(|m| m.contains("ctrl+c always quits")));
        // fully invalid entries leave the action on its defaults
        assert_eq!(km.translate(ch("k")), Tr::Pass);
        assert_eq!(km.translate(ch("r")), Tr::Pass);
        // partly valid entries apply the valid keys
        assert_eq!(km.translate(ch("z")), Tr::Feed(vec![ch("s")]));
        assert_eq!(km.translate(ch("s")), Tr::Drop);
    }

    #[test]
    fn conflicts_are_reported() {
        let (km, w) = Keymap::from_entries(&[e("move_down", &["x"]), e("move_up", &["x"])]);
        assert!(w.iter().any(|m| m.contains("'x' is already bound to move_down")), "{w:?}");
        assert_eq!(km.translate(ch("x")), Tr::Feed(vec![ch("j")])); // first wins
        // shadowing another action's default key
        let (_, w) = Keymap::from_entries(&[e("move_down", &["q"])]);
        assert!(w.iter().any(|m| m.contains("'q' now runs move_down instead of quit")), "{w:?}");
    }

    #[test]
    fn editor_set_steals_and_resets() {
        let mut km = Keymap::default();
        let notes = km.set(idx("copy_right"), vec![ch("l")]);
        assert_eq!(notes, vec!["'l' taken from expand".to_string()]);
        assert_eq!(km.translate(ch("l")), Tr::Feed(vec![ch(">")]));
        assert_eq!(km.translate(ch(">")), Tr::Drop);
        // expand lost `l` but kept `right`
        assert_eq!(km.keys_for(idx("expand")), vec!["right".to_string()]);
        // `right` is now a user binding that runs the handler's own `l`
        assert_eq!(km.translate(ch("right")), Tr::Feed(vec![ch("l")]));
        // entries round trip through the file format
        let (km2, w) = Keymap::from_entries(&km.entries());
        assert!(w.is_empty(), "{w:?}");
        for c in ["l", ">", "right", "j"] {
            assert_eq!(km2.translate(ch(c)), km.translate(ch(c)), "{c}");
        }
        km.reset(idx("copy_right"));
        assert_eq!(km.translate(ch(">")), Tr::Pass);
        km.reset_all();
        assert!(km.is_default());
        assert_eq!(km.translate(ch("l")), Tr::Pass);
    }

    #[test]
    fn binding_a_prefix_key_warns() {
        let (_, w) = Keymap::from_entries(&[e("move_down", &["z"])]);
        assert!(w.iter().any(|m| m.contains("'z' starts the default sequence 'zR'")), "{w:?}");
        assert!(w.iter().any(|m| m.contains("'zM'")) && w.iter().any(|m| m.contains("'zi'")));
        let (_, w) = Keymap::from_entries(&[e("move_down", &["]"])]);
        assert!(w.iter().any(|m| m.contains("']c'")), "{w:?}");
        let (_, w) = Keymap::from_entries(&[e("move_down", &["n"])]);
        assert!(w.is_empty());
    }

    #[test]
    fn loading_the_file() {
        let dir = std::env::temp_dir().join(format!("dirrcmp-keys-{}-{}", std::process::id(), crate::utmp::next()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // no path / missing file: defaults, no warning
        let (km, w) = load(None);
        assert!(km.is_default() && w.is_empty());
        let (km, w) = load(Some(&dir.join("nope.json")));
        assert!(km.is_default() && w.is_empty());
        // valid
        std::fs::write(dir.join("k.json"), r#"{"quit": "Q", "move_down": ["n", "down"]}"#).unwrap();
        let (km, w) = load(Some(&dir.join("k.json")));
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(km.translate(ch("Q")), Tr::Feed(vec![ch("q")]));
        assert_eq!(km.translate(ch("down")), Tr::Feed(vec![ch("j")]));
        // broken JSON: defaults and one warning naming the file
        std::fs::write(dir.join("bad.json"), "{ nope").unwrap();
        let (km, w) = load(Some(&dir.join("bad.json")));
        assert!(km.is_default());
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("bad.json") && w[0].contains("using default keys"), "{w:?}");
        // wrong value type
        std::fs::write(dir.join("t.json"), r#"{"quit": 5}"#).unwrap();
        let (km, w) = load(Some(&dir.join("t.json")));
        assert!(km.is_default() && w.len() == 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rebinding_every_action_to_itself_is_harmless() {
        let entries: Vec<_> = ACTIONS
            .iter()
            .map(|a| (a.name.to_string(), a.keys.iter().map(|s| s.to_string()).collect::<Vec<_>>()))
            .filter(|(_, k)| !k.is_empty())
            .collect();
        let (km, w) = Keymap::from_entries(&entries);
        assert!(w.is_empty(), "{w:?}");
        for a in ACTIONS {
            for k in a.keys {
                let c = ch(k);
                assert_eq!(km.translate(c), Tr::Feed(canon(action_index(a.name).unwrap())), "{k}");
            }
        }
    }
}
