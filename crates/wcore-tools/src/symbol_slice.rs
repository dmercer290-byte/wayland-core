//! Token-opt (semantic slicing): resolve a named symbol in a source file to a
//! line window, so the Read tool can return just that symbol instead of the
//! whole file.
//!
//! Model-driven ONLY: this fires when the model passes `symbol=` to Read. A
//! bare full-file Read is NEVER auto-sliced — log/error/config files carry lines
//! (denied permissions, stack frames) a safety or debugging decision needs, and
//! silently hiding them would be wrong.
//!
//! Symbol START lines come from `wcore-repomap`'s regex extractor (Rust +
//! TS/JS). The END line is computed here by a string/comment-aware brace
//! matcher, so a `"}"` inside a string literal or a `//` comment never closes a
//! block early. Single-line items (`use`, `type X = …;`, `const`) end at their
//! terminating `;`.

use std::path::Path;

use wcore_repomap::extractor::extract;
use wcore_repomap::{Language, SymbolKind};

/// How far forward the brace matcher will scan before giving up (a runaway
/// guard for pathologically unbalanced or minified input).
const MAX_BLOCK_LINES: usize = 4000;
/// Cap on how many available names to list when a symbol isn't found.
const MAX_AVAILABLE: usize = 40;

/// Outcome of resolving a `symbol=` request against a file's source.
#[derive(Debug, PartialEq, Eq)]
pub enum SymbolSlice {
    /// Resolved to a 1-based inclusive line window.
    Found {
        /// First line to show (includes contiguous doc-comments/attributes
        /// immediately above the declaration).
        start: usize,
        /// Last line of the symbol body.
        end: usize,
        kind: SymbolKind,
        /// More than one symbol shares this name; we resolved the first.
        multiple: bool,
    },
    /// No symbol with that name; carries available names for the model to retry.
    NotFound { available: Vec<String> },
    /// The file's language has no symbol extractor (`Language::Other`).
    Unsupported,
}

/// Resolve `want` to a line window in `source` (the file at `path`).
pub fn resolve_symbol(path: &Path, source: &str, want: &str) -> SymbolSlice {
    let language = Language::from_path(path);
    if language == Language::Other {
        return SymbolSlice::Unsupported;
    }

    let (symbols, _imports) = extract(language, source);

    let matches: Vec<_> = symbols.iter().filter(|s| s.name == want).collect();
    let Some(first) = matches.iter().min_by_key(|s| s.line) else {
        let mut available: Vec<String> = symbols.iter().map(|s| s.name.clone()).collect();
        available.dedup();
        available.truncate(MAX_AVAILABLE);
        return SymbolSlice::NotFound { available };
    };

    let lines: Vec<&str> = source.lines().collect();
    let decl_line = first.line; // 1-based
    let start = expand_up(&lines, decl_line);
    let end = block_end(&lines, decl_line);

    SymbolSlice::Found {
        start,
        end,
        kind: first.kind,
        multiple: matches.len() > 1,
    }
}

/// Walk upward from the declaration line over contiguous doc-comments (`///`,
/// `//!`) and attributes (`#[…]`, `#![…]`) so the slice carries the symbol's
/// documentation and attributes. Plain `//` comments and blank lines stop the
/// walk (they aren't reliably part of the symbol).
fn expand_up(lines: &[&str], decl_line: usize) -> usize {
    let mut start = decl_line;
    while start > 1 {
        let above = lines[start - 2].trim_start(); // start-2 = 0-based line above
        if above.starts_with("///")
            || above.starts_with("//!")
            || above.starts_with("#[")
            || above.starts_with("#![")
        {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

/// Compute the 1-based inclusive end line of the symbol declared at
/// `decl_line`, using a string/comment-aware character scan.
///
/// Block items (their first `{` opens a body): end at the matching `}`.
/// Single-line items (no brace before the first top-level `;`): end at that `;`.
/// Falls back to `decl_line` if neither is found within `MAX_BLOCK_LINES`.
fn block_end(lines: &[&str], decl_line: usize) -> usize {
    let mut state = ScanState::default();
    let last = lines.len().min(decl_line + MAX_BLOCK_LINES);

    // 0-based iteration from the declaration line to `last`.
    for (offset, &line) in lines[decl_line - 1..last].iter().enumerate() {
        let line_no = decl_line + offset;
        for ch in line.chars() {
            match state.step(ch) {
                Step::BlockClosed => return line_no,
                Step::StatementEnd if !state.opened_block => return line_no,
                _ => {}
            }
        }
        // Newline resets a line comment but not block comments / strings.
        state.end_of_line();
    }
    // Unterminated within the cap: show through the last scanned line.
    last.max(decl_line)
}

/// What a single character did to the scan.
enum Step {
    None,
    /// A `}` brought brace depth back to zero after a block had opened.
    BlockClosed,
    /// A `;` at depth zero before any block opened (single-line item).
    StatementEnd,
}

/// Minimal lexer state: enough to count braces while ignoring strings, char
/// literals, and comments. Not a full parser — good enough to bound a symbol.
#[derive(Default)]
struct ScanState {
    depth: i32,
    opened_block: bool,
    in_line_comment: bool,
    in_block_comment: bool,
    in_string: bool,
    in_char: bool,
    escaped: bool,
    prev: char,
}

impl ScanState {
    fn step(&mut self, ch: char) -> Step {
        // Inside a line comment: ignore everything until end_of_line().
        if self.in_line_comment {
            self.prev = ch;
            return Step::None;
        }
        if self.in_block_comment {
            if self.prev == '*' && ch == '/' {
                self.in_block_comment = false;
                self.prev = ' '; // consume so `*/` can't re-trigger
                return Step::None;
            }
            self.prev = ch;
            return Step::None;
        }
        if self.in_string {
            if self.escaped {
                self.escaped = false;
            } else if ch == '\\' {
                self.escaped = true;
            } else if ch == '"' {
                self.in_string = false;
            }
            self.prev = ch;
            return Step::None;
        }
        if self.in_char {
            if self.escaped {
                self.escaped = false;
            } else if ch == '\\' {
                self.escaped = true;
            } else if ch == '\'' {
                self.in_char = false;
            }
            self.prev = ch;
            return Step::None;
        }

        // Not in any string/comment context.
        let result = match ch {
            '/' if self.prev == '/' => {
                self.in_line_comment = true;
                Step::None
            }
            '*' if self.prev == '/' => {
                self.in_block_comment = true;
                Step::None
            }
            '"' => {
                self.in_string = true;
                Step::None
            }
            '\'' => {
                self.in_char = true;
                Step::None
            }
            '{' => {
                self.depth += 1;
                self.opened_block = true;
                Step::None
            }
            '}' => {
                self.depth -= 1;
                if self.opened_block && self.depth <= 0 {
                    Step::BlockClosed
                } else {
                    Step::None
                }
            }
            ';' if self.depth <= 0 && !self.opened_block => Step::StatementEnd,
            _ => Step::None,
        };
        self.prev = ch;
        result
    }

    fn end_of_line(&mut self) {
        self.in_line_comment = false;
        // A `/` at line end can't begin a comment on the next line.
        self.prev = '\n';
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rs(src: &str) -> PathBuf {
        let _ = src;
        PathBuf::from("x.rs")
    }

    #[test]
    fn finds_function_with_exact_body() {
        let src = "fn a() {\n    1\n}\n\nfn target() {\n    let x = 1;\n    x\n}\n\nfn b() {}\n";
        match resolve_symbol(&rs(src), src, "target") {
            SymbolSlice::Found { start, end, .. } => {
                assert_eq!(start, 5, "starts at the fn decl");
                assert_eq!(end, 8, "ends at the closing brace");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn brace_in_string_does_not_close_early() {
        let src = "fn target() {\n    let s = \"}\";\n    let t = '}';\n    s\n}\n";
        match resolve_symbol(&rs(src), src, "target") {
            SymbolSlice::Found { start, end, .. } => {
                assert_eq!(start, 1);
                assert_eq!(end, 5, "the string/char `}}` must not end the block");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn brace_in_line_comment_ignored() {
        let src = "fn target() {\n    // closing } here is a comment\n    1\n}\n";
        match resolve_symbol(&rs(src), src, "target") {
            SymbolSlice::Found { end, .. } => assert_eq!(end, 4),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn includes_doc_comments_and_attributes() {
        let src = "/// Doc line.\n#[inline]\nfn target() {\n    1\n}\n";
        match resolve_symbol(&rs(src), src, "target") {
            SymbolSlice::Found { start, end, .. } => {
                assert_eq!(start, 1, "doc + attribute pulled into the window");
                assert_eq!(end, 5);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn struct_with_nested_braces() {
        let src = "struct Target {\n    a: Vec<u8>,\n    b: HashMap<String, Vec<u8>>,\n}\n";
        match resolve_symbol(&rs(src), src, "Target") {
            SymbolSlice::Found {
                start,
                end,
                kind: SymbolKind::Struct,
                ..
            } => {
                assert_eq!((start, end), (1, 4));
            }
            other => panic!("expected struct Found, got {other:?}"),
        }
    }

    #[test]
    fn impl_block_spans_its_methods() {
        // The impl contains methods that are ALSO extracted as symbols; the
        // brace matcher must span the whole impl, not stop at the first method.
        let src = "impl Foo {\n    fn one(&self) {\n        1\n    }\n    fn two(&self) {\n        2\n    }\n}\n";
        match resolve_symbol(&rs(src), src, "Foo") {
            SymbolSlice::Found { start, end, .. } => assert_eq!((start, end), (1, 8)),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn not_found_lists_available() {
        let src = "fn alpha() {}\nstruct Beta {}\n";
        match resolve_symbol(&rs(src), src, "missing") {
            SymbolSlice::NotFound { available } => {
                assert!(available.contains(&"alpha".to_string()));
                assert!(available.contains(&"Beta".to_string()));
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_language() {
        let src = "some plain text\n";
        assert_eq!(
            resolve_symbol(&PathBuf::from("notes.txt"), src, "anything"),
            SymbolSlice::Unsupported
        );
    }

    #[test]
    fn multiple_same_name_resolves_first_and_flags() {
        // Two methods named `run` in different impls — resolve the first, flag it.
        let src = "impl A {\n    fn run(&self) {\n        1\n    }\n}\nimpl B {\n    fn run(&self) {\n        2\n    }\n}\n";
        match resolve_symbol(&rs(src), src, "run") {
            SymbolSlice::Found {
                start, multiple, ..
            } => {
                assert_eq!(start, 2, "first `run` declaration");
                assert!(multiple, "must flag that more than one `run` exists");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
}
