//! Optional highlighting through the `syntect` crate (cargo feature `syntect`).
//!
//! syntect brings the Sublime Text syntax definitions, so far more languages
//! are recognised than by the built-in tokenizer in `highlight.rs`, which is
//! used for anything syntect does not know (TOML, TypeScript, Kconfig, BitBake,
//! Devicetree, ...). Only syntect's *parser* is used: the scopes it produces are
//! mapped onto the same `Kind`s, so the colours stay the same either way.
//!
//! NOTE: this module needs the `syntect` crate and has not been compiled by the
//! author's offline test setup; build with `cargo build --release --features syntect`.

use std::path::Path;
use std::sync::OnceLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

use crate::highlight::Kind;

const MAX_TOTAL: usize = 3_000_000;

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn find<'a>(ss: &'a SyntaxSet, path: &Path, first: Option<&str>) -> Option<&'a SyntaxReference> {
    let by_name = path.file_name().and_then(|n| n.to_str()).and_then(|n| ss.find_syntax_by_extension(n));
    let by_ext = path.extension().and_then(|e| e.to_str()).and_then(|e| ss.find_syntax_by_extension(e));
    let found = by_name.or(by_ext).or_else(|| first.and_then(|l| ss.find_syntax_by_first_line(l)))?;
    if found.name == "Plain Text" {
        None
    } else {
        Some(found)
    }
}

/// Map the innermost recognised scope of `stack` to a `Kind`.
fn classify(stack: &ScopeStack) -> Kind {
    for sc in stack.as_slice().iter().rev() {
        let name = scope_name(sc);
        let has = |p: &str| name == p || name.starts_with(&format!("{p}."));
        if has("comment") {
            return Kind::Comment;
        }
        if has("string") {
            return Kind::Str;
        }
        if has("constant.numeric") {
            return Kind::Number;
        }
        if has("constant.character.escape") {
            return Kind::Attr;
        }
        if has("constant") {
            return Kind::Const;
        }
        if has("storage.type") || has("entity.name.type") || has("entity.name.class")
            || has("entity.name.struct") || has("entity.name.enum") || has("entity.name.namespace")
            || has("entity.name.tag") || has("support.type") || has("support.class")
            || has("entity.other.inherited-class")
        {
            return Kind::Type;
        }
        if has("keyword") || has("storage") {
            return Kind::Keyword;
        }
        if has("entity.name.function") || has("support.function") || has("variable.function")
            || has("meta.function-call")
        {
            return Kind::Func;
        }
        if has("meta.annotation") || has("meta.preprocessor") || has("entity.name.decorator")
            || has("punctuation.definition.annotation") || has("entity.name.function.preprocessor")
        {
            return Kind::Attr;
        }
        if has("entity.other.attribute-name") || has("meta.mapping.key") || has("entity.name.label") {
            return Kind::Key;
        }
        if has("variable.language") {
            return Kind::Keyword;
        }
        if has("punctuation.definition.variable") || has("variable.other.readwrite.shell") {
            return Kind::Var;
        }
    }
    Kind::Plain
}

fn scope_name(s: &Scope) -> String {
    s.to_string()
}

/// Highlight `lines`; `None` when syntect has no syntax for the file.
pub fn highlight(
    path: &Path,
    first: Option<&str>,
    lines: &[String],
) -> Option<(String, Vec<Vec<Kind>>)> {
    if lines.iter().map(|l| l.len()).sum::<usize>() > MAX_TOTAL {
        return None;
    }
    let ss = syntaxes();
    let syntax = find(ss, path, first)?;
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let nchars = line.chars().count();
        let mut kinds = vec![Kind::Plain; nchars];
        // very long lines are not worth parsing; keep the parser state as it is
        if line.len() <= 4000 {
            let text = format!("{line}\n");
            let ops = state.parse_line(&text, ss).ok()?;
            // byte offset -> char index
            let mut char_at = vec![0usize; text.len() + 1];
            for (ci, (bi, c)) in text.char_indices().enumerate() {
                for k in 0..c.len_utf8() {
                    char_at[bi + k] = ci;
                }
            }
            char_at[text.len()] = nchars + 1;
            let mut paint = |from: usize, to: usize, k: Kind| {
                let (a, b) = (char_at[from.min(text.len())], char_at[to.min(text.len())].min(nchars));
                for x in kinds.iter_mut().take(b).skip(a) {
                    *x = k;
                }
            };
            let mut last = 0usize;
            let mut cur = classify(&stack);
            for (pos, op) in ops {
                paint(last, pos, cur);
                stack.apply(&op).ok()?;
                cur = classify(&stack);
                last = pos;
            }
            paint(last, text.len(), cur);
        }
        out.push(kinds);
    }
    Some((syntax.name.clone(), out))
}
