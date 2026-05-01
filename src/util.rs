use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Resolve the project root.
/// Priority: --project-root flag, else cwd.
pub fn project_root(flag: Option<&Path>) -> PathBuf {
    if let Some(p) = flag {
        return p.to_path_buf();
    }
    env::current_dir().expect("cwd unavailable")
}

/// Print an error to stderr and exit nonzero.
pub fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("error: {}", msg);
    std::process::exit(1);
}

/// Quote a string as a TOML basic string.
pub fn toml_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

/// Lowercase, alphanumerics kept, runs of non-alphanumerics collapsed to a
/// single dash, leading/trailing dashes stripped.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    out
}

/// Read a single line of input from stdin after printing a prompt.
pub fn prompt(label: &str) -> String {
    print!("{}", label);
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line).expect("stdin read failed");
    line.trim_end_matches(&['\n', '\r'][..]).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Entry Point"), "entry-point");
    }

    #[test]
    fn slugify_strips_leading_and_trailing_punct() {
        assert_eq!(slugify("  hello, world!  "), "hello-world");
    }

    #[test]
    fn slugify_collapses_runs_of_separators() {
        assert_eq!(slugify("a   b---c"), "a-b-c");
    }

    #[test]
    fn slugify_returns_empty_for_no_alnum() {
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("!!!"), "");
        assert_eq!(slugify("   "), "");
    }

    #[test]
    fn slugify_preserves_digits() {
        assert_eq!(slugify("Step 1"), "step-1");
    }

    #[test]
    fn slugify_lowercases() {
        assert_eq!(slugify("ABC"), "abc");
    }

    #[test]
    fn toml_quote_simple() {
        assert_eq!(toml_quote("hello"), "\"hello\"");
    }

    #[test]
    fn toml_quote_escapes_backslash_and_double_quote() {
        assert_eq!(toml_quote(r#"a"b\c"#), r#""a\"b\\c""#);
    }

    #[test]
    fn project_root_uses_explicit_flag() {
        let p = Path::new("/some/path");
        assert_eq!(project_root(Some(p)), p.to_path_buf());
    }
}
