//! Side-by-side file view: aligned panes, copy hunk / line, inline editing,
//! undo/redo and saving. All state changes go through `engine::Merge`.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::engine::{self, FileDiff, Merge, Pane, RowKind};

pub enum Outcome {
    Stay,
    /// `saved` is true when anything was written to disk (the caller should
    /// refresh its directory comparison).
    Close { saved: bool },
}

#[derive(Clone, Copy)]
enum Disp {
    Row(usize),
    /// A folded run of this many unchanged rows.
    Fold(usize),
}

struct Edit {
    pane: Pane,
    row: usize,
    text: Vec<char>,
    cur: usize,
    /// Insert a new line after `row` instead of changing the line at `row`.
    new_line: bool,
}

enum Mode {
    Normal,
    Edit(Edit),
    ConfirmQuit,
    Help,
}

const FOLD_CONTEXT: usize = 3;

pub struct SideView {
    lpath: PathBuf,
    rpath: PathBuf,
    m: Merge,
    cursor: usize,
    top: usize,
    active: Pane,
    hscroll: usize,
    fold: bool,
    disp: Vec<Disp>,
    vis: Vec<usize>,
    mode: Mode,
    pending: Option<char>,
    msg: String,
    view_h: usize,
    text_w: usize,
    saved_any: bool,
}

impl SideView {
    /// Open two files. A side that does not exist is shown as empty and is
    /// created if you copy into it and save. Returns the `FileDiff` to show
    /// instead when the files cannot be edited as text (binary, too large...).
    pub fn open(
        lpath: PathBuf,
        rpath: PathBuf,
        lexists: bool,
        rexists: bool,
    ) -> Result<SideView, FileDiff> {
        let l = engine::load_text(if lexists { Some(lpath.as_path()) } else { None })?;
        let r = engine::load_text(if rexists { Some(rpath.as_path()) } else { None })?;
        Ok(SideView::new(lpath, rpath, Merge::new(l, r)))
    }

    fn new(lpath: PathBuf, rpath: PathBuf, m: Merge) -> SideView {
        let mut v = SideView {
            lpath,
            rpath,
            m,
            cursor: 0,
            top: 0,
            active: Pane::Right,
            hscroll: 0,
            fold: false,
            disp: Vec::new(),
            vis: Vec::new(),
            mode: Mode::Normal,
            pending: None,
            msg: String::new(),
            view_h: 20,
            text_w: 40,
            saved_any: false,
        };
        v.rebuild();
        match v.m.align.hunks.first() {
            Some(h) => v.cursor = h.rows.start,
            None => v.msg = "files are identical".to_string(),
        }
        v.fix_cursor();
        v
    }

    // ── layout bookkeeping ──

    fn rebuild(&mut self) {
        let rows = &self.m.align.rows;
        let n = rows.len();
        self.disp.clear();
        self.vis.clear();
        let any_change = rows.iter().any(|r| r.kind != RowKind::Equal);
        if !self.fold || !any_change {
            for i in 0..n {
                self.disp.push(Disp::Row(i));
                self.vis.push(i);
            }
            return;
        }
        let mut keep = vec![false; n];
        for (i, r) in rows.iter().enumerate() {
            if r.kind != RowKind::Equal {
                let lo = i.saturating_sub(FOLD_CONTEXT);
                let hi = (i + FOLD_CONTEXT + 1).min(n);
                for k in keep.iter_mut().take(hi).skip(lo) {
                    *k = true;
                }
            }
        }
        let mut i = 0;
        while i < n {
            if keep[i] {
                self.disp.push(Disp::Row(i));
                self.vis.push(i);
                i += 1;
            } else {
                let s = i;
                while i < n && !keep[i] {
                    i += 1;
                }
                self.disp.push(Disp::Fold(i - s));
            }
        }
    }

    fn fix_cursor(&mut self) {
        let n = self.m.align.rows.len();
        if n == 0 {
            self.cursor = 0;
            return;
        }
        if self.cursor >= n {
            self.cursor = n - 1;
        }
        if !self.vis.contains(&self.cursor) {
            let after = self.vis.iter().copied().find(|&i| i >= self.cursor);
            let before = self.vis.iter().rev().copied().find(|&i| i <= self.cursor);
            self.cursor = after.or(before).unwrap_or(0);
        }
    }

    fn after_edit(&mut self) {
        self.rebuild();
        self.fix_cursor();
    }

    fn mv(&mut self, delta: isize) {
        if self.vis.is_empty() {
            return;
        }
        let pos = self.vis.iter().position(|&r| r == self.cursor).unwrap_or(0) as isize;
        let np = (pos + delta).clamp(0, self.vis.len() as isize - 1) as usize;
        self.cursor = self.vis[np];
    }

    fn goto_pos(&mut self, pos: usize) {
        let last = self.vis.len().saturating_sub(1);
        if let Some(&r) = self.vis.get(pos.min(last)) {
            self.cursor = r;
        }
    }

    fn max_hscroll(&self) -> usize {
        let longest = self
            .m
            .left
            .lines
            .iter()
            .chain(self.m.right.lines.iter())
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0);
        longest.saturating_sub(self.text_w / 2)
    }

    // ── input ──

    pub fn on_key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.msg.clear();
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.normal_key(key, ctrl),
            Mode::Help => Outcome::Stay,
            Mode::ConfirmQuit => match key.code {
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    if self.save() {
                        Outcome::Close { saved: self.saved_any }
                    } else {
                        Outcome::Stay
                    }
                }
                KeyCode::Char('d') | KeyCode::Char('D') => Outcome::Close { saved: self.saved_any },
                _ => Outcome::Stay,
            },
            Mode::Edit(e) => {
                self.edit_key(e, key, ctrl);
                Outcome::Stay
            }
        }
    }

    fn normal_key(&mut self, key: KeyEvent, ctrl: bool) -> Outcome {
        if let Some(p) = self.pending.take() {
            match (p, key.code) {
                ('g', KeyCode::Char('g')) => self.goto_pos(0),
                (']', KeyCode::Char('c')) => self.jump_hunk(true),
                ('[', KeyCode::Char('c')) => self.jump_hunk(false),
                ('d', KeyCode::Char('p')) => self.put_hunk(self.active),
                ('d', KeyCode::Char('o')) => self.put_hunk(self.active.other()),
                ('d', KeyCode::Char('d')) => self.delete_line(),
                _ => {}
            }
            return Outcome::Stay;
        }

        let h = self.view_h.max(1) as isize;
        match key.code {
            KeyCode::Char('d') if ctrl => self.mv(h / 2),
            KeyCode::Char('u') if ctrl => self.mv(-(h / 2)),
            KeyCode::Char('f') if ctrl => self.mv(h),
            KeyCode::Char('b') if ctrl => self.mv(-h),
            KeyCode::Char('r') if ctrl => {
                if self.m.redo() {
                    self.after_edit();
                    self.msg = "redo".to_string();
                } else {
                    self.msg = "nothing to redo".to_string();
                }
            }
            KeyCode::Char('s') if ctrl => {
                self.save();
            }
            _ if ctrl => {}
            KeyCode::Char('q') | KeyCode::Esc => {
                if self.m.is_dirty(Pane::Left) || self.m.is_dirty(Pane::Right) {
                    self.mode = Mode::ConfirmQuit;
                } else {
                    return Outcome::Close { saved: self.saved_any };
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.mv(1),
            KeyCode::Char('k') | KeyCode::Up => self.mv(-1),
            KeyCode::PageDown => self.mv(h),
            KeyCode::PageUp => self.mv(-h),
            KeyCode::Home => self.goto_pos(0),
            KeyCode::End | KeyCode::Char('G') => self.goto_pos(usize::MAX),
            KeyCode::Char(c @ ('g' | ']' | '[' | 'd')) => self.pending = Some(c),
            KeyCode::Char('n') => self.jump_hunk(true),
            KeyCode::Char('N') => self.jump_hunk(false),
            KeyCode::Tab => self.active = self.active.other(),
            KeyCode::Char('H') => self.active = Pane::Left,
            KeyCode::Char('L') => self.active = Pane::Right,
            KeyCode::Char('h') | KeyCode::Left => self.hscroll = self.hscroll.saturating_sub(8),
            KeyCode::Char('l') | KeyCode::Right => {
                self.hscroll = (self.hscroll + 8).min(self.max_hscroll())
            }
            KeyCode::Char('0') => self.hscroll = 0,
            KeyCode::Char('>') => self.put_hunk(Pane::Left),
            KeyCode::Char('<') => self.put_hunk(Pane::Right),
            KeyCode::Char('.') => self.put_line(Pane::Left),
            KeyCode::Char(',') => self.put_line(Pane::Right),
            KeyCode::Char('e') | KeyCode::Char('i') | KeyCode::Enter => self.start_edit(false),
            KeyCode::Char('o') => self.start_edit(true),
            KeyCode::Char('u') => {
                if self.m.undo() {
                    self.after_edit();
                    self.msg = "undo".to_string();
                } else {
                    self.msg = "nothing to undo".to_string();
                }
            }
            KeyCode::Char('w') => {
                self.save();
            }
            KeyCode::Char('f') => {
                self.fold = !self.fold;
                self.after_edit();
                self.msg = format!("unchanged lines {}", if self.fold { "folded" } else { "shown" });
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
        Outcome::Stay
    }

    fn edit_key(&mut self, mut e: Edit, key: KeyEvent, ctrl: bool) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => {
                self.commit_edit(e);
                return;
            }
            KeyCode::Left => e.cur = e.cur.saturating_sub(1),
            KeyCode::Right => e.cur = (e.cur + 1).min(e.text.len()),
            KeyCode::Home => e.cur = 0,
            KeyCode::End => e.cur = e.text.len(),
            KeyCode::Backspace => {
                if e.cur > 0 {
                    e.cur -= 1;
                    e.text.remove(e.cur);
                }
            }
            KeyCode::Delete => {
                if e.cur < e.text.len() {
                    e.text.remove(e.cur);
                }
            }
            KeyCode::Tab => {
                e.text.insert(e.cur, '\t');
                e.cur += 1;
            }
            KeyCode::Char(c) if ctrl => match c {
                'a' => e.cur = 0,
                'e' => e.cur = e.text.len(),
                'u' => {
                    e.text.drain(..e.cur);
                    e.cur = 0;
                }
                'k' => e.text.truncate(e.cur),
                'w' => {
                    let mut i = e.cur;
                    while i > 0 && e.text[i - 1].is_whitespace() {
                        i -= 1;
                    }
                    while i > 0 && !e.text[i - 1].is_whitespace() {
                        i -= 1;
                    }
                    e.text.drain(i..e.cur);
                    e.cur = i;
                }
                _ => {}
            },
            KeyCode::Char(c) => {
                e.text.insert(e.cur, c);
                e.cur += 1;
            }
            _ => {}
        }
        self.mode = Mode::Edit(e);
    }

    // ── actions ──

    fn start_edit(&mut self, new_line: bool) {
        let pane = self.active;
        let row = self.m.align.rows.get(self.cursor).cloned();
        if row.is_none() && !new_line {
            return;
        }
        let text: Vec<char> = if new_line {
            Vec::new()
        } else {
            let idx = row.as_ref().and_then(|r| match pane {
                Pane::Left => r.left,
                Pane::Right => r.right,
            });
            match idx {
                Some(i) => self.m.buf(pane).lines[i].chars().collect(),
                None => Vec::new(), // a gap: typing here inserts a line
            }
        };
        let cur = text.len();
        self.mode = Mode::Edit(Edit { pane, row: self.cursor, text, cur, new_line });
    }

    fn commit_edit(&mut self, e: Edit) {
        let text: String = e.text.into_iter().collect();
        let changed = if e.new_line {
            self.m.insert_after(e.row, e.pane, text)
        } else {
            self.m.set_line(e.row, e.pane, text)
        };
        if changed {
            self.after_edit();
        }
    }

    fn put_hunk(&mut self, from: Pane) {
        let Some(h) = self.m.hunk_at(self.cursor) else {
            self.msg = "not on a change".to_string();
            return;
        };
        let start = self.m.align.hunks[h].rows.start;
        if self.m.put_hunk(h, from) {
            self.cursor = start;
            self.after_edit();
            self.msg = format!("copied change to the {} side", from.other().name());
        }
    }

    fn put_line(&mut self, from: Pane) {
        let on_change = self
            .m
            .align
            .rows
            .get(self.cursor)
            .map_or(false, |r| r.kind != RowKind::Equal);
        if !on_change {
            self.msg = "not on a change".to_string();
            return;
        }
        if self.m.put_line(self.cursor, from) {
            self.after_edit();
            self.msg = format!("copied line to the {} side", from.other().name());
        } else {
            self.msg = "lines are already equal".to_string();
        }
    }

    fn delete_line(&mut self) {
        if self.m.delete_line(self.cursor, self.active) {
            self.after_edit();
            self.msg = format!("deleted line on the {} side", self.active.name());
        } else {
            self.msg = format!("no line here on the {} side", self.active.name());
        }
    }

    fn jump_hunk(&mut self, forward: bool) {
        match self.m.next_hunk_row(self.cursor, forward) {
            Some(r) => self.cursor = r,
            None => self.msg = "no more changes".to_string(),
        }
    }

    fn save(&mut self) -> bool {
        match self.m.save(&self.lpath, &self.rpath) {
            Ok(w) if w.is_empty() => {
                self.msg = "nothing to save".to_string();
                true
            }
            Ok(w) => {
                self.saved_any = true;
                let names: Vec<&str> = w.iter().map(|p| p.name()).collect();
                self.msg = format!("saved {}", names.join(" + "));
                true
            }
            Err(e) => {
                self.saved_any = true; // one side may have been written
                self.msg = format!("save failed: {e}");
                false
            }
        }
    }

    // ── status / drawing ──

    pub fn status(&self) -> String {
        match &self.mode {
            Mode::Edit(e) => format!(
                " editing {} line: Enter accept, Esc cancel, ^a ^e ^u ^k ^w",
                e.pane.name()
            ),
            Mode::ConfirmQuit => {
                " Unsaved changes: s save & close, d discard & close, other key keeps editing"
                    .to_string()
            }
            _ if !self.msg.is_empty() => format!(" {}", self.msg),
            _ => {
                let n = self.m.align.hunks.len();
                let info = match self.m.hunk_at(self.cursor) {
                    Some(h) => format!("change {}/{}", h + 1, n),
                    None => format!("{n} change{}", if n == 1 { "" } else { "s" }),
                };
                format!(
                    " {info}  >/< copy change  ,/. copy line  e edit  o add  dd delete  u undo  w save  ? help  q close"
                )
            }
        }
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        self.view_h = (area.height.saturating_sub(2) as usize).max(1);
        let inner_w = cols[0].width.saturating_sub(2) as usize;
        let nmax = self.m.left.lines.len().max(self.m.right.lines.len());
        let gw = nmax.to_string().len().max(3);
        self.text_w = inner_w.saturating_sub(gw + 2).max(1);

        let cpos = self
            .disp
            .iter()
            .position(|d| matches!(d, Disp::Row(i) if *i == self.cursor))
            .unwrap_or(0);
        if cpos < self.top {
            self.top = cpos;
        } else if cpos >= self.top + self.view_h {
            self.top = cpos + 1 - self.view_h;
        }
        self.top = self.top.min(self.disp.len().saturating_sub(1));

        let cur_hunk = self.m.hunk_at(self.cursor);
        let end = (self.top + self.view_h).min(self.disp.len());
        for (ci, pane) in [Pane::Left, Pane::Right].into_iter().enumerate() {
            let path = if pane == Pane::Left { &self.lpath } else { &self.rpath };
            let buf = self.m.buf(pane);
            let title = format!(
                " {}{}{} ",
                path.display(),
                if self.m.is_dirty(pane) { " [+]" } else { "" },
                if buf.exists { "" } else { " (new)" }
            );
            let block = Block::default().borders(Borders::ALL).title(title).border_style(
                Style::default().fg(if self.active == pane { Color::Green } else { Color::DarkGray }),
            );
            let lines: Vec<Line> = self.disp[self.top..end]
                .iter()
                .map(|d| self.render_line(*d, pane, gw, cur_hunk))
                .collect();
            f.render_widget(Paragraph::new(lines).block(block), cols[ci]);
        }

        if let Mode::Help = self.mode {
            let lines: Vec<Line> = HELP.lines().map(|l| Line::from(l.to_string())).collect();
            let w = 74.min(area.width);
            let h = (lines.len() as u16 + 2).min(area.height);
            let r = Rect {
                x: area.x + (area.width - w) / 2,
                y: area.y + (area.height - h) / 2,
                width: w,
                height: h,
            };
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(lines).block(
                    Block::default().borders(Borders::ALL).title(" Side-by-side keys (any key closes) "),
                ),
                r,
            );
        }
    }

    fn render_line(&self, d: Disp, pane: Pane, gw: usize, cur_hunk: Option<usize>) -> Line<'static> {
        let tw = self.text_w;
        let dim = Style::default().add_modifier(Modifier::DIM);
        let i = match d {
            Disp::Fold(n) => {
                let msg = format!("··· {n} unchanged line{} ···", if n == 1 { "" } else { "s" });
                return Line::from(Span::styled(format!("{:^w$}", msg, w = gw + 2 + tw), dim));
            }
            Disp::Row(i) => i,
        };
        let row = &self.m.align.rows[i];
        let (idx, other_idx) = match pane {
            Pane::Left => (row.left, row.right),
            Pane::Right => (row.right, row.left),
        };
        let buf = self.m.buf(pane);

        let is_cursor = i == self.cursor;
        let active = self.active == pane;
        let deco = move |s: Style| -> Style {
            if is_cursor {
                let s = s.add_modifier(Modifier::REVERSED);
                if active {
                    s
                } else {
                    s.add_modifier(Modifier::DIM)
                }
            } else {
                s
            }
        };

        let mut base = Style::default();
        match (row.kind, idx.is_some()) {
            (RowKind::Equal, _) => {}
            (RowKind::Changed, _) => base = base.fg(Color::Yellow),
            (_, true) => base = base.fg(Color::Green),
            (_, false) => base = base.fg(Color::Red).add_modifier(Modifier::DIM),
        }

        let num = match idx {
            Some(k) => format!("{:>w$} ", k + 1, w = gw),
            None => " ".repeat(gw + 1),
        };
        let mark = if row.kind == RowKind::Equal {
            ' '
        } else if row.hunk.is_some() && row.hunk == cur_hunk {
            '┃'
        } else {
            '│'
        };
        let mut spans = vec![Span::styled(num, deco(dim)), Span::styled(mark.to_string(), deco(base))];

        let editing = match &self.mode {
            Mode::Edit(e) if e.pane == pane && e.row == i => Some(e),
            _ => None,
        };
        if let Some(e) = editing {
            spans.extend(edit_spans(e, tw));
        } else if let Some(k) = idx {
            let text = &buf.lines[k];
            let hl = match (row.kind, other_idx) {
                (RowKind::Changed, Some(o)) => {
                    Some(diff_span(text, &self.m.buf(pane.other()).lines[o]))
                }
                _ => None,
            };
            spans.extend(text_spans(text, hl, base, self.hscroll, tw, &deco));
        } else {
            spans.push(Span::styled(" ".repeat(tw), deco(base)));
        }
        Line::from(spans)
    }
}

const HELP: &str = "\
 Move                              Copy / edit
  j k  ^d ^u ^f ^b  gg G            >  copy change left -> right
  n N  ]c [c   next/prev change     <  copy change right -> left
  Tab H L   switch active pane      .  copy line left -> right
  h l   scroll sideways, 0 reset    ,  copy line right -> left
  f   fold/unfold unchanged lines   dp put change active -> other
                                    do obtain change other -> active
 Edit (active pane)                 e  Enter  edit line (on a gap: add one)
  o   add a line below              dd delete line
  while editing: arrows Home End    u  undo     ^r  redo
  Backspace Delete ^a ^e ^u ^k ^w   w  ^s  save changed sides
  Enter accept, Esc cancel          q  Esc  close (asks if unsaved)

 Yellow = changed, green = only here, red = gap (missing here).
 Saving a side that does not exist yet creates the file.";

// ───────────────────────────── text helpers ─────────────────────────────

/// Character range `[start, end)` of `a` that differs from `b`, found by
/// trimming the common prefix and suffix.
fn diff_span(a: &str, b: &str) -> (usize, usize) {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let mut p = 0;
    while p < ac.len() && p < bc.len() && ac[p] == bc[p] {
        p += 1;
    }
    let mut s = 0;
    while s < ac.len() - p && s < bc.len() - p && ac[ac.len() - 1 - s] == bc[bc.len() - 1 - s] {
        s += 1;
    }
    (p, ac.len() - s)
}

fn text_spans(
    text: &str,
    hl: Option<(usize, usize)>,
    base: Style,
    hscroll: usize,
    tw: usize,
    deco: &dyn Fn(Style) -> Style,
) -> Vec<Span<'static>> {
    let mut cells: Vec<(char, bool)> = Vec::new();
    for (ci, c) in text.chars().enumerate() {
        let h = hl.map_or(false, |(a, b)| ci >= a && ci < b);
        if c == '\t' {
            for _ in 0..4 {
                cells.push((' ', h));
            }
        } else if c.is_control() {
            cells.push(('·', h));
        } else {
            cells.push((c, h));
        }
    }
    let mut vis: Vec<(char, bool)> = cells.into_iter().skip(hscroll).take(tw).collect();
    while vis.len() < tw {
        vis.push((' ', false));
    }
    let make = |s: String, h: bool| -> Span<'static> {
        let st = if h {
            base.add_modifier(Modifier::BOLD).add_modifier(Modifier::UNDERLINED)
        } else {
            base
        };
        Span::styled(s, deco(st))
    };
    let mut spans = Vec::new();
    let mut cur = String::new();
    let mut cur_h = false;
    for (c, h) in vis {
        if h != cur_h && !cur.is_empty() {
            spans.push(make(std::mem::take(&mut cur), cur_h));
        }
        cur_h = h;
        cur.push(c);
    }
    if !cur.is_empty() {
        spans.push(make(cur, cur_h));
    }
    spans
}

fn edit_spans(e: &Edit, tw: usize) -> Vec<Span<'static>> {
    let mut cells: Vec<char> = Vec::new();
    let mut cx = 0;
    for (i, &c) in e.text.iter().enumerate() {
        if i == e.cur {
            cx = cells.len();
        }
        if c == '\t' {
            cells.extend([' '; 4]);
        } else if c.is_control() {
            cells.push('·');
        } else {
            cells.push(c);
        }
    }
    if e.cur >= e.text.len() {
        cx = cells.len();
    }
    cells.push(' '); // room for the cursor at the end of the line
    let start = if cx >= tw { cx + 1 - tw } else { 0 };
    let window: Vec<char> = cells.iter().skip(start).take(tw).copied().collect();
    let cpos = (cx - start).min(window.len().saturating_sub(1));
    let style = Style::default().fg(Color::Cyan);
    let before: String = window[..cpos].iter().collect();
    let at: String = window[cpos..cpos + 1].iter().collect();
    let mut after: String = window[cpos + 1..].iter().collect();
    let used = window.len();
    if used < tw {
        after.push_str(&" ".repeat(tw - used));
    }
    vec![
        Span::styled(before, style),
        Span::styled(at, style.add_modifier(Modifier::REVERSED)),
        Span::styled(after, style),
    ]
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir()
            .join(format!("dircmp-side-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn view(a: &str, b: &str) -> (PathBuf, SideView) {
        let root = tmp();
        fs::write(root.join("l"), a).unwrap();
        fs::write(root.join("r"), b).unwrap();
        let v = SideView::open(root.join("l"), root.join("r"), true, true).ok().unwrap();
        (root, v)
    }

    fn k(v: &mut SideView, code: KeyCode) -> bool {
        matches!(v.on_key(KeyEvent::new(code, KeyModifiers::NONE)), Outcome::Close { .. })
    }
    fn ctrl(v: &mut SideView, c: char) {
        v.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }
    fn typed(v: &mut SideView, s: &str) {
        for c in s.chars() {
            k(v, KeyCode::Char(c));
        }
    }
    fn lines(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn copy_hunk_and_save() {
        let (root, mut v) = view("a\nb\nc\n", "a\nX\nc\n");
        assert_eq!(v.cursor, 1); // starts on the first change
        typed(&mut v, ">");
        assert_eq!(v.m.right.lines, lines("a b c"));
        assert!(v.m.is_dirty(Pane::Right) && !v.m.is_dirty(Pane::Left));
        assert!(v.m.align.hunks.is_empty());
        typed(&mut v, "u");
        assert_eq!(v.m.right.lines, lines("a X c"));
        ctrl(&mut v, 'r');
        assert_eq!(v.m.right.lines, lines("a b c"));
        typed(&mut v, "w");
        assert_eq!(fs::read_to_string(root.join("r")).unwrap(), "a\nb\nc\n");
        assert!(v.saved_any);
        // other direction
        let (_root2, mut v) = view("a\nb\nc\n", "a\nX\nc\n");
        typed(&mut v, "<");
        assert_eq!(v.m.left.lines, lines("a X c"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_line_and_not_on_change() {
        let (root, mut v) = view("a\nb\nc\nd\n", "a\nX\nc\nd\ne\n");
        typed(&mut v, ".");
        assert_eq!(v.m.right.lines, lines("a b c d e"));
        typed(&mut v, ",");
        assert_eq!(v.msg, "not on a change"); // row 1 is equal now
        // jump to the trailing right-only line and pull it left
        typed(&mut v, "n");
        assert_eq!(v.m.align.rows[v.cursor].kind, RowKind::RightOnly);
        typed(&mut v, ",");
        assert_eq!(v.m.left.lines, lines("a b c d e"));
        typed(&mut v, "n");
        assert_eq!(v.msg, "no more changes");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn navigation() {
        let (root, mut v) = view("1\n2\n3\n4\n5\n6\n", "1\nX\n3\n4\nY\n6\n");
        assert_eq!(v.cursor, 1);
        typed(&mut v, "n");
        assert_eq!(v.cursor, 4);
        typed(&mut v, "N");
        assert_eq!(v.cursor, 1);
        typed(&mut v, "]c");
        assert_eq!(v.cursor, 4);
        typed(&mut v, "[c");
        assert_eq!(v.cursor, 1);
        typed(&mut v, "G");
        assert_eq!(v.cursor, 5);
        typed(&mut v, "gg");
        assert_eq!(v.cursor, 0);
        typed(&mut v, "k");
        assert_eq!(v.cursor, 0);
        k(&mut v, KeyCode::Tab);
        assert_eq!(v.active, Pane::Left);
        typed(&mut v, "L");
        assert_eq!(v.active, Pane::Right);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn dp_do_dd() {
        let (root, mut v) = view("a\nb\nc\n", "a\nX\nc\n");
        // active pane is right: `do` obtains the left version into the right
        typed(&mut v, "do");
        assert_eq!(v.m.right.lines, lines("a b c"));
        typed(&mut v, "u");
        // `dp` puts the active (right) version into the left
        typed(&mut v, "dp");
        assert_eq!(v.m.left.lines, lines("a X c"));
        // dd deletes the line on the active pane
        typed(&mut v, "dd");
        assert_eq!(v.m.right.lines, lines("a c"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn editing_lines() {
        let (root, mut v) = view("a\nb\nc\n", "a\nb\nc\n");
        typed(&mut v, "j"); // row 1, active pane = right
        typed(&mut v, "e");
        assert!(matches!(v.mode, Mode::Edit(_)));
        typed(&mut v, "!!");
        k(&mut v, KeyCode::Enter);
        assert_eq!(v.m.right.lines, lines("a b!! c"));
        assert!(matches!(v.mode, Mode::Normal));

        // cursor keys, delete and kill commands
        typed(&mut v, "e");
        k(&mut v, KeyCode::Home);
        k(&mut v, KeyCode::Delete); // removes 'b'
        k(&mut v, KeyCode::Right);
        k(&mut v, KeyCode::Backspace); // removes first '!'
        typed(&mut v, "XY");
        k(&mut v, KeyCode::Enter);
        assert_eq!(v.m.right.lines[1], "XY!");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn edit_kill_keys_and_cancel() {
        let (root, mut v) = view("one\ntwo three\n", "one\nzzz\n");
        typed(&mut v, "j");
        typed(&mut v, "e"); // right pane, "zzz"
        ctrl(&mut v, 'u'); // clears before cursor (cursor is at the end)
        typed(&mut v, "alpha beta gamma");
        ctrl(&mut v, 'w'); // kills "gamma"
        ctrl(&mut v, 'a');
        ctrl(&mut v, 'k'); // kills everything after cursor
        typed(&mut v, "new");
        ctrl(&mut v, 'e');
        typed(&mut v, "!");
        k(&mut v, KeyCode::Enter);
        assert_eq!(v.m.right.lines[1], "new!");
        // Esc cancels without changes
        typed(&mut v, "e");
        typed(&mut v, "junk");
        k(&mut v, KeyCode::Esc);
        assert_eq!(v.m.right.lines[1], "new!");
        assert!(matches!(v.mode, Mode::Normal));
        // unchanged text is not an edit
        let undo_before = v.m.undo();
        assert!(undo_before);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn add_line_and_gap_edit() {
        let (root, mut v) = view("a\nb\n", "a\n");
        typed(&mut v, "gg");
        typed(&mut v, "o");
        typed(&mut v, "new");
        k(&mut v, KeyCode::Enter);
        assert_eq!(v.m.right.lines, lines("a new"));
        // typing on a gap inserts a line there
        let (root2, mut v) = view("a\nb\n", "a\n");
        typed(&mut v, "n"); // the right-side gap row for "b"
        assert!(v.m.align.rows[v.cursor].right.is_none());
        typed(&mut v, "e");
        typed(&mut v, "b");
        k(&mut v, KeyCode::Enter);
        assert_eq!(v.m.right.lines, lines("a b"));
        assert!(v.m.align.hunks.is_empty());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&root2);
    }

    #[test]
    fn quit_flow() {
        let (root, mut v) = view("a\n", "b\n");
        // clean quit
        let (_r2, mut clean) = view("a\n", "a\n");
        assert!(k(&mut clean, KeyCode::Char('q')));
        // dirty quit asks; other key keeps editing
        typed(&mut v, ">");
        assert!(!k(&mut v, KeyCode::Char('q')));
        assert!(matches!(v.mode, Mode::ConfirmQuit));
        assert!(!k(&mut v, KeyCode::Char('x')));
        assert!(matches!(v.mode, Mode::Normal));
        // discard
        k(&mut v, KeyCode::Char('q'));
        assert!(k(&mut v, KeyCode::Char('d')));
        assert_eq!(fs::read_to_string(root.join("r")).unwrap(), "b\n");
        // save & close
        k(&mut v, KeyCode::Char('q'));
        assert!(k(&mut v, KeyCode::Char('s')));
        assert_eq!(fs::read_to_string(root.join("r")).unwrap(), "a\n");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn one_sided_file_is_created_on_save() {
        let root = tmp();
        fs::write(root.join("l"), "x\ny\n").unwrap();
        let mut v = SideView::open(root.join("l"), root.join("sub/r"), true, false).ok().unwrap();
        assert!(!v.m.right.exists);
        typed(&mut v, ">");
        assert_eq!(v.m.right.lines, lines("x y"));
        typed(&mut v, "w");
        assert_eq!(fs::read_to_string(root.join("sub/r")).unwrap(), "x\ny\n");
        assert!(v.m.right.exists);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unsupported_files_fall_back() {
        let root = tmp();
        fs::write(root.join("bin"), [0u8, 1, 2]).unwrap();
        fs::write(root.join("t"), "x\n").unwrap();
        assert!(matches!(
            SideView::open(root.join("bin"), root.join("t"), true, true),
            Err(FileDiff::Binary)
        ));
        assert!(matches!(
            SideView::open(root.clone(), root.join("t"), true, true),
            Err(FileDiff::TypeMismatch)
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn folding() {
        let mut a = String::new();
        let mut b = String::new();
        for i in 0..40 {
            a.push_str(&format!("line{i}\n"));
            b.push_str(&if i == 20 { "CHANGED\n".to_string() } else { format!("line{i}\n") });
        }
        let (root, mut v) = view(&a, &b);
        assert_eq!(v.vis.len(), 40);
        typed(&mut v, "f");
        assert!(v.fold);
        // 3 rows of context each side of the change + the change = 7 visible
        assert_eq!(v.vis.len(), 7);
        assert_eq!(v.disp.iter().filter(|d| matches!(d, Disp::Fold(_))).count(), 2);
        assert_eq!(v.cursor, 20);
        // moving skips the folded rows
        typed(&mut v, "gg");
        assert_eq!(v.cursor, 17);
        typed(&mut v, "G");
        assert_eq!(v.cursor, 23);
        typed(&mut v, "f");
        assert_eq!(v.vis.len(), 40);
        assert_eq!(v.cursor, 23);
        // nothing to fold when there are no changes: everything stays visible
        let (_r2, mut same) = view("a\nb\n", "a\nb\n");
        typed(&mut same, "f");
        assert_eq!(same.vis.len(), 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn horizontal_scroll_and_help() {
        let long = "x".repeat(300);
        let (root, mut v) = view(&format!("{long}\n"), "short\n");
        v.text_w = 40;
        typed(&mut v, "l");
        assert_eq!(v.hscroll, 8);
        for _ in 0..100 {
            typed(&mut v, "l");
        }
        assert_eq!(v.hscroll, 300 - 20);
        typed(&mut v, "h");
        assert_eq!(v.hscroll, 300 - 28);
        typed(&mut v, "0");
        assert_eq!(v.hscroll, 0);
        typed(&mut v, "?");
        assert!(matches!(v.mode, Mode::Help));
        typed(&mut v, "q");
        assert!(matches!(v.mode, Mode::Normal));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn intraline_span() {
        assert_eq!(diff_span("hello world", "hello brave world"), (6, 6)); // pure insertion on the other side
        assert_eq!(diff_span("abcXdef", "abcYYdef"), (3, 4));
        assert_eq!(diff_span("same", "same"), (4, 4));
        assert_eq!(diff_span("", "x"), (0, 0));
        assert_eq!(diff_span("abc", "xyz"), (0, 3));
        assert_eq!(diff_span("aXa", "aa"), (1, 2));
    }

    #[test]
    fn status_text() {
        let (root, mut v) = view("a\nb\n", "a\nX\n");
        assert!(v.status().contains("change 1/1"));
        typed(&mut v, "e");
        assert!(v.status().contains("editing right"));
        k(&mut v, KeyCode::Esc);
        typed(&mut v, ">");
        typed(&mut v, "q");
        assert!(v.status().contains("Unsaved"));
        let _ = fs::remove_dir_all(&root);
    }
}
