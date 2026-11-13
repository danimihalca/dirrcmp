//! Preferences from `config.json` (same file and keys as the original dircmp).
//!
//! Location: `$XDG_CONFIG_HOME/dircmp/config.json`, else `~/.config/dircmp/config.json`,
//! else `%APPDATA%\dircmp\config.json` on Windows. Std only: a small JSON reader
//! that understands the flat object this file uses and skips anything else.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// `compareDates`: a differing modification time counts as a difference.
    pub compare_dates: bool,
    /// `compareContents`: compare file contents (default). When false, files of
    /// equal size and modification time are assumed identical (fast, no reads).
    pub compare_contents: bool,
    /// `dirsFirst`: list directories before files.
    pub dirs_first: bool,
    /// `showHints`: show the key hints in the status bar.
    pub show_hints: bool,
    /// `nerdFont`: use Nerd Font icons instead of ASCII markers.
    pub nerd_font: bool,
    /// `diffCommand`: external diff for Enter on a file, e.g. `nvim -d`.
    pub diff_command: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            compare_dates: false,
            compare_contents: true,
            dirs_first: true,
            show_hints: true,
            nerd_font: false,
            diff_command: None,
        }
    }
}

pub fn config_path() -> Option<PathBuf> {
    if let Some(x) = env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(x).join("dircmp").join("config.json"));
    }
    if let Some(h) = env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(h).join(".config").join("dircmp").join("config.json"));
    }
    env::var_os("APPDATA")
        .filter(|v| !v.is_empty())
        .map(|a| PathBuf::from(a).join("dircmp").join("config.json"))
}

/// Load the config. A missing file is normal (defaults, no warning). A broken
/// file yields defaults plus a warning to show the user.
pub fn load() -> (Config, Option<String>) {
    match config_path() {
        Some(p) => load_from(&p),
        None => (Config::default(), None),
    }
}

pub fn load_from(path: &Path) -> (Config, Option<String>) {
    match fs::read_to_string(path) {
        Ok(text) => match parse(&text) {
            Ok(c) => (c, None),
            Err(e) => (Config::default(), Some(format!("{}: {e} (using defaults)", path.display()))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), None),
        Err(e) => (Config::default(), Some(format!("{}: {e} (using defaults)", path.display()))),
    }
}

// ───────────────────────────── parsing ─────────────────────────────

enum Val {
    Str(String),
    /// An array whose elements are all strings.
    Strs(Vec<String>),
    Bool(bool),
    Other,
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn err<T>(&self, msg: &str) -> Result<T, String> {
        // report a 1-based line number
        let line = self.s[..self.i.min(self.s.len())].iter().filter(|&&b| b == b'\n').count() + 1;
        Err(format!("invalid JSON at line {line}: {msg}"))
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        self.ws();
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            self.err(&format!("expected '{}'", c as char))
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        if self.i + 4 > self.s.len() {
            return self.err("short \\u escape");
        }
        let h = std::str::from_utf8(&self.s[self.i..self.i + 4]).unwrap_or("");
        match u32::from_str_radix(h, 16) {
            Ok(v) => {
                self.i += 4;
                Ok(v)
            }
            Err(_) => self.err("bad \\u escape"),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(b) = self.peek() else {
                return self.err("unterminated string");
            };
            self.i += 1;
            match b {
                b'"' => break,
                b'\\' => {
                    let Some(e) = self.peek() else {
                        return self.err("unterminated string");
                    };
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp)
                                && self.s[self.i..].starts_with(b"\\u")
                            {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                } else {
                                    self.i = save;
                                }
                            }
                            char::from_u32(cp).unwrap_or('\u{fffd}')
                        }
                        _ => return self.err("bad escape"),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                b if b < 0x20 => return self.err("control character in string"),
                b => out.push(b),
            }
        }
        String::from_utf8(out).or_else(|_| self.err("invalid UTF-8"))
    }

    fn literal(&mut self, word: &str) -> Result<(), String> {
        if self.s[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(())
        } else {
            self.err("unexpected token")
        }
    }

    fn value(&mut self, depth: usize) -> Result<Val, String> {
        if depth > 32 {
            return self.err("nesting too deep");
        }
        self.ws();
        match self.peek() {
            Some(b'"') => Ok(Val::Str(self.string()?)),
            Some(b't') => self.literal("true").map(|_| Val::Bool(true)),
            Some(b'f') => self.literal("false").map(|_| Val::Bool(false)),
            Some(b'n') => self.literal("null").map(|_| Val::Other),
            Some(b'{') => {
                self.i += 1;
                self.ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                    return Ok(Val::Other);
                }
                loop {
                    self.ws();
                    self.string()?;
                    self.expect(b':')?;
                    self.value(depth + 1)?;
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Val::Other);
                        }
                        _ => return self.err("expected ',' or '}'"),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    return Ok(Val::Strs(Vec::new()));
                }
                let mut strs: Option<Vec<String>> = Some(Vec::new());
                loop {
                    match self.value(depth + 1)? {
                        Val::Str(s) => {
                            if let Some(v) = strs.as_mut() {
                                v.push(s);
                            }
                        }
                        _ => strs = None,
                    }
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(match strs {
                                Some(v) => Val::Strs(v),
                                None => Val::Other,
                            });
                        }
                        _ => return self.err("expected ',' or ']'"),
                    }
                }
            }
            Some(b'-') | Some(b'0'..=b'9') => {
                while matches!(self.peek(), Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
                    self.i += 1;
                }
                Ok(Val::Other)
            }
            _ => self.err("expected a value"),
        }
    }
}

pub fn parse(text: &str) -> Result<Config, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut p = P { s: text.as_bytes(), i: 0 };
    let mut cfg = Config::default();
    p.ws();
    if p.i >= p.s.len() {
        return Ok(cfg); // an empty file is fine
    }
    p.expect(b'{')?;
    p.ws();
    if p.peek() == Some(b'}') {
        p.i += 1;
    } else {
        loop {
            p.ws();
            let key = p.string()?;
            p.expect(b':')?;
            let val = p.value(0)?;
            let want_bool = |v: Val| -> Result<bool, String> {
                match v {
                    Val::Bool(b) => Ok(b),
                    _ => Err(format!("'{key}' must be true or false")),
                }
            };
            match key.as_str() {
                "compareDates" => cfg.compare_dates = want_bool(val)?,
                "compareContents" => cfg.compare_contents = want_bool(val)?,
                "dirsFirst" => cfg.dirs_first = want_bool(val)?,
                "showHints" => cfg.show_hints = want_bool(val)?,
                "nerdFont" => cfg.nerd_font = want_bool(val)?,
                "diffCommand" => match val {
                    Val::Str(s) => cfg.diff_command = if s.trim().is_empty() { None } else { Some(s) },
                    Val::Other => cfg.diff_command = None, // null
                    Val::Bool(_) | Val::Strs(_) => {
                        return Err("'diffCommand' must be a string".to_string())
                    }
                },
                _ => {} // dateLocale and anything unknown
            }
            p.ws();
            match p.peek() {
                Some(b',') => p.i += 1,
                Some(b'}') => {
                    p.i += 1;
                    break;
                }
                _ => return p.err("expected ',' or '}'"),
            }
        }
    }
    p.ws();
    if p.i < p.s.len() {
        return p.err("unexpected text after the closing '}'");
    }
    Ok(cfg)
}

// ───────────────────────────── writing ─────────────────────────────

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Serialise to the same JSON `parse` reads.
pub fn to_json(c: &Config) -> String {
    let mut s = String::from("{\n");
    s.push_str(&format!("  \"compareDates\": {},\n", c.compare_dates));
    s.push_str(&format!("  \"compareContents\": {},\n", c.compare_contents));
    s.push_str(&format!("  \"dirsFirst\": {},\n", c.dirs_first));
    s.push_str(&format!("  \"showHints\": {},\n", c.show_hints));
    s.push_str(&format!("  \"nerdFont\": {},\n", c.nerd_font));
    match &c.diff_command {
        Some(d) => s.push_str(&format!("  \"diffCommand\": {}\n", json_str(d))),
        None => s.push_str("  \"diffCommand\": null\n"),
    }
    s.push_str("}\n");
    s
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn save_to(path: &Path, c: &Config) -> Result<(), String> {
    write_file(path, &to_json(c))
}

// ───────────────────────────── key bindings file ─────────────────────────────

/// `keybindings.json` lives next to `config.json`.
pub fn keybindings_path() -> Option<PathBuf> {
    config_path().map(|p| p.with_file_name("keybindings.json"))
}

/// Parse `{ "action": "key" | ["key", ...], ... }` into (action, keys) pairs in
/// file order. Anything else is an error.
pub fn parse_string_map(text: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut p = P { s: text.as_bytes(), i: 0 };
    let mut out = Vec::new();
    p.ws();
    if p.i >= p.s.len() {
        return Ok(out);
    }
    p.expect(b'{')?;
    p.ws();
    if p.peek() == Some(b'}') {
        p.i += 1;
    } else {
        loop {
            p.ws();
            let key = p.string()?;
            p.expect(b':')?;
            match p.value(0)? {
                Val::Str(s) => out.push((key, vec![s])),
                Val::Strs(v) => out.push((key, v)),
                _ => return Err(format!("'{key}' must be a key or a list of keys")),
            }
            p.ws();
            match p.peek() {
                Some(b',') => p.i += 1,
                Some(b'}') => {
                    p.i += 1;
                    break;
                }
                _ => return p.err("expected ',' or '}'"),
            }
        }
    }
    p.ws();
    if p.i < p.s.len() {
        return p.err("unexpected text after the closing '}'");
    }
    Ok(out)
}

/// Write `{ "action": ["key", ...] }`, one action per line.
pub fn string_map_to_json(entries: &[(String, Vec<String>)]) -> String {
    if entries.is_empty() {
        return "{}\n".to_string();
    }
    let mut s = String::from("{\n");
    for (i, (k, v)) in entries.iter().enumerate() {
        let list: Vec<String> = v.iter().map(|x| json_str(x)).collect();
        s.push_str(&format!(
            "  {}: [{}]{}\n",
            json_str(k),
            list.join(", "),
            if i + 1 < entries.len() { "," } else { "" }
        ));
    }
    s.push_str("}\n");
    s
}

pub fn save_string_map(path: &Path, entries: &[(String, Vec<String>)]) -> Result<(), String> {
    write_file(path, &string_map_to_json(entries))
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_empty() {
        let d = Config::default();
        assert!(d.compare_contents && d.dirs_first && d.show_hints);
        assert!(!d.compare_dates && !d.nerd_font && d.diff_command.is_none());
        assert_eq!(parse("").unwrap(), d);
        assert_eq!(parse("  \n").unwrap(), d);
        assert_eq!(parse("{}").unwrap(), d);
        assert_eq!(parse("{ }").unwrap(), d);
    }

    #[test]
    fn all_keys() {
        let c = parse(
            r#"{
              "dateLocale": "en-GB",
              "showHints": false,
              "compareDates": true,
              "compareContents": false,
              "nerdFont": true,
              "dirsFirst": false,
              "diffCommand": "nvim -d"
            }"#,
        )
        .unwrap();
        assert!(!c.show_hints && c.compare_dates && !c.compare_contents);
        assert!(c.nerd_font && !c.dirs_first);
        assert_eq!(c.diff_command.as_deref(), Some("nvim -d"));
    }

    #[test]
    fn unknown_values_are_skipped() {
        let c = parse(
            r#"{"x": {"a": [1, 2.5e3, -3, {"b": null}], "c": "}"}, "y": [], "z": {}, "nerdFont": true, "n": null}"#,
        )
        .unwrap();
        assert!(c.nerd_font);
        assert_eq!(parse(r#"{"diffCommand": null}"#).unwrap().diff_command, None);
        assert_eq!(parse(r#"{"diffCommand": "  "}"#).unwrap().diff_command, None);
    }

    #[test]
    fn string_escapes() {
        let c = parse(r#"{"diffCommand": "a\"b\\c\/d\n\t é 😀 end"}"#).unwrap();
        assert_eq!(c.diff_command.as_deref(), Some("a\"b\\c/d\n\t é 😀 end"));
        // a lone surrogate becomes the replacement character instead of failing
        let c = parse(r#"{"diffCommand": "x\ud83dy"}"#).unwrap();
        assert_eq!(c.diff_command.as_deref(), Some("x\u{fffd}y"));
        // BOM tolerated
        assert!(parse("\u{feff}{\"nerdFont\": true}").unwrap().nerd_font);
    }

    #[test]
    fn errors_are_reported() {
        for bad in [
            "[]",
            "{",
            r#"{"a" 1}"#,
            r#"{"a": }"#,
            r#"{"a": tru}"#,
            r#"{"a": 1,}"#,
            r#"{"a": "unterminated}"#,
            r#"{"a": 1} trailing"#,
            r#"{"a": "bad \q escape"}"#,
            r#"{"nerdFont": "yes"}"#,
            r#"{"diffCommand": true}"#,
            r#"{"compareDates": 1}"#,
        ] {
            assert!(parse(bad).is_err(), "should fail: {bad}");
        }
        let e = parse("{\n\"a\": 1,\n\"b\" 2}").unwrap_err();
        assert!(e.contains("line 3"), "{e}");
    }

    #[test]
    fn deep_nesting_is_rejected_not_crashed() {
        let deep = format!("{{\"x\": {}{}}}", "[".repeat(100), "]".repeat(100));
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn file_loading() {
        let dir = std::env::temp_dir().join(format!("dircmp-cfg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // missing: defaults, no warning
        let (c, w) = load_from(&dir.join("nope.json"));
        assert_eq!(c, Config::default());
        assert!(w.is_none());
        // good
        fs::write(dir.join("ok.json"), r#"{"nerdFont": true}"#).unwrap();
        let (c, w) = load_from(&dir.join("ok.json"));
        assert!(c.nerd_font && w.is_none());
        // broken: defaults and a warning that names the file
        fs::write(dir.join("bad.json"), "{oops").unwrap();
        let (c, w) = load_from(&dir.join("bad.json"));
        assert_eq!(c, Config::default());
        let w = w.unwrap();
        assert!(w.contains("bad.json") && w.contains("using defaults"), "{w}");
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod save_tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let mut c = Config::default();
        c.compare_dates = true;
        c.nerd_font = true;
        c.dirs_first = false;
        c.diff_command = Some("nvim -d \"x\"\\y".to_string());
        let back = parse(&to_json(&c)).unwrap();
        assert!(back.compare_dates && back.nerd_font && !back.dirs_first && back.compare_contents);
        assert_eq!(back.diff_command.as_deref(), Some("nvim -d \"x\"\\y"));
        let d = parse(&to_json(&Config::default())).unwrap();
        assert_eq!(d.diff_command, None);
        assert!(d.show_hints);
    }

    #[test]
    fn save_creates_directories() {
        let dir = std::env::temp_dir().join(format!("dircmp-cfg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let p = dir.join("a/b/config.json");
        save_to(&p, &Config::default()).unwrap();
        let (c, w) = load_from(&p);
        assert!(w.is_none() && c.show_hints);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn string_maps() {
        let m = parse_string_map(r#"{"quit": "Q", "move_down": ["j", "down"], "x": []}"#).unwrap();
        assert_eq!(m[0], ("quit".to_string(), vec!["Q".to_string()]));
        assert_eq!(m[1].1, vec!["j".to_string(), "down".to_string()]);
        assert!(m[2].1.is_empty());
        assert!(parse_string_map("").unwrap().is_empty());
        assert!(parse_string_map("{}").unwrap().is_empty());
        assert!(parse_string_map(r#"{"a": 1}"#).is_err());
        assert!(parse_string_map(r#"{"a": [1, "b"]}"#).is_err());
        assert!(parse_string_map(r#"{"a": true}"#).is_err());
        assert!(parse_string_map(r#"{"a": "b"} x"#).is_err());
        assert!(parse_string_map("[").is_err());
        let back = parse_string_map(&string_map_to_json(&m)).unwrap();
        assert_eq!(back, m);
        assert_eq!(string_map_to_json(&[]), "{}\n");
    }

    #[test]
    fn diff_command_array_is_an_error() {
        assert!(parse(r#"{"diffCommand": ["a"]}"#).is_err());
        assert!(parse(r#"{"unknown": ["a", 1]}"#).is_ok());
    }
}
