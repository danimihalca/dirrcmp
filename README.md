# dircmp (Rust)

A terminal UI for comparing two directories side by side, with a side-by-side
file editor. A Rust rewrite of [ilyasturki/dircmp](https://github.com/ilyasturki/dircmp).
Single binary, Linux / macOS / Windows, no VCS needed.

## Build

    cargo build --release        # Rust 1.75+; the only dependency is ratatui 0.29
    cargo test                   # engine, CLI, config, undo, side-by-side and TUI logic
    ./target/release/dircmp LEFT RIGHT

## Usage

    dircmp LEFT RIGHT                  interactive TUI (two directories)
    dircmp FILE1 FILE2                 side-by-side editor (unified diff when not a terminal)
    dircmp diff  LEFT RIGHT            print differences (--format tree|flat|json, --filter, --stat)
    dircmp check LEFT RIGHT            silent, exit 0 if identical / 1 if different (CI)

    --no-ignore  --ignore PATTERN (repeatable)  --follow-symlinks  --dates  --quick  --no-syntax
    --diff-cmd "nvim -d"   (or env DIRCMP_DIFF)  external diff for Enter in the TUI

Ignored by default: `.git`, `node_modules`, `.DS_Store`. More patterns come from
`$XDG_DATA_HOME/dircmp/ignore` (default `~/.local/share/dircmp/ignore`) and, per pair of
directories, from `pairs/<id>.ignore` next to it (written by `i` / `I` in the TUI; also
applied by `diff` and `check`). Syntax is gitignore-like: `*`, `?`, `**`, a leading or inner
`/` anchors to the root, a trailing `/` matches directories only, `!` negates.

Preferences: `$XDG_CONFIG_HOME/dircmp/config.json` (default `~/.config/dircmp/config.json`) with
`compareDates`, `compareContents`, `dirsFirst`, `showHints`, `nerdFont`, `syntaxHighlight`, `chunkBackground`, `diffCommand`. Press `,` in
the app to change them live; each change is saved. Key bindings: `keybindings.json` next to it
(see below), or press `K`.

## Directory view (press `?` in the app)

| Keys | Action |
|---|---|
| `j k` / arrows, `gg` `G`, `^d ^u ^f ^b`, `^e ^y` | move / scroll |
| `Tab` `%` / `H` `L` | switch panel focus |
| `l` / `h` | expand or open file / collapse or go to parent |
| `Enter` | side-by-side editor on a file (dirs: toggle) |
| `D` | read-only unified diff |
| `zR` `zM` | expand / collapse all |
| `]c` `[c` | next / previous difference |
| `>` `<` | copy entry to right / left |
| `Space` | copy the focused side across |
| `d` | delete on the focused side (confirms) |
| `u` `U` | undo / redo copy and delete (this session) |
| `m` `M` | pair / unpair a renamed file or directory |
| `y` | yank path to clipboard (OSC 52) |
| `e` | open in `$VISUAL` / `$EDITOR` |
| `/` `f` | filter by name / cycle status filter |
| `i` `I` `zi` | ignore entry (saved per pair) / manage patterns / toggle ignoring |
| `s` `S` | cycle sort (name/size/date/status) / swap panels |
| `.` | actions menu for the selected entry (shows each action's key) |
| `,` | preferences (toggle options live, saved to `config.json`) |
| `K` | key binding editor |
| `r` `?` `q` | refresh / help / quit |

Colors: yellow = modified, green = only on this side, red = missing on this side, dim = identical.

**Pairing renamed files and directories.** Press `m` on a file or directory that exists on one
side only, `Tab` to the other panel, and press `m` on the renamed one there (same kind, same
parent). They are then compared, copied and diffed as one entry, each panel showing its own
name. `M` (or `m` again on the marked entry, or Esc) cancels a pending mark; `M` on a paired entry
removes the pairing. Pairings last for the session.

## Key bindings

Every directory-view action can be rebound, in the editor (`K`: `Enter` rebind, `a` add a key,
`x` reset, `R` reset all; changes apply at once and are saved) or by writing
`keybindings.json` yourself. Keys are a character (`"n"`, `"G"`), `"ctrl+x"`, or one of `enter`,
`esc`, `tab`, `space`, `backspace`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`,
`pagedown`. A value is one key or a list; an empty list unbinds the action.

    { "move_down": ["n", "down"], "quit": "Q", "copy_right": ["ctrl+l"] }

Action names are listed in the editor's order (`move_down`, `move_up`, `expand`, `open`,
`copy_right`, `delete`, `undo`, `filter`, `preferences`, `quit`, ...; `src/keys.rs` has the table).

- Binding keys to an action replaces its single-key defaults (`j` and `Down` for `move_down`).
- The multi-key defaults `gg`, `zR`, `zM`, `zi`, `]c`, `[c` always keep working; binding a key
  to one of those actions adds a one-key shortcut. Binding a key that starts such a sequence
  (`g`, `z`, `[`, `]`) breaks it, and a warning says so.
- Ctrl-C always quits. Problems in the file (unknown action, bad key, a key used twice, a key
  that shadows another action's default) are shown on start; valid entries still apply.
- Only the directory view is configurable. The side-by-side editor keys are fixed.

## Side-by-side editor (`Enter` on a file, or `dircmp FILE1 FILE2`)

Aligned panes with intra-line highlighting; changes are yellow, lines only on one side green,
gaps red. Unchanged stretches can be folded.

| Keys | Action |
|---|---|
| `n N` / `]c [c` | next / previous change |
| `>` `<` | copy the change under the cursor left to right / right to left |
| `.` `,` | copy just this line left to right / right to left |
| `dp` `do` | put the change from the active pane / obtain it into the active pane |
| `e` / `Enter`, `o`, `dd` | edit line (on a gap: add one) / add a line below / delete line |
| `u` `^r` | undo / redo |
| `w` `^s` | save the changed sides (a side that does not exist yet is created) |
| `Tab` `H` `L`, `f`, `h l`, `0` | switch pane / fold unchanged / scroll sideways / reset |
| `x` | syntax highlighting on / off for this view |
| `B` | background tint of changed rows on / off for this view |
| `q` `Esc` | close (asks about unsaved edits) |

Line endings (LF/CRLF) and a missing final newline are preserved when saving. Binary,
non-UTF-8 and files over 8 MiB fall back to a read-only note.

## Chunk background

Changed rows get a background tint across the whole row, in both the side-by-side editor and
the unified view: yellowish = modified, green = only on this side, red = missing on this side
(the gap), and a brighter shade for the change under the cursor. It uses 256-colour palette
entries, so it works in any modern terminal. Toggle with `B` (one view) or `,` → *Chunk
background colours* (saved as `chunkBackground`); on by default.

## Syntax highlighting

Both the side-by-side editor and the unified `D` view are highlighted. The file is tokenized
from the top, so multi-line comments and strings are right even when only a few lines are on
screen, and the colours follow edits and undo. In the side-by-side view the change colour
(yellow / green / red) moves to the gutter bar so it does not fight with the code colours; the
changed part of a modified line is still bold and underlined. The status bar shows the language.
Switch it off with `x` (one view), `,` → *Syntax highlighting* (saved), or `--no-syntax` (one run).

The built-in highlighter needs no dependency. It is a small table-driven tokenizer, not a
parser: keywords, types, strings (incl. raw / triple-quoted / multi-line), comments (nested where
the language nests them), numbers, constants, calls, attributes / preprocessor / decorators,
shell and make variables, and keys. Languages: Rust, C, C++, C#, Java, Kotlin, Go, JavaScript,
TypeScript, Python, Ruby, Lua, shell, Makefile, BitBake, Dockerfile, CMake, SQL, JSON, TOML,
INI / `.conf` / `.service`, Kconfig, YAML, Nix, Devicetree (`.dts`, `.overlay`), CSS / SCSS,
HTML / XML, Markdown. A language is picked from the file name or extension, or a `#!` line.
Files over about 3 MB and lines over 4000 characters are left plain.

For more languages build with the optional [syntect](https://crates.io/crates/syntect) crate
(Sublime syntax definitions, pure-Rust regex engine; adds compile time and a few MB):

    cargo build --release --features syntect

syntect is tried first and the built-in highlighter takes anything syntect does not know
(TOML, TypeScript, Kconfig, BitBake, Devicetree...). Only syntect's parser is used, its scopes
are mapped onto the same colours. That feature module (`src/syn_syntect.rs`) could not be
compiled in the environment this was written in, so treat it as untested.

## Differences from the original

- Not implemented: remote targets via rclone (nothing here can be tested without it), and a
  modification-date column in the panels (sorting by date works; `dateLocale` is ignored).
- Files are compared byte for byte (size first, then a streaming compare that stops at the
  first difference) instead of by SHA-256. `--dates` also treats a differing mtime (more than
  2 s apart) as a difference; `--quick` trusts size + mtime and reads nothing.
- Copying a directory merges into the destination; nothing extra is deleted there. Copies keep
  file mtimes and skip ignored entries. Overwrites and deletes ask first.
- Undo works only within a session: deletes move the entry to a private backup folder in the
  system temp dir, overwrites snapshot what they replace, and everything is removed on exit.
  A snapshot of an overwritten directory temporarily needs that much free disk space.
- Diff: line-based Myers, files up to 8 MiB; beyond ~2000 edits in a region it reports the
  region as replaced rather than computing a minimal diff.

## Layout

    src/engine.rs   scan / compare / pairing / ignore / diff / merge model (std only, tested)
    src/cli.rs      arguments, tree|flat|json|stat output, per-pair ignore files (std only)
    src/config.rs   config.json / keybindings.json reader and writer (std only)
    src/keys.rs     key chords, action table, user key bindings (std only)
    src/undo.rs     undo / redo journal for copy and delete (std only)
    src/highlight.rs  syntax highlighting: language table + tokenizer (std only, tested)
    src/syn_syntect.rs  optional syntect backend (feature `syntect`, untested)
    src/side.rs     side-by-side editor view
    src/tui.rs      directory view
    src/main.rs     wiring
