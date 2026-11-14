//! Syntax highlighting for the diff views (std only, no dependencies).
//!
//! A small table-driven tokenizer: per language a keyword list, comment and
//! string syntax and a few flags. It is not a full parser; it colours the
//! things that matter in a diff (keywords, types, strings, comments, numbers,
//! constants, calls, attributes, variables, keys) and never fails: on odd
//! input it just paints less.
//!
//! The whole file is tokenized once, from the top, so multi-line comments and
//! strings are right even when only a few lines are shown. The result is one
//! `Kind` per `char` of every line.
//!
//! With the cargo feature `syntect` a much larger set of languages is used
//! first (see `syn_syntect.rs`); this tokenizer is the fallback.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Keyword,
    Type,
    Str,
    Comment,
    Number,
    Const,
    Func,
    /// Attributes, decorators, preprocessor directives, macros, lifetimes.
    Attr,
    /// Shell / make / nix variables, device-tree references.
    Var,
    /// Keys in JSON / YAML / TOML / ini / makefiles, attribute names in markup.
    Key,
}

pub type Lang = &'static LangDef;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Code,
    Markup,
    Markdown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Keys {
    None,
    /// `key: value`
    Yaml,
    /// `key = value`, `KEY ?= value`, `KEY:append += value`
    Assign,
    /// `Assign` plus makefile targets `name: deps`
    Make,
}

struct Multi {
    open: &'static str,
    close: &'static str,
    escapes: bool,
}

pub struct LangDef {
    pub name: &'static str,
    exts: &'static [&'static str],
    /// Exact file names; a trailing `*` makes it a (case-insensitive) prefix.
    files: &'static [&'static str],
    /// Interpreter names from a `#!` line (trailing `*` = prefix).
    shebang: &'static [&'static str],
    mode: Mode,
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    consts: &'static [&'static str],
    line_comments: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    nested: bool,
    /// Single-line string delimiters.
    quotes: &'static [char],
    /// `'...'` has no backslash escapes (shell, TOML literal strings).
    sq_raw: bool,
    /// Strings that may continue on the next line.
    multi: &'static [Multi],
    /// Letters that may precede a quote (`b"..."`, `f'...'`, `r#"..."#`).
    prefixes: &'static [&'static str],
    /// Attribute openers such as `#[` that run to the matching `]`.
    attrs: &'static [&'static str],
    case_insens: bool,
    /// Capitalised words are types, ALL_CAPS words are constants.
    caps_types: bool,
    /// A word followed by `(` is a call.
    funcs: bool,
    /// `name!` is a macro (Rust).
    macros: bool,
    /// `'a` is a lifetime and `'x'` a char (Rust).
    rust_chars: bool,
    raw_strings: bool,
    /// `#include`, `#define`, ... at the start of a line.
    preproc: bool,
    /// `@name` (decorators, annotations, CSS at-rules).
    decorator: bool,
    /// `$x`, `${x}`.
    dollar: bool,
    /// `$(X)` is a variable too (make).
    paren_vars: bool,
    /// `&label` (device tree).
    amp_refs: bool,
    /// `/dts-v1/`
    slash_dirs: bool,
    /// `a-b` is one word.
    dash_idents: bool,
    /// `#rrggbb`
    hash_colors: bool,
    /// A line comment must be preceded by whitespace (shell, ini).
    comment_boundary: bool,
    /// A quote must not directly follow a letter or digit (yaml, ini).
    quote_boundary: bool,
    /// A string followed by `:` is a key (JSON, YAML).
    string_keys: bool,
    keys: Keys,
    /// `[section]` lines.
    ini_sections: bool,
}

const BASE: LangDef = LangDef {
    name: "",
    exts: &[],
    files: &[],
    shebang: &[],
    mode: Mode::Code,
    keywords: &[],
    types: &[],
    consts: &[],
    line_comments: &[],
    block: None,
    nested: false,
    quotes: &['"', '\''],
    sq_raw: false,
    multi: &[],
    prefixes: &[],
    attrs: &[],
    case_insens: false,
    caps_types: false,
    funcs: false,
    macros: false,
    rust_chars: false,
    raw_strings: false,
    preproc: false,
    decorator: false,
    dollar: false,
    paren_vars: false,
    amp_refs: false,
    slash_dirs: false,
    dash_idents: false,
    hash_colors: false,
    comment_boundary: false,
    quote_boundary: false,
    string_keys: false,
    keys: Keys::None,
    ini_sections: false,
};

const C_BLOCK: Option<(&str, &str)> = Some(("/*", "*/"));
const DQ_MULTI: &[Multi] = &[Multi { open: "\"", close: "\"", escapes: true }];
const SH_MULTI: &[Multi] = &[
    Multi { open: "\"", close: "\"", escapes: true },
    Multi { open: "'", close: "'", escapes: false },
];
const TRIPLE: &[Multi] = &[
    Multi { open: "\"\"\"", close: "\"\"\"", escapes: true },
    Multi { open: "'''", close: "'''", escapes: true },
];
const BACKTICK: &[Multi] = &[Multi { open: "`", close: "`", escapes: false }];
const TEMPLATE: &[Multi] = &[Multi { open: "`", close: "`", escapes: true }];
const TEXT_BLOCK: &[Multi] = &[Multi { open: "\"\"\"", close: "\"\"\"", escapes: true }];

// ───────────────────────────── language table ─────────────────────────────

static RUST: LangDef = LangDef {
    name: "Rust",
    exts: &["rs"],
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
        "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "type",
        "unsafe", "use", "where", "while",
    ],
    types: &[
        "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
        "f32", "f64", "bool", "char", "str",
    ],
    consts: &["true", "false"],
    line_comments: &["//"],
    block: C_BLOCK,
    nested: true,
    quotes: &[],
    multi: DQ_MULTI,
    prefixes: &["b", "r", "br", "c", "cr"],
    attrs: &["#![", "#["],
    caps_types: true,
    funcs: true,
    macros: true,
    rust_chars: true,
    raw_strings: true,
    ..BASE
};

const C_KW: &[&str] = &[
    "auto", "break", "case", "const", "continue", "default", "do", "else", "enum", "extern", "for",
    "goto", "if", "inline", "register", "restrict", "return", "sizeof", "static", "struct",
    "switch", "typedef", "union", "volatile", "while", "_Static_assert", "_Alignas", "_Alignof",
];
const C_TYPES: &[&str] = &[
    "void", "char", "short", "int", "long", "float", "double", "signed", "unsigned", "bool",
    "_Bool", "size_t", "ssize_t", "ptrdiff_t", "intptr_t", "uintptr_t", "int8_t", "int16_t",
    "int32_t", "int64_t", "uint8_t", "uint16_t", "uint32_t", "uint64_t", "FILE", "va_list",
    "wchar_t", "off_t",
];

static C: LangDef = LangDef {
    name: "C",
    exts: &["c", "h"],
    keywords: C_KW,
    types: C_TYPES,
    consts: &["NULL", "true", "false"],
    line_comments: &["//"],
    block: C_BLOCK,
    caps_types: true,
    funcs: true,
    preproc: true,
    ..BASE
};

static CPP: LangDef = LangDef {
    name: "C++",
    exts: &["cc", "cpp", "cxx", "c++", "hpp", "hh", "hxx", "h++", "ino", "ipp", "tpp"],
    keywords: &[
        "alignas", "alignof", "and", "asm", "auto", "break", "case", "catch", "class", "co_await",
        "co_return", "co_yield", "concept", "const", "const_cast", "consteval", "constexpr",
        "constinit", "continue", "decltype", "default", "delete", "do", "dynamic_cast", "else",
        "enum", "explicit", "export", "extern", "final", "for", "friend", "goto", "if", "inline",
        "mutable", "namespace", "new", "noexcept", "not", "operator", "or", "override", "private",
        "protected", "public", "register", "reinterpret_cast", "requires", "return", "sizeof",
        "static", "static_assert", "static_cast", "struct", "switch", "template", "this",
        "thread_local", "throw", "try", "typedef", "typeid", "typename", "union", "using",
        "virtual", "volatile", "while",
    ],
    types: C_TYPES,
    consts: &["NULL", "nullptr", "true", "false"],
    line_comments: &["//"],
    block: C_BLOCK,
    caps_types: true,
    funcs: true,
    preproc: true,
    multi: &[Multi { open: "R\"(", close: ")\"", escapes: false }],
    ..BASE
};

static CSHARP: LangDef = LangDef {
    name: "C#",
    exts: &["cs"],
    keywords: &[
        "abstract", "as", "async", "await", "base", "break", "case", "catch", "checked", "class",
        "const", "continue", "default", "delegate", "do", "else", "enum", "event", "explicit",
        "extern", "finally", "fixed", "for", "foreach", "goto", "if", "implicit", "in",
        "interface", "internal", "is", "lock", "namespace", "new", "operator", "out", "override",
        "params", "partial", "private", "protected", "public", "readonly", "ref", "return",
        "sealed", "sizeof", "stackalloc", "static", "struct", "switch", "this", "throw", "try",
        "typeof", "unchecked", "unsafe", "using", "virtual", "volatile", "while", "var", "yield",
        "record", "init", "get", "set", "where", "when",
    ],
    types: &[
        "bool", "byte", "sbyte", "char", "decimal", "double", "float", "int", "uint", "long",
        "ulong", "short", "ushort", "object", "string", "void", "dynamic",
    ],
    consts: &["true", "false", "null"],
    line_comments: &["//"],
    block: C_BLOCK,
    multi: TEXT_BLOCK,
    caps_types: true,
    funcs: true,
    preproc: true,
    ..BASE
};

static JAVA: LangDef = LangDef {
    name: "Java",
    exts: &["java"],
    keywords: &[
        "abstract", "assert", "break", "case", "catch", "class", "const", "continue", "default",
        "do", "else", "enum", "extends", "final", "finally", "for", "goto", "if", "implements",
        "import", "instanceof", "interface", "native", "new", "package", "private", "protected",
        "public", "return", "static", "strictfp", "super", "switch", "synchronized", "this",
        "throw", "throws", "transient", "try", "volatile", "while", "var", "record", "sealed",
        "permits", "yield",
    ],
    types: &["boolean", "byte", "char", "double", "float", "int", "long", "short", "void"],
    consts: &["true", "false", "null"],
    line_comments: &["//"],
    block: C_BLOCK,
    multi: TEXT_BLOCK,
    decorator: true,
    caps_types: true,
    funcs: true,
    ..BASE
};

static KOTLIN: LangDef = LangDef {
    name: "Kotlin",
    exts: &["kt", "kts"],
    keywords: &[
        "abstract", "annotation", "as", "break", "by", "catch", "class", "companion", "const",
        "constructor", "continue", "crossinline", "data", "do", "else", "enum", "external",
        "final", "finally", "for", "fun", "get", "if", "import", "in", "infix", "init", "inline",
        "inner", "interface", "internal", "is", "lateinit", "noinline", "object", "open",
        "operator", "out", "override", "package", "private", "protected", "public", "reified",
        "return", "sealed", "set", "super", "suspend", "tailrec", "this", "throw", "try",
        "typealias", "val", "var", "vararg", "when", "where", "while",
    ],
    consts: &["true", "false", "null"],
    line_comments: &["//"],
    block: C_BLOCK,
    nested: true,
    multi: TEXT_BLOCK,
    decorator: true,
    caps_types: true,
    funcs: true,
    ..BASE
};

static GO: LangDef = LangDef {
    name: "Go",
    exts: &["go"],
    keywords: &[
        "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough",
        "for", "func", "go", "goto", "if", "import", "interface", "map", "package", "range",
        "return", "select", "struct", "switch", "type", "var",
    ],
    types: &[
        "bool", "byte", "complex64", "complex128", "error", "float32", "float64", "int", "int8",
        "int16", "int32", "int64", "rune", "string", "uint", "uint8", "uint16", "uint32",
        "uint64", "uintptr", "any",
    ],
    consts: &["true", "false", "nil", "iota"],
    line_comments: &["//"],
    block: C_BLOCK,
    multi: BACKTICK,
    caps_types: true,
    funcs: true,
    ..BASE
};

const JS_KW: &[&str] = &[
    "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger",
    "default", "delete", "do", "else", "export", "extends", "finally", "for", "from", "function",
    "if", "import", "in", "instanceof", "let", "new", "of", "return", "static", "super", "switch",
    "this", "throw", "try", "typeof", "var", "void", "while", "with", "yield", "get", "set",
];

static JAVASCRIPT: LangDef = LangDef {
    name: "JavaScript",
    exts: &["js", "mjs", "cjs", "jsx"],
    shebang: &["node", "nodejs", "deno", "bun"],
    keywords: JS_KW,
    consts: &["true", "false", "null", "undefined", "NaN", "Infinity"],
    line_comments: &["//"],
    block: C_BLOCK,
    multi: TEMPLATE,
    decorator: true,
    caps_types: true,
    funcs: true,
    ..BASE
};

static TYPESCRIPT: LangDef = LangDef {
    name: "TypeScript",
    exts: &["ts", "tsx", "mts", "cts"],
    keywords: &[
        "abstract", "as", "async", "await", "break", "case", "catch", "class", "const",
        "continue", "declare", "default", "delete", "do", "else", "enum", "export", "extends",
        "finally", "for", "from", "function", "if", "implements", "import", "in", "infer",
        "instanceof", "interface", "is", "keyof", "let", "namespace", "new", "of", "private",
        "protected", "public", "readonly", "return", "satisfies", "static", "super", "switch",
        "this", "throw", "try", "type", "typeof", "var", "void", "while", "with", "yield", "get",
        "set",
    ],
    types: &[
        "any", "bigint", "boolean", "never", "number", "object", "string", "symbol", "unknown",
    ],
    consts: &["true", "false", "null", "undefined", "NaN", "Infinity"],
    line_comments: &["//"],
    block: C_BLOCK,
    multi: TEMPLATE,
    decorator: true,
    caps_types: true,
    funcs: true,
    ..BASE
};

static PYTHON: LangDef = LangDef {
    name: "Python",
    exts: &["py", "pyi", "pyw"],
    files: &["SConstruct", "SConscript", "wscript"],
    shebang: &["python*"],
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is",
        "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with",
        "yield", "match", "case", "self", "cls",
    ],
    types: &[
        "int", "float", "str", "bool", "list", "dict", "set", "tuple", "bytes", "bytearray",
        "object", "type", "complex", "frozenset", "range",
    ],
    consts: &["True", "False", "None"],
    line_comments: &["#"],
    multi: TRIPLE,
    prefixes: &["r", "b", "u", "f", "br", "rb", "fr", "rf"],
    decorator: true,
    caps_types: true,
    funcs: true,
    ..BASE
};

static RUBY: LangDef = LangDef {
    name: "Ruby",
    exts: &["rb", "rake", "gemspec"],
    files: &["Rakefile", "Gemfile", "Vagrantfile"],
    shebang: &["ruby*"],
    keywords: &[
        "alias", "and", "begin", "break", "case", "class", "def", "defined?", "do", "else",
        "elsif", "end", "ensure", "for", "if", "in", "module", "next", "not", "or", "redo",
        "rescue", "retry", "return", "self", "super", "then", "undef", "unless", "until", "when",
        "while", "yield", "require", "require_relative", "include", "extend", "attr_reader",
        "attr_writer", "attr_accessor", "private", "public", "protected", "lambda", "proc",
    ],
    consts: &["true", "false", "nil"],
    line_comments: &["#"],
    quotes: &['"', '\'', '`'],
    caps_types: true,
    funcs: true,
    ..BASE
};

static LUA: LangDef = LangDef {
    name: "Lua",
    exts: &["lua"],
    shebang: &["lua*", "luajit"],
    keywords: &[
        "and", "break", "do", "else", "elseif", "end", "for", "function", "goto", "if", "in",
        "local", "not", "or", "repeat", "return", "then", "until", "while",
    ],
    consts: &["true", "false", "nil"],
    line_comments: &["--"],
    block: Some(("--[[", "]]")),
    multi: &[Multi { open: "[[", close: "]]", escapes: false }],
    funcs: true,
    ..BASE
};

static SHELL: LangDef = LangDef {
    name: "Shell",
    exts: &["sh", "bash", "zsh", "ksh", "ash", "dash", "fish"],
    files: &[
        ".bashrc", ".bash_profile", ".bash_aliases", ".bash_logout", ".profile", ".zshrc",
        ".zprofile", ".zshenv", "PKGBUILD", "APKBUILD", "configure",
    ],
    shebang: &["sh", "bash", "zsh", "ksh", "ash", "dash", "fish"],
    keywords: &[
        "if", "then", "else", "elif", "fi", "case", "esac", "for", "while", "until", "do", "done",
        "in", "function", "select", "return", "exit", "break", "continue", "local", "export",
        "readonly", "declare", "typeset", "unset", "shift", "source", "alias", "trap", "eval",
        "exec",
    ],
    line_comments: &["#"],
    quotes: &['`'],
    multi: SH_MULTI,
    sq_raw: true,
    dollar: true,
    comment_boundary: true,
    keys: Keys::Assign,
    ..BASE
};

static MAKE: LangDef = LangDef {
    name: "Makefile",
    exts: &["mk", "mak"],
    files: &["Makefile*", "makefile*", "GNUmakefile*"],
    keywords: &[
        "ifeq", "ifneq", "ifdef", "ifndef", "else", "endif", "include", "-include", "sinclude",
        "define", "endef", "export", "unexport", "override", "vpath",
    ],
    line_comments: &["#"],
    quotes: &[],
    dollar: true,
    paren_vars: true,
    comment_boundary: true,
    keys: Keys::Make,
    ..BASE
};

static BITBAKE: LangDef = LangDef {
    name: "BitBake",
    exts: &["bb", "bbappend", "bbclass"],
    keywords: &[
        "inherit", "require", "include", "python", "def", "addtask", "deltask", "after", "before",
        "export", "EXPORT_FUNCTIONS", "fakeroot", "inherit_defer",
    ],
    line_comments: &["#"],
    dollar: true,
    comment_boundary: true,
    keys: Keys::Assign,
    ..BASE
};

static DOCKERFILE: LangDef = LangDef {
    name: "Dockerfile",
    exts: &["dockerfile"],
    files: &["Dockerfile*", "Containerfile*"],
    keywords: &[
        "from", "run", "cmd", "label", "maintainer", "expose", "env", "add", "copy",
        "entrypoint", "volume", "user", "workdir", "arg", "onbuild", "stopsignal", "healthcheck",
        "shell", "as",
    ],
    case_insens: true,
    line_comments: &["#"],
    dollar: true,
    comment_boundary: true,
    ..BASE
};

static CMAKE: LangDef = LangDef {
    name: "CMake",
    exts: &["cmake"],
    files: &["CMakeLists.txt"],
    keywords: &[
        "if", "elseif", "else", "endif", "foreach", "endforeach", "while", "endwhile", "function",
        "endfunction", "macro", "endmacro", "return", "break", "continue", "block", "endblock",
    ],
    consts: &["on", "off", "true", "false", "yes", "no"],
    case_insens: true,
    line_comments: &["#"],
    quotes: &['"'],
    dollar: true,
    funcs: true,
    ..BASE
};

static SQL: LangDef = LangDef {
    name: "SQL",
    exts: &["sql", "psql", "mysql"],
    keywords: &[
        "select", "from", "where", "and", "or", "not", "insert", "into", "values", "update",
        "set", "delete", "create", "table", "alter", "drop", "index", "view", "join", "inner",
        "left", "right", "outer", "full", "cross", "on", "as", "group", "by", "order", "having",
        "limit", "offset", "union", "all", "distinct", "case", "when", "then", "else", "end",
        "is", "in", "like", "between", "exists", "primary", "key", "foreign", "references",
        "default", "unique", "check", "constraint", "begin", "commit", "rollback", "transaction",
        "with", "returning", "asc", "desc", "add", "column", "if", "trigger", "function",
        "procedure", "return", "returns", "declare", "using",
    ],
    types: &[
        "int", "integer", "bigint", "smallint", "serial", "text", "varchar", "char", "boolean",
        "bool", "date", "time", "timestamp", "numeric", "decimal", "float", "real", "double",
        "json", "jsonb", "uuid", "blob",
    ],
    consts: &["true", "false", "null"],
    case_insens: true,
    line_comments: &["--"],
    block: C_BLOCK,
    quotes: &['"', '`'],
    multi: &[Multi { open: "'", close: "'", escapes: false }],
    funcs: true,
    ..BASE
};

static JSON: LangDef = LangDef {
    name: "JSON",
    exts: &["json", "jsonc", "json5", "webmanifest", "geojson", "ipynb", "lock"],
    files: &[".babelrc", ".eslintrc", "composer.lock"],
    consts: &["true", "false", "null"],
    line_comments: &["//"],
    block: C_BLOCK,
    quotes: &['"'],
    string_keys: true,
    ..BASE
};

static TOML: LangDef = LangDef {
    name: "TOML",
    exts: &["toml"],
    files: &["Cargo.lock", "Pipfile", "poetry.lock"],
    consts: &["true", "false", "inf", "nan"],
    line_comments: &["#"],
    multi: TRIPLE,
    sq_raw: true,
    keys: Keys::Assign,
    ini_sections: true,
    ..BASE
};

static INI: LangDef = LangDef {
    name: "Config",
    exts: &["conf", "ini", "cfg", "properties", "desktop", "service", "socket", "timer", "env"],
    files: &[".config", ".gitconfig", ".editorconfig", "config", "defconfig"],
    consts: &["y", "n", "m", "yes", "no", "true", "false", "on", "off"],
    line_comments: &["#", ";"],
    comment_boundary: true,
    quote_boundary: true,
    dollar: true,
    keys: Keys::Assign,
    ini_sections: true,
    ..BASE
};

static KCONFIG: LangDef = LangDef {
    name: "Kconfig",
    exts: &["kconfig"],
    files: &["Kconfig*"],
    keywords: &[
        "mainmenu", "menu", "endmenu", "menuconfig", "config", "choice", "endchoice", "comment",
        "if", "endif", "source", "rsource", "osource", "orsource", "depends", "on", "select",
        "imply", "default", "range", "help", "prompt", "visible", "def_bool", "def_tristate",
        "bool", "tristate", "string", "hex", "int", "option", "modules",
    ],
    consts: &["y", "n", "m"],
    line_comments: &["#"],
    quotes: &['"'],
    ..BASE
};

static YAML: LangDef = LangDef {
    name: "YAML",
    exts: &["yml", "yaml"],
    files: &[".clang-format", ".clang-tidy"],
    consts: &["true", "false", "null", "yes", "no", "on", "off"],
    line_comments: &["#"],
    comment_boundary: true,
    quote_boundary: true,
    string_keys: true,
    keys: Keys::Yaml,
    ..BASE
};

static NIX: LangDef = LangDef {
    name: "Nix",
    exts: &["nix"],
    keywords: &["let", "in", "rec", "inherit", "if", "then", "else", "assert", "with", "or"],
    consts: &["true", "false", "null"],
    line_comments: &["#"],
    block: C_BLOCK,
    quotes: &[],
    multi: &[
        Multi { open: "\"", close: "\"", escapes: true },
        Multi { open: "''", close: "''", escapes: false },
    ],
    dollar: true,
    dash_idents: true,
    keys: Keys::Assign,
    ..BASE
};

static DTS: LangDef = LangDef {
    name: "Devicetree",
    exts: &["dts", "dtsi", "overlay", "dtso"],
    line_comments: &["//"],
    block: C_BLOCK,
    quotes: &['"'],
    preproc: true,
    amp_refs: true,
    slash_dirs: true,
    keys: Keys::Assign,
    ..BASE
};

static CSS: LangDef = LangDef {
    name: "CSS",
    exts: &["css"],
    block: C_BLOCK,
    decorator: true,
    dash_idents: true,
    hash_colors: true,
    funcs: true,
    keys: Keys::Yaml,
    ..BASE
};

static SCSS: LangDef = LangDef {
    name: "SCSS",
    exts: &["scss", "less", "sass"],
    line_comments: &["//"],
    comment_boundary: true,
    block: C_BLOCK,
    decorator: true,
    dash_idents: true,
    hash_colors: true,
    funcs: true,
    keys: Keys::Yaml,
    ..BASE
};

static HTML: LangDef = LangDef {
    name: "HTML/XML",
    exts: &[
        "html", "htm", "xhtml", "xml", "svg", "xsl", "xslt", "xsd", "plist", "rss", "atom", "ui",
        "glade", "vue", "kml", "csproj", "fsproj", "props", "targets", "wsdl",
    ],
    mode: Mode::Markup,
    ..BASE
};

static MARKDOWN: LangDef = LangDef {
    name: "Markdown",
    exts: &["md", "markdown", "mdx", "mkd"],
    mode: Mode::Markdown,
    ..BASE
};

static LANGS: &[&LangDef] = &[
    &RUST, &C, &CPP, &CSHARP, &JAVA, &KOTLIN, &GO, &JAVASCRIPT, &TYPESCRIPT, &PYTHON, &RUBY,
    &LUA, &SHELL, &MAKE, &BITBAKE, &DOCKERFILE, &CMAKE, &SQL, &JSON, &TOML, &INI, &KCONFIG,
    &YAML, &NIX, &DTS, &CSS, &SCSS, &HTML, &MARKDOWN,
];

// ───────────────────────────── detection ─────────────────────────────

fn file_matches(pat: &str, name: &str, lname: &str) -> bool {
    match pat.strip_suffix('*') {
        Some(p) => lname.starts_with(&p.to_lowercase()),
        None => pat == name || pat.to_lowercase() == lname,
    }
}

fn shebang_interpreter(line: &str) -> Option<String> {
    let rest = line.strip_prefix("#!")?.trim();
    let mut words = rest.split_whitespace();
    let first = words.next()?;
    let base = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
    let b = base(first);
    if b == "env" {
        // `env -S python3 -u` / `env VAR=1 python3`: first plain word
        for w in words {
            if !w.starts_with('-') && !w.contains('=') {
                return Some(base(w));
            }
        }
        return None;
    }
    Some(b)
}

/// Pick a language from the file name / extension, or from a `#!` first line.
pub fn detect(path: &Path, first_line: Option<&str>) -> Option<Lang> {
    if let Some(n) = path.file_name() {
        let name = n.to_string_lossy().into_owned();
        let lname = name.to_lowercase();
        for l in LANGS {
            if l.files.iter().any(|f| file_matches(f, &name, &lname)) {
                return Some(l);
            }
        }
    }
    if let Some(e) = path.extension() {
        let ext = e.to_string_lossy().to_lowercase();
        for l in LANGS {
            if l.exts.iter().any(|x| *x == ext) {
                return Some(l);
            }
        }
    }
    let interp = shebang_interpreter(first_line?)?;
    for l in LANGS {
        let hit = l.shebang.iter().any(|s| match s.strip_suffix('*') {
            Some(p) => interp.starts_with(p),
            None => interp == *s,
        });
        if hit {
            return Some(l);
        }
    }
    None
}

// ───────────────────────────── scanning ─────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum St {
    Normal,
    /// Inside a block comment, with its nesting depth.
    Block(u32),
    /// Inside the `n`th multi-line string of the language.
    Multi(usize),
    /// Inside a raw string closed by `"` and this many `#`.
    Raw(usize),
    /// Inside a markup tag.
    Tag,
    TagStr(char),
    /// Inside a fenced code block (markdown).
    Fence,
}

const MAX_LINE: usize = 4000;
const MAX_TOTAL: usize = 3_000_000;

fn starts(c: &[char], i: usize, s: &str) -> bool {
    let mut k = i;
    for ch in s.chars() {
        if c.get(k) != Some(&ch) {
            return false;
        }
        k += 1;
    }
    true
}

fn clen(s: &str) -> usize {
    s.chars().count()
}

fn paint(out: &mut [Kind], a: usize, b: usize, k: Kind) {
    let b = b.min(out.len());
    for x in out.iter_mut().take(b).skip(a) {
        *x = k;
    }
}

fn skip_ws(c: &[char], mut i: usize) -> usize {
    while i < c.len() && c[i].is_whitespace() {
        i += 1;
    }
    i
}

fn is_ident_char(def: &LangDef, c: &[char], j: usize) -> bool {
    let ch = c[j];
    if ch.is_alphanumeric() || ch == '_' {
        return true;
    }
    def.dash_idents
        && ch == '-'
        && c.get(j + 1).map_or(false, |x| x.is_alphanumeric() || *x == '_')
}

fn in_list(list: &[&str], w: &str) -> bool {
    list.iter().any(|k| *k == w)
}

/// Tokenize all `lines`. Returns one `Kind` per `char` of every line, or `None`
/// when the text is too large to be worth highlighting.
pub fn highlight_lines(lang: Lang, lines: &[String]) -> Option<Vec<Vec<Kind>>> {
    let total: usize = lines.iter().map(|l| l.len()).sum();
    if total > MAX_TOTAL {
        return None;
    }
    let mut st = St::Normal;
    let mut out = Vec::with_capacity(lines.len());
    for l in lines {
        let chars: Vec<char> = l.chars().collect();
        let mut kinds = vec![Kind::Plain; chars.len()];
        if chars.len() <= MAX_LINE {
            match lang.mode {
                Mode::Code => scan_code(lang, &chars, &mut st, &mut kinds),
                Mode::Markup => scan_markup(&chars, &mut st, &mut kinds),
                Mode::Markdown => scan_markdown(&chars, &mut st, &mut kinds),
            }
        }
        out.push(kinds);
    }
    Some(out)
}

/// Detect the language of `path` and highlight `lines`. Returns the language
/// name and the kinds. Uses `syntect` first when that feature is built in.
pub fn highlight_file(
    path: &Path,
    first_line: Option<&str>,
    lines: &[String],
) -> Option<(String, Vec<Vec<Kind>>)> {
    #[cfg(all(feature = "syntect", not(test)))]
    {
        if let Some(r) = crate::syn_syntect::highlight(path, first_line, lines) {
            return Some(r);
        }
    }
    let lang = detect(path, first_line)?;
    let kinds = highlight_lines(lang, lines)?;
    Some((lang.name.to_string(), kinds))
}

fn scan_code(def: &LangDef, c: &[char], st: &mut St, out: &mut [Kind]) {
    let n = c.len();
    let mut i = 0usize;
    let key = if *st == St::Normal && def.keys != Keys::None { line_key(def, c) } else { None };

    'outer: while i < n {
        match *st {
            St::Block(d) => {
                let Some((open, close)) = def.block else {
                    *st = St::Normal;
                    continue;
                };
                let (ol, cl) = (clen(open), clen(close));
                let start = i;
                let mut depth = d;
                let mut done = false;
                while i < n {
                    if def.nested && starts(c, i, open) {
                        depth += 1;
                        i += ol;
                    } else if starts(c, i, close) {
                        i += cl;
                        depth -= 1;
                        if depth == 0 {
                            done = true;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                paint(out, start, i, Kind::Comment);
                *st = if done { St::Normal } else { St::Block(depth) };
                continue;
            }
            St::Multi(k) => {
                let m = &def.multi[k];
                let cl = clen(m.close);
                let start = i;
                let mut done = false;
                while i < n {
                    if m.escapes && c[i] == '\\' {
                        i += 2;
                        continue;
                    }
                    if starts(c, i, m.close) {
                        i += cl;
                        done = true;
                        break;
                    }
                    i += 1;
                }
                paint(out, start, i, Kind::Str);
                if done {
                    *st = St::Normal;
                }
                continue;
            }
            St::Raw(h) => {
                let start = i;
                let mut done = false;
                while i < n {
                    if c[i] == '"' && (1..=h).all(|k| c.get(i + k) == Some(&'#')) {
                        i += 1 + h;
                        done = true;
                        break;
                    }
                    i += 1;
                }
                paint(out, start, i, Kind::Str);
                if done {
                    *st = St::Normal;
                }
                continue;
            }
            _ => {}
        }

        if let Some((ks, ke, kk)) = key {
            if i == ks {
                paint(out, ks, ke, kk);
                i = ke;
                continue;
            }
        }

        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }

        // comments (block before line: Lua's `--[[` vs `--`)
        if let Some((open, _)) = def.block {
            if starts(c, i, open) {
                paint(out, i, i + clen(open), Kind::Comment);
                i += clen(open);
                *st = St::Block(1);
                continue;
            }
        }
        if let Some(p) = def.line_comments.iter().find(|p| starts(c, i, p)) {
            let _ = p;
            if !def.comment_boundary || i == 0 || c[i - 1].is_whitespace() {
                paint(out, i, n, Kind::Comment);
                return;
            }
        }

        // multi-line strings (before plain quotes: `"""` vs `"`)
        for (k, m) in def.multi.iter().enumerate() {
            if starts(c, i, m.open) && (!def.quote_boundary || i == 0 || !c[i - 1].is_alphanumeric())
            {
                let ol = clen(m.open);
                paint(out, i, i + ol, Kind::Str);
                i += ol;
                *st = St::Multi(k);
                continue 'outer;
            }
        }

        if def.rust_chars && ch == '\'' {
            if let Some(j) = rust_char(c, i, i, out) {
                i = j;
                continue;
            }
            if i + 1 < n && (c[i + 1].is_alphabetic() || c[i + 1] == '_') {
                let mut j = i + 2;
                while j < n && (c[j].is_alphanumeric() || c[j] == '_') {
                    j += 1;
                }
                paint(out, i, j, Kind::Attr);
                i = j;
            } else {
                i += 1;
            }
            continue;
        }

        if def.quotes.contains(&ch)
            && (!def.quote_boundary || i == 0 || !c[i - 1].is_alphanumeric())
        {
            if let Some(j) = string_start(def, c, i, i, false, st, out) {
                i = j;
                continue;
            }
        }

        // attributes: #[...]
        if let Some(a) = def.attrs.iter().find(|a| starts(c, i, a)) {
            let mut j = i + clen(a);
            let mut depth = 1;
            while j < n && depth > 0 {
                match c[j] {
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            paint(out, i, j, Kind::Attr);
            i = j;
            continue;
        }

        // preprocessor
        if def.preproc && ch == '#' && c[..i].iter().all(|x| x.is_whitespace()) {
            let mut j = skip_ws(c, i + 1);
            let ds = j;
            while j < n && (c[j].is_alphanumeric() || c[j] == '_') {
                j += 1;
            }
            let word: String = c[ds..j].iter().collect();
            const DIRS: &[&str] = &[
                "include", "include_next", "import", "define", "undef", "if", "ifdef", "ifndef",
                "elif", "elifdef", "elifndef", "else", "endif", "pragma", "error", "warning",
                "line", "region", "endregion",
            ];
            if in_list(DIRS, &word) {
                paint(out, i, j, Kind::Attr);
                i = j;
                if word.starts_with("include") || word == "import" {
                    let k = skip_ws(c, i);
                    if k < n && c[k] == '<' {
                        let mut e = k + 1;
                        while e < n && c[e] != '>' {
                            e += 1;
                        }
                        let e = (e + 1).min(n);
                        paint(out, k, e, Kind::Str);
                        i = e;
                    }
                }
                continue;
            }
        }

        // decorators / annotations / at-rules
        if def.decorator && ch == '@' && c.get(i + 1).map_or(false, |x| x.is_alphabetic() || *x == '_')
        {
            let mut j = i + 1;
            while j < n && (c[j].is_alphanumeric() || matches!(c[j], '_' | '.') || is_ident_char(def, c, j))
            {
                j += 1;
            }
            paint(out, i, j, Kind::Attr);
            i = j;
            continue;
        }

        if def.dollar && ch == '$' {
            i = dollar_var(def, c, i, out);
            continue;
        }

        if def.amp_refs
            && ch == '&'
            && c.get(i + 1).map_or(false, |x| x.is_alphabetic() || *x == '_')
            && (i == 0 || !c[i - 1].is_alphanumeric())
        {
            let mut j = i + 1;
            while j < n && (c[j].is_alphanumeric() || matches!(c[j], '_' | '-')) {
                j += 1;
            }
            paint(out, i, j, Kind::Var);
            i = j;
            continue;
        }

        if def.slash_dirs && ch == '/' && c.get(i + 1).map_or(false, |x| x.is_alphabetic()) {
            let mut j = i + 1;
            while j < n && (c[j].is_alphanumeric() || matches!(c[j], '-' | '_')) {
                j += 1;
            }
            if j < n && c[j] == '/' {
                paint(out, i, j + 1, Kind::Attr);
                i = j + 1;
                continue;
            }
        }

        if def.hash_colors && ch == '#' && c.get(i + 1).map_or(false, |x| x.is_ascii_hexdigit()) {
            let mut j = i + 1;
            while j < n && c[j].is_ascii_hexdigit() {
                j += 1;
            }
            paint(out, i, j, Kind::Number);
            i = j;
            continue;
        }

        let prev_wordy = i > 0 && (c[i - 1].is_alphanumeric() || matches!(c[i - 1], '_' | '.' | ')' | ']'));
        if ch.is_ascii_digit()
            || (ch == '.' && c.get(i + 1).map_or(false, |x| x.is_ascii_digit()) && !prev_wordy)
        {
            i = number(c, i, out);
            continue;
        }

        if ch.is_alphabetic() || ch == '_' {
            let start = i;
            let mut j = i + 1;
            while j < n && is_ident_char(def, c, j) {
                j += 1;
            }
            let word: String = c[start..j].iter().collect();
            let lw = if def.case_insens { word.to_lowercase() } else { word.clone() };

            if def.raw_strings
                && word == "r"
                && c.get(j) == Some(&'#')
                && c.get(j + 1).map_or(false, |x| x.is_alphabetic() || *x == '_')
            {
                let mut e = j + 1;
                while e < n && (c[e].is_alphanumeric() || c[e] == '_') {
                    e += 1;
                }
                i = e;
                continue;
            }
            if !def.prefixes.is_empty()
                && j < n
                && (c[j] == '"' || c[j] == '\'' || (def.raw_strings && c[j] == '#'))
            {
                let lp = word.to_lowercase();
                if in_list(def.prefixes, &lp) {
                    let raw_ok = def.raw_strings && lp.ends_with('r');
                    if let Some(nx) = string_start(def, c, start, j, raw_ok, st, out) {
                        i = nx;
                        continue;
                    }
                }
            }

            let mut end = j;
            let kind = if in_list(def.keywords, &lw) {
                Kind::Keyword
            } else if in_list(def.types, &lw) {
                Kind::Type
            } else if in_list(def.consts, &lw) {
                Kind::Const
            } else if def.macros && j < n && c[j] == '!' && c.get(j + 1) != Some(&'=') {
                end = j + 1;
                Kind::Attr
            } else if def.funcs && c.get(skip_ws(c, j)) == Some(&'(') {
                Kind::Func
            } else if def.caps_types && word.chars().next().map_or(false, |f| f.is_uppercase()) {
                if word.chars().count() > 1
                    && word.chars().all(|x| x.is_uppercase() || x.is_ascii_digit() || x == '_')
                {
                    Kind::Const
                } else {
                    Kind::Type
                }
            } else {
                Kind::Plain
            };
            paint(out, start, end, kind);
            i = end;
            continue;
        }

        i += 1;
    }
}

/// `'x'` or `'\n'` starting at `q`; paints from `from`.
fn rust_char(c: &[char], from: usize, q: usize, out: &mut [Kind]) -> Option<usize> {
    let n = c.len();
    if q + 1 < n && c[q + 1] == '\\' {
        let lim = (q + 12).min(n);
        let mut j = q + 3;
        while j < lim {
            if c[j] == '\'' {
                paint(out, from, j + 1, Kind::Str);
                return Some(j + 1);
            }
            j += 1;
        }
        return None;
    }
    if q + 2 < n && c[q + 2] == '\'' && c[q + 1] != '\'' {
        paint(out, from, q + 3, Kind::Str);
        return Some(q + 3);
    }
    None
}

/// A string whose opening quote is at `q` (`from` includes any prefix letters).
/// Returns the index after what was consumed, or `None` if this is no string.
fn string_start(
    def: &LangDef,
    c: &[char],
    from: usize,
    q: usize,
    raw_ok: bool,
    st: &mut St,
    out: &mut [Kind],
) -> Option<usize> {
    let n = c.len();
    if raw_ok {
        let mut k = q;
        let mut h = 0;
        while k < n && c[k] == '#' {
            h += 1;
            k += 1;
        }
        if k < n && c[k] == '"' {
            paint(out, from, k + 1, Kind::Str);
            *st = St::Raw(h);
            return Some(k + 1);
        }
        if c[q] == '#' {
            return None;
        }
    }
    for (k, m) in def.multi.iter().enumerate() {
        if starts(c, q, m.open) {
            let ol = clen(m.open);
            paint(out, from, q + ol, Kind::Str);
            *st = St::Multi(k);
            return Some(q + ol);
        }
    }
    let qc = c[q];
    if qc == '\'' && def.rust_chars {
        return rust_char(c, from, q, out);
    }
    if !def.quotes.contains(&qc) {
        return None;
    }
    let esc = !(qc == '\'' && def.sq_raw);
    let mut j = q + 1;
    let mut closed = false;
    while j < n {
        if esc && c[j] == '\\' {
            j += 2;
            continue;
        }
        if c[j] == qc {
            j += 1;
            closed = true;
            break;
        }
        j += 1;
    }
    let end = j.min(n);
    let mut kind = Kind::Str;
    if closed && def.string_keys && c.get(skip_ws(c, end)) == Some(&':') {
        kind = Kind::Key;
    }
    paint(out, from, end, kind);
    Some(end)
}

fn number(c: &[char], i: usize, out: &mut [Kind]) -> usize {
    let n = c.len();
    let mut j = i;
    if c[j] == '0' && j + 1 < n && matches!(c[j + 1], 'x' | 'X' | 'b' | 'B' | 'o' | 'O') {
        j += 2;
        while j < n && (c[j].is_ascii_hexdigit() || c[j] == '_') {
            j += 1;
        }
    } else {
        while j < n && (c[j].is_ascii_digit() || c[j] == '_') {
            j += 1;
        }
        if j + 1 < n && c[j] == '.' && c[j + 1].is_ascii_digit() {
            j += 1;
            while j < n && (c[j].is_ascii_digit() || c[j] == '_') {
                j += 1;
            }
        }
        if j < n && (c[j] == 'e' || c[j] == 'E') {
            let mut k = j + 1;
            if k < n && (c[k] == '+' || c[k] == '-') {
                k += 1;
            }
            if k < n && c[k].is_ascii_digit() {
                j = k;
                while j < n && (c[j].is_ascii_digit() || c[j] == '_') {
                    j += 1;
                }
            }
        }
    }
    // suffix: u32, f64, UL, px, ...
    while j < n && (c[j].is_alphanumeric() || c[j] == '_') {
        j += 1;
    }
    paint(out, i, j, Kind::Number);
    j
}

fn dollar_var(def: &LangDef, c: &[char], i: usize, out: &mut [Kind]) -> usize {
    let n = c.len();
    let mut j = i + 1;
    if j < n && c[j] == '{' {
        j += 1;
        while j < n && c[j] != '}' {
            j += 1;
        }
        if j < n {
            j += 1;
        }
    } else if j < n && c[j] == '(' && def.paren_vars {
        j += 1;
        let mut depth = 1;
        while j < n && depth > 0 {
            match c[j] {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            j += 1;
        }
    } else if j < n && (c[j].is_alphabetic() || c[j] == '_') {
        while j < n && (c[j].is_alphanumeric() || c[j] == '_') {
            j += 1;
        }
    } else if j < n && (c[j].is_ascii_digit() || "@*#?!$-<^%+&".contains(c[j])) {
        j += 1;
    }
    paint(out, i, j, Kind::Var);
    j
}

// ───────────────────────────── line keys ─────────────────────────────

fn line_key(def: &LangDef, c: &[char]) -> Option<(usize, usize, Kind)> {
    let n = c.len();
    let s = skip_ws(c, 0);
    if s >= n {
        return None;
    }
    if def.ini_sections && c[s] == '[' {
        let mut e = n;
        while e > s && c[e - 1].is_whitespace() {
            e -= 1;
        }
        return if e > s + 1 && c[e - 1] == ']' { Some((s, e, Kind::Keyword)) } else { None };
    }
    if def.line_comments.iter().any(|p| starts(c, s, p)) {
        return None;
    }
    match def.keys {
        Keys::Yaml => yaml_key(c, s),
        Keys::Assign | Keys::Make => assign_key(def, c, s),
        Keys::None => None,
    }
}

fn yaml_key(c: &[char], s: usize) -> Option<(usize, usize, Kind)> {
    let n = c.len();
    let mut p = s;
    while p + 1 < n && c[p] == '-' && c[p + 1] == ' ' {
        p += 2;
        while p < n && c[p] == ' ' {
            p += 1;
        }
    }
    if p >= n || matches!(c[p], '"' | '\'' | '#' | '{' | '[' | '&' | '*' | '!' | '|' | '>' | '%' | '@') {
        return None;
    }
    let mut q = p;
    while q < n {
        if c[q] == ':' && (q + 1 == n || c[q + 1] == ' ') {
            break;
        }
        if c[q] == '#' && q > p && c[q - 1] == ' ' {
            return None;
        }
        q += 1;
    }
    if q >= n || q == p {
        return None;
    }
    let mut ke = q;
    while ke > p && c[ke - 1] == ' ' {
        ke -= 1;
    }
    Some((p, ke, Kind::Key))
}

const OPS: &str = ":?+!.";

/// `[:?+!.]{0,2}=` not followed by another `=`.
fn assign_at(c: &[char], p: usize) -> bool {
    let mut k = p;
    let mut ops = 0;
    while k < c.len() && OPS.contains(c[k]) && ops < 2 {
        k += 1;
        ops += 1;
    }
    k < c.len() && c[k] == '=' && c.get(k + 1) != Some(&'=')
}

fn assign_key(def: &LangDef, c: &[char], s: usize) -> Option<(usize, usize, Kind)> {
    let n = c.len();
    if def.keys == Keys::Make && c[0] == '\t' {
        return None; // recipe line
    }
    let end_of = |ch: char| {
        ch.is_whitespace() || matches!(ch, '=' | '"' | '\'' | ';' | '{' | '}' | '(' | ')' | ',')
    };
    let mut q = s;
    while q < n && !end_of(c[q]) {
        q += 1;
    }
    if q > s {
        let r = skip_ws(c, q);
        if assign_at(c, r) {
            let mut ke = q;
            if r == q {
                // operator characters glued to the name: `CC:=x`, `FOO+=x`
                let mut t = 0;
                while t < 2 && ke > s + 1 && OPS.contains(c[ke - 1]) {
                    ke -= 1;
                    t += 1;
                }
            }
            return Some((s, ke, Kind::Key));
        }
    }
    if def.keys == Keys::Make {
        let mut p = s;
        while p < n && !matches!(c[p], ':' | '=' | '#') {
            p += 1;
        }
        if p < n && p > s && c[p] == ':' && c.get(p + 1) != Some(&'=') {
            let mut ke = p;
            while ke > s && c[ke - 1].is_whitespace() {
                ke -= 1;
            }
            if ke > s {
                return Some((s, ke, Kind::Key));
            }
        }
    }
    None
}

// ───────────────────────────── markup ─────────────────────────────

fn scan_markup(c: &[char], st: &mut St, out: &mut [Kind]) {
    let n = c.len();
    let mut i = 0;
    while i < n {
        match *st {
            St::Block(_) => {
                let start = i;
                let mut done = false;
                while i < n {
                    if starts(c, i, "-->") {
                        i += 3;
                        done = true;
                        break;
                    }
                    i += 1;
                }
                paint(out, start, i, Kind::Comment);
                if done {
                    *st = St::Normal;
                }
                continue;
            }
            St::TagStr(q) => {
                let start = i;
                let mut done = false;
                while i < n {
                    if c[i] == q {
                        i += 1;
                        done = true;
                        break;
                    }
                    i += 1;
                }
                paint(out, start, i, Kind::Str);
                if done {
                    *st = St::Tag;
                }
                continue;
            }
            St::Tag => {
                let ch = c[i];
                if ch == '>' {
                    *st = St::Normal;
                    i += 1;
                } else if (ch == '/' || ch == '?') && c.get(i + 1) == Some(&'>') {
                    *st = St::Normal;
                    i += 2;
                } else if ch == '"' || ch == '\'' {
                    let start = i;
                    i += 1;
                    let mut done = false;
                    while i < n {
                        if c[i] == ch {
                            i += 1;
                            done = true;
                            break;
                        }
                        i += 1;
                    }
                    paint(out, start, i, Kind::Str);
                    if !done {
                        *st = St::TagStr(ch);
                    }
                } else if ch.is_alphabetic() || ch == '_' || ch == ':' {
                    let start = i;
                    i += 1;
                    while i < n && (c[i].is_alphanumeric() || matches!(c[i], '_' | '-' | ':' | '.')) {
                        i += 1;
                    }
                    paint(out, start, i, Kind::Key);
                } else {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        if starts(c, i, "<!--") {
            paint(out, i, i + 4, Kind::Comment);
            i += 4;
            *st = St::Block(1);
            continue;
        }
        if c[i] == '<' {
            let mut j = i + 1;
            if j < n && matches!(c[j], '/' | '!' | '?') {
                j += 1;
            }
            if j < n && (c[j].is_alphabetic() || c[j] == '_') {
                let ns = j;
                while j < n && (c[j].is_alphanumeric() || matches!(c[j], '_' | '-' | ':' | '.')) {
                    j += 1;
                }
                paint(out, ns, j, Kind::Type);
                *st = St::Tag;
                i = j;
                continue;
            }
        }
        if c[i] == '&' {
            let mut j = i + 1;
            while j < n && j < i + 10 && (c[j].is_alphanumeric() || c[j] == '#') {
                j += 1;
            }
            if j < n && c[j] == ';' && j > i + 1 {
                paint(out, i, j + 1, Kind::Const);
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
}

// ───────────────────────────── markdown ─────────────────────────────

fn scan_markdown(c: &[char], st: &mut St, out: &mut [Kind]) {
    let n = c.len();
    let mut s = 0;
    while s < n && c[s] == ' ' {
        s += 1;
    }
    let fence = starts(c, s, "```") || starts(c, s, "~~~");
    if *st == St::Fence {
        paint(out, 0, n, Kind::Str);
        if fence {
            *st = St::Normal;
        }
        return;
    }
    if fence {
        paint(out, 0, n, Kind::Str);
        *st = St::Fence;
        return;
    }
    if s < n && c[s] == '#' {
        let mut h = s;
        while h < n && c[h] == '#' {
            h += 1;
        }
        if h - s <= 6 && (h == n || c[h] == ' ') {
            paint(out, s, n, Kind::Keyword);
            return;
        }
    }
    if s < n && c[s] == '>' {
        paint(out, 0, n, Kind::Comment);
        return;
    }
    let mut i = s;
    if i + 1 < n && matches!(c[i], '-' | '*' | '+') && c[i + 1] == ' ' {
        paint(out, i, i + 1, Kind::Attr);
        i += 2;
    } else {
        let mut d = i;
        while d < n && c[d].is_ascii_digit() {
            d += 1;
        }
        if d > i && d + 1 < n && matches!(c[d], '.' | ')') && c[d + 1] == ' ' {
            paint(out, i, d + 1, Kind::Attr);
            i = d + 2;
        }
    }
    while i < n {
        match c[i] {
            '`' => {
                let mut ticks = 0;
                while i + ticks < n && c[i + ticks] == '`' {
                    ticks += 1;
                }
                let mut j = i + ticks;
                let mut found = None;
                while j < n {
                    if c[j] == '`' {
                        let mut t = 0;
                        while j + t < n && c[j + t] == '`' {
                            t += 1;
                        }
                        if t == ticks {
                            found = Some(j + t);
                            break;
                        }
                        j += t;
                    } else {
                        j += 1;
                    }
                }
                match found {
                    Some(e) => {
                        paint(out, i, e, Kind::Str);
                        i = e;
                    }
                    None => i += ticks,
                }
            }
            '[' => {
                let mut k = i + 1;
                while k < n && c[k] != ']' {
                    k += 1;
                }
                if k < n {
                    paint(out, i, k + 1, Kind::Func);
                    i = k + 1;
                    if i < n && c[i] == '(' {
                        let mut m = i + 1;
                        while m < n && c[m] != ')' {
                            m += 1;
                        }
                        if m < n {
                            paint(out, i, m + 1, Kind::Str);
                            i = m + 1;
                        }
                    }
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn lang(file: &str) -> Lang {
        detect(&PathBuf::from(file), None).unwrap_or_else(|| panic!("no language for {file}"))
    }

    /// Non-plain runs of one line as "Kind:text".
    fn runs(text: &str, kinds: &[Kind]) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let k = kinds[i];
            let mut j = i;
            while j < chars.len() && kinds[j] == k {
                j += 1;
            }
            if k != Kind::Plain {
                out.push(format!("{:?}:{}", k, chars[i..j].iter().collect::<String>()));
            }
            i = j;
        }
        out
    }

    /// Highlight a whole snippet; returns the runs of every line.
    fn hl(file: &str, src: &str) -> Vec<Vec<String>> {
        let lines: Vec<String> = src.lines().map(String::from).collect();
        let k = highlight_lines(lang(file), &lines).unwrap();
        lines.iter().zip(&k).map(|(l, k)| runs(l, k)).collect()
    }

    fn one(file: &str, line: &str) -> Vec<String> {
        hl(file, line).remove(0)
    }

    #[test]
    fn detection() {
        let d = |p: &str, first: Option<&str>| detect(Path::new(p), first).map(|l| l.name);
        assert_eq!(d("src/main.rs", None), Some("Rust"));
        assert_eq!(d("A.RS", None), Some("Rust"));
        assert_eq!(d("x.h", None), Some("C"));
        assert_eq!(d("x.hpp", None), Some("C++"));
        assert_eq!(d("Makefile", None), Some("Makefile"));
        assert_eq!(d("Makefile.am", None), Some("Makefile"));
        assert_eq!(d("Dockerfile.prod", None), Some("Dockerfile"));
        assert_eq!(d("CMakeLists.txt", None), Some("CMake"));
        assert_eq!(d("Kconfig.zephyr", None), Some("Kconfig"));
        assert_eq!(d("prj.conf", None), Some("Config"));
        assert_eq!(d("board.overlay", None), Some("Devicetree"));
        assert_eq!(d("recipe_1.0.bbappend", None), Some("BitBake"));
        assert_eq!(d("flake.nix", None), Some("Nix"));
        assert_eq!(d(".bashrc", None), Some("Shell"));
        assert_eq!(d("notes.txt", None), None);
        assert_eq!(d("noext", None), None);
        // shebang only when nothing else matches
        assert_eq!(d("init", Some("#!/bin/sh")), Some("Shell"));
        assert_eq!(d("init", Some("#!/usr/bin/env python3")), Some("Python"));
        assert_eq!(d("init", Some("#!/usr/bin/env -S node --x")), Some("JavaScript"));
        assert_eq!(d("init", Some("#!/usr/bin/env FOO=1 bash")), Some("Shell"));
        assert_eq!(d("init", Some("#!/usr/bin/awk -f")), None);
        assert_eq!(d("init", Some("not a shebang")), None);
        assert_eq!(d("run.py", Some("#!/bin/sh")), Some("Python"));
    }

    #[test]
    fn no_extension_is_claimed_twice() {
        let mut seen: Vec<(&str, &str)> = Vec::new();
        for l in LANGS {
            for e in l.exts {
                assert!(
                    !seen.iter().any(|(x, _)| x == e),
                    "extension {e} is in {} and another language",
                    l.name
                );
                seen.push((e, l.name));
            }
            assert!(!l.name.is_empty());
        }
    }

    #[test]
    fn rust_basics() {
        assert_eq!(one("a.rs", "let x = 5;"), ["Keyword:let", "Number:5"]);
        assert_eq!(
            one("a.rs", "pub fn add(a: u32, b: u32) -> Result<Vec<String>, Error> {"),
            [
                "Keyword:pub", "Keyword:fn", "Func:add", "Type:u32", "Type:u32", "Type:Result",
                "Type:Vec", "Type:String", "Type:Error"
            ]
        );
        assert_eq!(one("a.rs", "x // note"), ["Comment:// note"]);
        assert_eq!(one("a.rs", r#"let s = "a\"b";"#), ["Keyword:let", r#"Str:"a\"b""#]);
        assert_eq!(one("a.rs", "println!(\"hi {}\", 1_000u64);"), ["Attr:println!", "Str:\"hi {}\"", "Number:1_000u64"]);
        assert_eq!(one("a.rs", "#[derive(Debug)]"), ["Attr:#[derive(Debug)]"]);
        assert_eq!(one("a.rs", "#![allow(dead_code)]"), ["Attr:#![allow(dead_code)]"]);
        assert_eq!(one("a.rs", "if a != b {}"), ["Keyword:if"]);
        assert_eq!(one("a.rs", "x == y && z"), Vec::<String>::new());
        assert_eq!(one("a.rs", "const MAX: usize = 0xFF;"), ["Keyword:const", "Const:MAX", "Type:usize", "Number:0xFF"]);
        assert_eq!(one("a.rs", "1..2"), ["Number:1", "Number:2"]);
        assert_eq!(one("a.rs", "let f = 1.5e-3;"), ["Keyword:let", "Number:1.5e-3"]);
        assert_eq!(one("a.rs", "true"), ["Const:true"]);
    }

    #[test]
    fn rust_chars_and_lifetimes() {
        assert_eq!(one("a.rs", "let c = 'x';"), ["Keyword:let", "Str:'x'"]);
        assert_eq!(one("a.rs", r"let c = '\n';"), ["Keyword:let", r"Str:'\n'"]);
        assert_eq!(one("a.rs", r"let c = '\'';"), ["Keyword:let", r"Str:'\''"]);
        assert_eq!(one("a.rs", r"let c = '\u{1F600}';"), ["Keyword:let", r"Str:'\u{1F600}'"]);
        assert_eq!(one("a.rs", "let c = b'a';"), ["Keyword:let", "Str:b'a'"]);
        assert_eq!(
            one("a.rs", "fn f<'a>(x: &'a str) -> &'static str {"),
            ["Keyword:fn", "Attr:'a", "Attr:'a", "Type:str", "Attr:'static", "Type:str"]
        );
    }

    #[test]
    fn rust_raw_and_multiline() {
        let src = "let a = r#\"raw \"quoted\" text\nsecond line\"# + 1;\nlet b = \"open\ncontinued\";\nlet c = 2;";
        let r = hl("a.rs", src);
        assert_eq!(r[0], ["Keyword:let", "Str:r#\"raw \"quoted\" text"]);
        assert_eq!(r[1], ["Str:second line\"#", "Number:1"]);
        assert_eq!(r[2], ["Keyword:let", "Str:\"open"]);
        assert_eq!(r[3], ["Str:continued\""]);
        assert_eq!(r[4], ["Keyword:let", "Number:2"]);
        // r"..." with no hashes, and a raw identifier is not a string
        assert_eq!(one("a.rs", "r\"a\\\" b"), ["Str:r\"a\\\""]);
        assert_eq!(one("a.rs", "let r#type = 1;"), ["Keyword:let", "Number:1"]);
    }

    #[test]
    fn rust_nested_block_comments() {
        let r = hl("a.rs", "a /* one /* two */ still\nstill */ b\n/* x */ c");
        assert_eq!(r[0], ["Comment:/* one /* two */ still"]);
        assert_eq!(r[1], ["Comment:still */"]);
        assert_eq!(r[2], ["Comment:/* x */"]);
    }

    #[test]
    fn c_and_preprocessor() {
        assert_eq!(
            one("a.c", "#include <stdio.h>"),
            ["Attr:#include", "Str:<stdio.h>"]
        );
        assert_eq!(one("a.c", "#include \"my.h\""), ["Attr:#include", "Str:\"my.h\""]);
        assert_eq!(one("a.c", "  #  define MAX 10"), ["Attr:#  define", "Const:MAX", "Number:10"]);
        assert_eq!(
            one("a.c", "static int count = 0; // total"),
            ["Keyword:static", "Type:int", "Number:0", "Comment:// total"]
        );
        assert_eq!(one("a.c", "x = a # b;"), Vec::<String>::new()); // not at line start
        assert_eq!(one("a.c", "char c = '\\0';"), ["Type:char", "Str:'\\0'"]);
        assert_eq!(one("a.c", "return NULL;"), ["Keyword:return", "Const:NULL"]);
        assert_eq!(one("a.c", "printf(\"%d\\n\", x);"), ["Func:printf", "Str:\"%d\\n\""]);
        let r = hl("a.c", "/* a\n * b */ int x;");
        assert_eq!(r[0], ["Comment:/* a"]);
        assert_eq!(r[1], ["Comment: * b */", "Type:int"]);
    }

    #[test]
    fn python() {
        let src = "def f(self, x: int = 3):\n    \"\"\"doc\n    string\"\"\"  # trailing\n    return f'{x}' + r\"\\d\"\n@property\nclass Foo(Base): pass";
        let r = hl("a.py", src);
        assert_eq!(r[0], ["Keyword:def", "Func:f", "Keyword:self", "Type:int", "Number:3"]);
        assert_eq!(r[1], ["Str:\"\"\"doc"]);
        assert_eq!(r[2], ["Str:    string\"\"\"", "Comment:# trailing"]);
        assert_eq!(r[3], ["Keyword:return", "Str:f'{x}'", "Str:r\"\\d\""]);
        assert_eq!(r[4], ["Attr:@property"]);
        assert_eq!(r[5], ["Keyword:class", "Func:Foo", "Type:Base", "Keyword:pass"]);
        assert_eq!(one("a.py", "x = None"), ["Const:None"]);
        // escaped quote inside a triple-quoted string does not end it
        let r = hl("a.py", "s = \"\"\"a \\\"\"\" b\nc\"\"\"");
        assert_eq!(r[0], ["Str:\"\"\"a \\\"\"\" b"]);
        assert_eq!(r[1], ["Str:c\"\"\""]);
    }

    #[test]
    fn javascript_and_typescript() {
        assert_eq!(
            one("a.js", "const x = `a${b}c`; // c"),
            ["Keyword:const", "Str:`a${b}c`", "Comment:// c"]
        );
        let r = hl("a.js", "let t = `line1\nline2`;\nfoo(1);");
        assert_eq!(r[0], ["Keyword:let", "Str:`line1"]);
        assert_eq!(r[1], ["Str:line2`"]);
        assert_eq!(r[2], ["Func:foo", "Number:1"]);
        assert_eq!(
            one("a.ts", "interface Foo { a: string; b?: number }"),
            ["Keyword:interface", "Type:Foo", "Type:string", "Type:number"]
        );
        assert_eq!(one("a.js", "return undefined"), ["Keyword:return", "Const:undefined"]);
    }

    #[test]
    fn go_and_java() {
        let r = hl("a.go", "s := `raw\nstuff`\nfunc main() {}");
        assert_eq!(r[0], ["Str:`raw"]);
        assert_eq!(r[1], ["Str:stuff`"]);
        assert_eq!(r[2], ["Keyword:func", "Func:main"]);
        assert_eq!(
            one("A.java", "@Override public void run() {}"),
            ["Attr:@Override", "Keyword:public", "Type:void", "Func:run"]
        );
        let r = hl("A.java", "String s = \"\"\"\n  hi\n  \"\"\";");
        assert_eq!(r[0], ["Type:String", "Str:\"\"\""]);
        assert_eq!(r[1], ["Str:  hi"]);
        assert_eq!(r[2], ["Str:  \"\"\""]);
    }

    #[test]
    fn shell() {
        assert_eq!(
            one("a.sh", "FOO=bar echo \"$HOME/x\" # note"),
            ["Key:FOO", "Str:\"$HOME/x\"", "Comment:# note"]
        );
        assert_eq!(one("a.sh", "echo $# ${x:-1} $(date) $1"), ["Var:$#", "Var:${x:-1}", "Var:$", "Var:$1"]);
        assert_eq!(one("a.sh", "echo a#b"), Vec::<String>::new()); // # inside a word
        assert_eq!(one("a.sh", "if [ -f x ]; then"), ["Keyword:if", "Keyword:then"]);
        assert_eq!(one("a.sh", "[ \"$a\" = \"b\" ]"), ["Str:\"$a\"", "Str:\"b\""]);
        assert_eq!(one("a.sh", "export PATH=\"$PATH:/x\""), ["Keyword:export", "Str:\"$PATH:/x\""]);
        // a single-quoted string keeps backslashes literally
        assert_eq!(one("a.sh", "echo 'a\\' b"), ["Str:'a\\'"]);
        // double quotes continue across lines
        let r = hl("a.sh", "echo \"one\ntwo\" done\nls");
        assert_eq!(r[0], ["Str:\"one"]);
        assert_eq!(r[1], ["Str:two\"", "Keyword:done"]);
        assert_eq!(r[2], Vec::<String>::new());
        assert_eq!(one("a.sh", "# just a comment"), ["Comment:# just a comment"]);
    }

    #[test]
    fn make_and_bitbake() {
        let r = hl(
            "Makefile",
            "CC := gcc\nCFLAGS+=-O2\n.PHONY: all\nall: main.o\n\t$(CC) -o $@ $<  # link\nifeq ($(X),1)\nendif",
        );
        assert_eq!(r[0], ["Key:CC"]);
        assert_eq!(r[1], ["Key:CFLAGS"]);
        assert_eq!(r[2], ["Key:.PHONY"]);
        assert_eq!(r[3], ["Key:all"]);
        assert_eq!(r[4], ["Var:$(CC)", "Var:$@", "Var:$<", "Comment:# link"]);
        assert_eq!(r[5], ["Keyword:ifeq", "Var:$(X)", "Number:1"]);
        assert_eq!(r[6], ["Keyword:endif"]);
        let r = hl(
            "x.bb",
            "inherit cmake\nDEPENDS:append = \" zlib\"\nSRC_URI ?= \"git://x\"\ndo_install() {\n}",
        );
        assert_eq!(r[0], ["Keyword:inherit"]);
        assert_eq!(r[1], ["Key:DEPENDS:append", "Str:\" zlib\""]);
        assert_eq!(r[2], ["Key:SRC_URI", "Str:\"git://x\""]);
        assert_eq!(r[3], Vec::<String>::new());
    }

    #[test]
    fn config_files() {
        let r = hl("prj.conf", "# comment\nCONFIG_GPIO=y\nCONFIG_NAME=\"x\"\n[section]\nkey = 5");
        assert_eq!(r[0], ["Comment:# comment"]);
        assert_eq!(r[1], ["Key:CONFIG_GPIO", "Const:y"]);
        assert_eq!(r[2], ["Key:CONFIG_NAME", "Str:\"x\""]);
        assert_eq!(r[3], ["Keyword:[section]"]);
        assert_eq!(r[4], ["Key:key", "Number:5"]);
        let r = hl("c.toml", "[package]\nname = \"x\"\nversion = '1.0'\nok = true\ndeps = { a = 1 }\n# note");
        assert_eq!(r[0], ["Keyword:[package]"]);
        assert_eq!(r[1], ["Key:name", "Str:\"x\""]);
        assert_eq!(r[2], ["Key:version", "Str:'1.0'"]);
        assert_eq!(r[3], ["Key:ok", "Const:true"]);
        assert_eq!(r[4], ["Key:deps", "Number:1"]);
        assert_eq!(r[5], ["Comment:# note"]);
        assert_eq!(
            one("Kconfig", "config FOO"),
            ["Keyword:config"]
        );
    }

    #[test]
    fn json_and_yaml() {
        let r = hl("a.json", "{\n  \"name\": \"x\",\n  \"n\": [1, -2.5e3, true, null]\n}");
        assert_eq!(r[1], ["Key:\"name\"", "Str:\"x\""]);
        assert_eq!(r[2], ["Key:\"n\"", "Number:1", "Number:2.5e3", "Const:true", "Const:null"]);
        let r = hl(
            "a.yml",
            "# c\nname: build\non:\n  push:\n    branches: [main]\n- run: echo hi # x\n  url: http://x.y/z\nmsg: \"it's\"\nlist:\n  - a\n  - b: 2",
        );
        assert_eq!(r[0], ["Comment:# c"]);
        assert_eq!(r[1], ["Key:name"]);
        assert_eq!(r[2], ["Key:on"]);
        assert_eq!(r[3], ["Key:push"]);
        assert_eq!(r[4], ["Key:branches"]);
        assert_eq!(r[5], ["Key:run", "Comment:# x"]);
        assert_eq!(r[6], ["Key:url"]);
        assert_eq!(r[7], ["Key:msg", "Str:\"it's\""]);
        assert_eq!(r[9], Vec::<String>::new());
        assert_eq!(r[10], ["Key:b", "Number:2"]);
        // an apostrophe inside a word is not a string
        assert_eq!(one("a.yml", "text: don't stop"), ["Key:text"]);
    }

    #[test]
    fn nix_dts_cmake_sql_css_lua() {
        let r = hl("a.nix", "{ pkgs, ... }:\nlet\n  name = \"x-${v}\";\n  s = ''\n    multi\n  '';\nin {\n  buildInputs = [ pkgs.foo ]; # c\n}");
        assert_eq!(r[2], ["Key:name", "Str:\"x-${v}\""]);
        assert_eq!(r[3], ["Key:s", "Str:''"]);
        assert_eq!(r[4], ["Str:    multi"]);
        assert_eq!(r[5], ["Str:  ''"]);
        assert_eq!(r[7], ["Key:buildInputs", "Comment:# c"]);
        let r = hl(
            "a.dts",
            "/dts-v1/;\n#include <dt-bindings/gpio/gpio.h>\n&uart0 {\n\tstatus = \"okay\";\n\t#address-cells = <1>;\n\treg = <0x1000 0x20>;\n};",
        );
        assert_eq!(r[0], ["Attr:/dts-v1/"]);
        assert_eq!(r[1], ["Attr:#include", "Str:<dt-bindings/gpio/gpio.h>"]);
        assert_eq!(r[2], ["Var:&uart0"]);
        assert_eq!(r[3], ["Key:status", "Str:\"okay\""]);
        assert_eq!(r[4], ["Key:#address-cells", "Number:1"]);
        assert_eq!(r[5], ["Key:reg", "Number:0x1000", "Number:0x20"]);
        let r = hl("CMakeLists.txt", "cmake_minimum_required(VERSION 3.20)\nIF(${X} AND ON)\n  add_executable(app \"main.c\") # go\nendif()");
        assert_eq!(r[0], ["Func:cmake_minimum_required", "Number:3.20"]);
        assert_eq!(r[1], ["Keyword:IF", "Var:${X}", "Const:ON"]);
        assert_eq!(r[2], ["Func:add_executable", "Str:\"main.c\"", "Comment:# go"]);
        assert_eq!(r[3], ["Keyword:endif"]);
        assert_eq!(
            one("a.sql", "SELECT name, COUNT(*) FROM t WHERE id = 'it''s' -- x"),
            ["Keyword:SELECT", "Func:COUNT", "Keyword:FROM", "Keyword:WHERE", "Str:'it''s'", "Comment:-- x"]
        );
        let r = hl("a.css", "a:hover {\n  color: #fff; /* c */\n  margin: 10px auto;\n  background: url(\"x.png\");\n}\n@media (max-width: 600px) {}");
        assert_eq!(r[1], ["Key:color", "Number:#fff", "Comment:/* c */"]);
        assert_eq!(r[2], ["Key:margin", "Number:10px"]);
        assert_eq!(r[3], ["Key:background", "Func:url", "Str:\"x.png\""]);
        assert_eq!(r[5], ["Attr:@media", "Number:600px"]);
        let r = hl("a.lua", "local x = [[a\nb]] -- c\n--[[ block\nmore ]] print(1)");
        assert_eq!(r[0], ["Keyword:local", "Str:[[a"]);
        assert_eq!(r[1], ["Str:b]]", "Comment:-- c"]);
        assert_eq!(r[2], ["Comment:--[[ block"]);
        assert_eq!(r[3], ["Comment:more ]]", "Func:print", "Number:1"]);
    }

    #[test]
    fn markup_and_markdown() {
        let r = hl(
            "a.html",
            "<!DOCTYPE html>\n<div class=\"a b\" id='x'\n  data-n=\"1\">&amp; text <!-- c\nmore --></div>",
        );
        assert_eq!(r[0], ["Type:DOCTYPE", "Key:html"]);
        assert_eq!(r[1], ["Type:div", "Key:class", "Str:\"a b\"", "Key:id", "Str:'x'"]);
        assert_eq!(r[2], ["Key:data-n", "Str:\"1\"", "Const:&amp;", "Comment:<!-- c"]);
        assert_eq!(r[3], ["Comment:more -->", "Type:div"]);
        let r = hl(
            "a.md",
            "# Title\n- item with `code` and [link](http://x)\n1. second\n> quote\n```sh\n# not a heading\n```\ntext",
        );
        assert_eq!(r[0], ["Keyword:# Title"]);
        assert_eq!(r[1], ["Attr:-", "Str:`code`", "Func:[link]", "Str:(http://x)"]);
        assert_eq!(r[2], ["Attr:1."]);
        assert_eq!(r[3], ["Comment:> quote"]);
        assert_eq!(r[4], ["Str:```sh"]);
        assert_eq!(r[5], ["Str:# not a heading"]);
        assert_eq!(r[6], ["Str:```"]);
        assert_eq!(r[7], Vec::<String>::new());
        assert_eq!(one("a.md", "####### seven"), Vec::<String>::new());
        assert_eq!(one("a.md", "#hashtag"), Vec::<String>::new());
    }

    #[test]
    fn output_shape_and_limits() {
        let lines: Vec<String> = vec!["héllo \"wörld\" // ünï".into(), "".into(), "\t x".into()];
        let k = highlight_lines(lang("a.rs"), &lines).unwrap();
        assert_eq!(k.len(), 3);
        for (l, kk) in lines.iter().zip(&k) {
            assert_eq!(kk.len(), l.chars().count());
        }
        // a very long line is left plain
        let long = vec!["a".repeat(MAX_LINE + 1)];
        assert!(highlight_lines(lang("a.rs"), &long).unwrap()[0].iter().all(|k| *k == Kind::Plain));
        // too much text: no highlighting at all
        let big: Vec<String> = (0..(MAX_TOTAL / 100 + 10)).map(|_| "x".repeat(100)).collect();
        assert!(highlight_lines(lang("a.rs"), &big).is_none());
        assert!(highlight_lines(lang("a.rs"), &[]).unwrap().is_empty());
    }

    #[test]
    fn highlight_file_picks_the_language() {
        let lines = vec!["fn main() {}".to_string()];
        let (name, k) = highlight_file(Path::new("m.rs"), None, &lines).unwrap();
        assert_eq!(name, "Rust");
        assert_eq!(k[0][0], Kind::Keyword);
        assert!(highlight_file(Path::new("m.txt"), None, &lines).is_none());
        assert_eq!(
            highlight_file(Path::new("run"), Some("#!/usr/bin/python3"), &lines).unwrap().0,
            "Python"
        );
    }

    #[test]
    fn never_panics_and_always_matches_lengths() {
        let alphabet: Vec<char> = "ab_ 9\t\"'`\\/*#-<>[](){}$@&:=;,.!?%é\n".chars().collect();
        let words = ["fn", "let", "\"\"\"", "/*", "*/", "<!--", "-->", "r#\"", "'''", "[[", "]]", "''", "${", "$(", "#[", "```"];
        let mut s = 0x9E3779B97F4A7C15u64;
        let mut rnd = |m: usize| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 33) as usize) % m
        };
        for l in LANGS {
            for _ in 0..120 {
                let nlines = 1 + rnd(6);
                let lines: Vec<String> = (0..nlines)
                    .map(|_| {
                        let mut t = String::new();
                        for _ in 0..rnd(30) {
                            if rnd(5) == 0 {
                                t.push_str(words[rnd(words.len())]);
                            } else {
                                t.push(alphabet[rnd(alphabet.len())]);
                            }
                        }
                        t
                    })
                    .collect();
                let k = highlight_lines(l, &lines).unwrap();
                assert_eq!(k.len(), lines.len(), "{}", l.name);
                for (line, kk) in lines.iter().zip(&k) {
                    assert_eq!(kk.len(), line.chars().count(), "{}: {line:?}", l.name);
                }
            }
        }
    }
}
