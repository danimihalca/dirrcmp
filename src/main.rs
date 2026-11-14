mod cli;
mod config;
mod engine;
mod fuzzy;
mod git;
mod highlight;
mod keys;
mod renames;
mod side;
mod smooth;
#[cfg(feature = "syntect")]
mod syn_syntect;
#[cfg(test)]
mod utmp;
mod undo;
mod tui;

use std::io::IsTerminal;
use std::process;

use cli::Command;
use engine::{Ignore, Opts, SortKey, Status};

fn main() {
    process::exit(run());
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let c = match cli::parse_args(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dirrcmp: {e}\nTry 'dirrcmp --help'.");
            return 2;
        }
    };
    match c.cmd {
        Command::Help => {
            print!("{}", cli::USAGE);
            return 0;
        }
        Command::Version => {
            println!("dirrcmp {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        _ => {}
    }
    // `--git`: the two sides are temporary exports of revisions (and the working tree).
    // The session removes the exports when it is dropped at the end of `run`.
    let session = match &c.git {
        Some(g) => match git::prepare(g) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("dirrcmp: git: {e}");
                return 2;
            }
        },
        None => None,
    };
    let (left, right) = match (&session, &c.left, &c.right) {
        (Some(s), _, _) => (s.left.clone(), s.right.clone()),
        (None, Some(l), Some(r)) => (l.clone(), r.clone()),
        _ => return 2, // parse_args guarantees both paths for these commands
    };

    // Two files: side-by-side editor on a terminal, otherwise a plain unified
    // diff like `diff -u` (exit 1 if different).
    if session.is_none() && left.is_file() && right.is_file() {
        if matches!(c.cmd, Command::Tui) && std::io::stdout().is_terminal() {
            let prefs = config::load().0;
            let syntax = prefs.syntax && !c.no_syntax;
            match tui::run_files(left.clone(), right.clone(), syntax, prefs.smooth) {
                Ok(None) => return 0,
                Ok(Some(fd)) => {
                    print!("{}", cli::render_file_diff(&fd, &left, &right));
                    return 1;
                }
                Err(e) => {
                    eprintln!("dirrcmp: {e}");
                    return 1;
                }
            }
        }
        let fd = engine::file_diff(Some(&left), Some(&right));
        print!("{}", cli::render_file_diff(&fd, &left, &right));
        return if matches!(fd, engine::FileDiff::Identical) { 0 } else { 1 };
    }
    if !left.is_dir() || !right.is_dir() {
        eprintln!(
            "dirrcmp: both arguments must be directories (or both files); \
             remote targets are not supported"
        );
        return 2;
    }

    let (cfg, warning) = config::load();
    if let Some(w) = warning {
        eprintln!("dirrcmp: warning: {w}");
    }
    let mut ignore_all = cli::build_ignore_full(&c);
    if let Some(s) = &session {
        for p in &s.ignores {
            ignore_all.add(p);
        }
    }
    // Patterns saved for this pair of directories (TUI `i` / `I`) apply everywhere.
    let pair_file = if session.is_some() { None } else { cli::pair_ignore_path(&left, &right) };
    let mut effective = ignore_all.clone();
    if let Some(pf) = &pair_file {
        for l in cli::read_pair_ignore(pf) {
            effective.add(&l);
        }
    }
    let opts = Opts {
        ignore: if c.no_ignore { Ignore::new() } else { effective },
        follow_symlinks: c.follow_symlinks,
        compare_dates: c.compare_dates || cfg.compare_dates,
        compare_contents: cfg.compare_contents && !c.quick,
        pairs: Vec::new(),
        detect_renames: cfg.renames && !c.no_renames,
    };

    match c.cmd {
        Command::Tui => {
            if !std::io::stdout().is_terminal() {
                eprintln!("dirrcmp: stdout is not a terminal; use 'dirrcmp diff' or 'dirrcmp check'");
                return 2;
            }
            let keys_file = config::keybindings_path();
            let (keymap, key_warnings) = keys::load(keys_file.as_deref());
            let tui_cfg = tui::Config {
                left,
                right,
                opts,
                ignore_all,
                ignore_on: !c.no_ignore,
                prefs: cfg,
                cli_dates: c.compare_dates,
                cli_quick: c.quick,
                cli_diff: c.diff_cmd,
                cli_no_syntax: c.no_syntax,
                cli_no_renames: c.no_renames,
                readonly: session.as_ref().map_or((false, false), |s| s.readonly),
                labels: session.as_ref().map(|s| s.labels.clone()),
                keymap,
                key_warnings,
                config_file: config::config_path(),
                keys_file,
                pair_ignore: pair_file,
            };
            match tui::run(tui_cfg) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("dirrcmp: {e}");
                    1
                }
            }
        }
        Command::Diff { format, filter, stat } => {
            let mut root = engine::compare(&left, &right, &opts);
            engine::sort_tree(&mut root, SortKey::Name, cfg.dirs_first);
            let out = match format {
                cli::Format::Tree => cli::render_tree(&root, &filter),
                cli::Format::Flat => cli::render_flat(&root, &filter),
                cli::Format::Json => cli::render_json(&root, &filter),
            };
            print!("{out}");
            if stat {
                // keep JSON on stdout valid: put the summary on stderr there
                let s = cli::render_stat(&root);
                if format == cli::Format::Json {
                    eprint!("{s}");
                } else {
                    print!("{s}");
                }
            }
            0
        }
        Command::Check { stat } => {
            let root = engine::compare(&left, &right, &opts);
            if stat {
                print!("{}", cli::render_stat(&root));
            }
            if root.status == Status::Same {
                0
            } else {
                1
            }
        }
        Command::Help | Command::Version => 0,
    }
}
