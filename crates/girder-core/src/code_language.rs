//! The file-extension to language table every code surface shares.
//!
//! One table rather than one per surface: a preview, a diff view, and a read tool that each decided
//! independently would disagree about what a `.m` file is, and the disagreement would look like a bug
//! in whichever surface the user happened to open second.
//!
//! # Why the id is a grammar name
//!
//! The value is what a highlighter resolves, not what a human reads. `kotlin` is a grammar id; "Kotlin
//! source" is a label, and a label is the consumer's business. A table that stored labels would force
//! every consumer to map them back.
//!
//! # Why short ids for a recorded hint
//!
//! A session records a hint when it reads a file, so the value must be stable across versions — a
//! recorded `yaml` stays meaningful, while a recorded `YAML Configuration` would break the day someone
//! renamed it. [`read_hint`] is that projection, and for most languages it is the grammar id itself.

/// The language for one file extension.
///
/// Extensions are lowercase and carry no dot. A path that is not in the table renders as plain text,
/// which is the safe default: guessing would highlight a language the file is not.
#[must_use]
pub fn for_extension(extension: &str) -> Option<&'static str> {
    let extension = extension.trim_start_matches('.').to_ascii_lowercase();
    Some(match extension.as_str() {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "jsx",
        "py" | "pyi" => "python",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "scala" => "scala",
        "sh" | "bash" | "zsh" => "bash",
        "ps1" | "psm1" => "powershell",
        "lua" => "lua",
        "pl" | "pm" => "perl",
        "r" => "r",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "hs" => "haskell",
        "ml" | "mli" => "ocaml",
        "clj" | "cljs" => "clojure",
        "vim" => "viml",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "sass" => "sass",
        "less" => "less",
        "vue" => "vue",
        "svelte" => "svelte",
        "json" => "json",
        "jsonc" => "jsonc",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "ini" | "cfg" => "ini",
        "xml" => "xml",
        "md" | "markdown" => "markdown",
        "rst" => "rst",
        "tex" => "latex",
        "diff" | "patch" => "diff",
        "dockerfile" => "dockerfile",
        "makefile" | "mk" => "makefile",
        "gradle" => "groovy",
        "proto" => "protobuf",
        "graphql" | "gql" => "graphql",
        "tf" | "hcl" => "hcl",
        "nix" => "nix",
        "zig" => "zig",
        "dart" => "dart",
        "asm" | "s" => "asm",
        _ => return None,
    })
}

/// The language for one path, by its final extension.
///
/// The whole name is checked first so `Dockerfile`-style names work, then the extension. A name with no
/// extension and no match — `Makefile`, for instance — stays unlisted rather than guessed.
#[must_use]
pub fn for_path(path: &str) -> Option<&'static str> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    // The base name is taken before the extension is read: `for_extension("src/main.rs")` would look up
    // the whole path as an extension and find nothing.
    if let Some((_, extension)) = name.rsplit_once('.')
        && let Some(language) = for_extension(extension)
    {
        return Some(language);
    }
    if let Some(language) = for_extension(name) {
        return Some(language);
    }
    match name.to_ascii_lowercase().as_str() {
        "dockerfile" => Some("dockerfile"),
        "makefile" | "gnumakefile" => Some("makefile"),
        "cmakelists.txt" => Some("cmake"),
        _ => None,
    }
}

/// The short id recorded in a session when a file is read.
///
/// Stable across versions, because a recorded hint outlives the code that wrote it. For most languages
/// this is the grammar id; `tsx` and `jsx` collapse onto their base language because a reader that only
/// wants to know "is this JavaScript" does not need the variant.
#[must_use]
pub fn read_hint(path: &str) -> Option<&'static str> {
    Some(match for_path(path)? {
        "tsx" => "typescript",
        "jsx" => "javascript",
        "jsonc" => "json",
        other => other,
    })
}
