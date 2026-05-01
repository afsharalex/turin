//! End-to-end tests that spawn the actual `turin` binary and assert
//! against its filesystem effects, stdout, and exit codes.

use std::path::Path;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_turin");

fn turin(root: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.arg("--project-root").arg(root);
    c
}

/// `turin new --tour-title "Test tour"` — the minimal valid `new` invocation.
/// Tests that don't care about tour-level metadata use this; tests that do
/// build their own command via `turin(root)`.
fn turin_new(root: &Path) -> Command {
    let mut c = turin(root);
    c.args(["new", "--tour-title", "Test tour"]);
    c
}

fn assert_ok(out: &Output, ctx: &str) {
    assert!(
        out.status.success(),
        "{}: exited {:?}\nstdout:\n{}\nstderr:\n{}",
        ctx,
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn assert_err(out: &Output, ctx: &str) {
    assert!(
        !out.status.success(),
        "{}: expected nonzero exit but got success\nstdout:\n{}",
        ctx,
        String::from_utf8_lossy(&out.stdout),
    );
}

#[test]
fn new_creates_turin_dir_and_skeleton_tour_json() {
    let dir = tempfile::tempdir().unwrap();
    let out = turin_new(dir.path()).output().unwrap();
    assert_ok(&out, "turin new");

    let tour_path = dir.path().join(".turin/tour.json");
    assert!(tour_path.exists());
    let json = std::fs::read_to_string(&tour_path).unwrap();
    assert!(json.contains("\"title\": \"Test tour\""));
    assert!(json.contains("\"stops\": []"));
}

#[test]
fn new_requires_tour_title() {
    let dir = tempfile::tempdir().unwrap();
    let out = turin(dir.path()).arg("new").output().unwrap();
    assert_err(&out, "turin new without --tour-title");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--tour-title"),
        "expected stderr to mention --tour-title, got: {}",
        stderr
    );
}

#[test]
fn new_writes_all_optional_tour_metadata_fields() {
    let dir = tempfile::tempdir().unwrap();
    let out = turin(dir.path())
        .args([
            "new",
            "--tour-title",
            "My tour",
            "--tour-description",
            "Walks through the parser",
            "--tour-author",
            "alex",
            "--tour-created",
            "2026-04-29",
        ])
        .output()
        .unwrap();
    assert_ok(&out, "turin new with full metadata");

    let json = std::fs::read_to_string(dir.path().join(".turin/tour.json")).unwrap();
    assert!(json.contains("\"title\": \"My tour\""));
    assert!(json.contains("\"description\": \"Walks through the parser\""));
    assert!(json.contains("\"author\": \"alex\""));
    assert!(json.contains("\"created\": \"2026-04-29\""));
}

#[test]
fn new_omits_optional_metadata_when_unset() {
    let dir = tempfile::tempdir().unwrap();
    assert_ok(&turin_new(dir.path()).output().unwrap(), "minimal new");
    let json = std::fs::read_to_string(dir.path().join(".turin/tour.json")).unwrap();
    assert!(!json.contains("description"));
    assert!(!json.contains("author"));
    assert!(!json.contains("created"));
}

#[test]
fn new_with_full_stop_flags_seeds_initial_stop() {
    let dir = tempfile::tempdir().unwrap();
    let out = turin_new(dir.path())
        .args([
            "--title",
            "Entry point",
            "--file",
            "src/lib.rs",
            "--anchor-kind",
            "pattern",
            "--anchor",
            "fn main",
            "--body",
            "starting here",
        ])
        .output()
        .unwrap();
    assert_ok(&out, "turin new with seed");

    let tour: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(".turin/tour.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(tour["stops"], serde_json::json!(["entry-point.md"]));

    let stop = std::fs::read_to_string(dir.path().join(".turin/entry-point.md")).unwrap();
    assert!(stop.contains("file = \"src/lib.rs\""));
    assert!(stop.contains("anchor = { kind = \"pattern\", value = \"fn main\" }"));
    assert!(stop.contains("starting here"));
}

#[test]
fn new_refuses_to_clobber_existing_tour() {
    let dir = tempfile::tempdir().unwrap();
    assert_ok(&turin_new(dir.path()).output().unwrap(), "first new");
    let second = turin_new(dir.path()).output().unwrap();
    assert_err(&second, "second new");
    assert!(String::from_utf8_lossy(&second.stderr).contains("already exists"));
}

#[test]
fn new_with_partial_stop_flags_errors_with_field_list() {
    let dir = tempfile::tempdir().unwrap();
    let out = turin_new(dir.path())
        .args(["--title", "Only a title"])
        .output()
        .unwrap();
    assert_err(&out, "partial new");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--file"));
    assert!(stderr.contains("--anchor-kind"));
    assert!(stderr.contains("--anchor"));

    // No partial state should be left behind.
    assert!(!dir.path().join(".turin/tour.json").exists());
}

#[test]
fn add_appends_stop_and_updates_index() {
    let dir = tempfile::tempdir().unwrap();
    assert_ok(&turin_new(dir.path()).output().unwrap(), "new");

    assert_ok(
        &turin(dir.path())
            .args([
                "add",
                "--title",
                "First",
                "--file",
                "src/a.rs",
                "--anchor-kind",
                "pattern",
                "--anchor",
                "fn a",
            ])
            .output()
            .unwrap(),
        "first add",
    );
    assert_ok(
        &turin(dir.path())
            .args([
                "add",
                "--title",
                "Second",
                "--file",
                "src/b.rs",
                "--anchor-kind",
                "pattern",
                "--anchor",
                "fn b",
            ])
            .output()
            .unwrap(),
        "second add",
    );

    let tour: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(".turin/tour.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(tour["stops"], serde_json::json!(["first.md", "second.md"]));
}

/// Helper for inserting a simple stop with a unique title and matching anchor.
fn add_simple(root: &Path, title: &str, extra: &[&str]) -> Output {
    let mut args = vec![
        "add",
        "--title",
        title,
        "--file",
        "src/x.rs",
        "--anchor-kind",
        "pattern",
        "--anchor",
        title,
    ];
    args.extend_from_slice(extra);
    turin(root).args(&args).output().unwrap()
}

fn read_stops(root: &Path) -> serde_json::Value {
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(".turin/tour.json")).unwrap())
            .unwrap();
    json["stops"].clone()
}

#[test]
fn add_with_position_inserts_at_index() {
    let dir = tempfile::tempdir().unwrap();
    assert_ok(&turin_new(dir.path()).output().unwrap(), "new");
    for name in ["a", "b", "c"] {
        assert_ok(&add_simple(dir.path(), name, &[]), &format!("add {}", name));
    }
    assert_ok(
        &add_simple(dir.path(), "d", &["--position", "3"]),
        "insert d at 3",
    );
    assert_eq!(
        read_stops(dir.path()),
        serde_json::json!(["a.md", "b.md", "d.md", "c.md"])
    );
}

#[test]
fn add_with_position_one_inserts_at_start() {
    let dir = tempfile::tempdir().unwrap();
    assert_ok(&turin_new(dir.path()).output().unwrap(), "new");
    assert_ok(&add_simple(dir.path(), "first", &[]), "add first");
    assert_ok(
        &add_simple(dir.path(), "zero", &["--position", "1"]),
        "insert zero at 1",
    );
    assert_eq!(
        read_stops(dir.path()),
        serde_json::json!(["zero.md", "first.md"])
    );
}

#[test]
fn add_with_position_at_end_plus_one_appends() {
    let dir = tempfile::tempdir().unwrap();
    assert_ok(&turin_new(dir.path()).output().unwrap(), "new");
    assert_ok(&add_simple(dir.path(), "a", &[]), "add a");
    assert_ok(
        &add_simple(dir.path(), "b", &["--position", "2"]),
        "append via position",
    );
    assert_eq!(read_stops(dir.path()), serde_json::json!(["a.md", "b.md"]));
}

#[test]
fn add_with_position_zero_errors() {
    let dir = tempfile::tempdir().unwrap();
    assert_ok(&turin_new(dir.path()).output().unwrap(), "new");
    let out = add_simple(dir.path(), "x", &["--position", "0"]);
    assert_err(&out, "position 0");
    assert!(String::from_utf8_lossy(&out.stderr).contains("--position"));
    // No stop file should have been written.
    assert!(!dir.path().join(".turin/x.md").exists());
}

#[test]
fn add_with_position_beyond_end_plus_one_errors() {
    let dir = tempfile::tempdir().unwrap();
    assert_ok(&turin_new(dir.path()).output().unwrap(), "new");
    // Empty tour: max valid is 1.
    let out = add_simple(dir.path(), "x", &["--position", "5"]);
    assert_err(&out, "position too large");
    assert!(!dir.path().join(".turin/x.md").exists());
}

#[test]
fn add_without_existing_tour_errors() {
    let dir = tempfile::tempdir().unwrap();
    let out = turin(dir.path())
        .args([
            "add",
            "--file",
            "x",
            "--anchor-kind",
            "pattern",
            "--anchor",
            "y",
        ])
        .output()
        .unwrap();
    assert_err(&out, "add without new");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no tour"));
}

#[test]
fn duplicate_slug_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    assert_ok(&turin_new(dir.path()).output().unwrap(), "new");
    let args = [
        "add",
        "--title",
        "Foo",
        "--file",
        "src/x.rs",
        "--anchor-kind",
        "pattern",
        "--anchor",
        "x",
    ];
    assert_ok(&turin(dir.path()).args(args).output().unwrap(), "first add");
    let dup = turin(dir.path()).args(args).output().unwrap();
    assert_err(&dup, "duplicate add");
    assert!(String::from_utf8_lossy(&dup.stderr).contains("already exists"));
}

#[test]
fn line_anchor_writes_unquoted_integer_in_frontmatter() {
    let dir = tempfile::tempdir().unwrap();
    assert_ok(&turin_new(dir.path()).output().unwrap(), "new");
    assert_ok(
        &turin(dir.path())
            .args([
                "add",
                "--file",
                "Cargo.toml",
                "--anchor-kind",
                "line",
                "--anchor",
                "1",
                "--slug",
                "top",
            ])
            .output()
            .unwrap(),
        "add line",
    );
    let stop = std::fs::read_to_string(dir.path().join(".turin/top.md")).unwrap();
    assert!(stop.contains("anchor = { kind = \"line\", value = 1 }"));
}

#[test]
fn list_renders_table_with_anchor_kinds() {
    let dir = tempfile::tempdir().unwrap();
    assert_ok(&turin_new(dir.path()).output().unwrap(), "new");
    assert_ok(
        &turin(dir.path())
            .args([
                "add",
                "--title",
                "Pattern stop",
                "--file",
                "src/a.rs",
                "--anchor-kind",
                "pattern",
                "--anchor",
                "fn a",
            ])
            .output()
            .unwrap(),
        "add p",
    );
    assert_ok(
        &turin(dir.path())
            .args([
                "add",
                "--title",
                "Line stop",
                "--file",
                "src/b.rs",
                "--anchor-kind",
                "line",
                "--anchor",
                "5",
            ])
            .output()
            .unwrap(),
        "add l",
    );

    let out = turin(dir.path()).arg("list").output().unwrap();
    assert_ok(&out, "list");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Pattern stop"));
    assert!(stdout.contains("Line stop"));
    assert!(stdout.contains("pattern /fn a/"));
    assert!(stdout.contains("line 5"));
}

#[test]
fn list_against_empty_tour_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    assert_ok(&turin_new(dir.path()).output().unwrap(), "new");
    let out = turin(dir.path()).arg("list").output().unwrap();
    assert_ok(&out, "list empty");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("0 stops"));
}

#[test]
fn quickstart_prints_format_reference() {
    let out = Command::new(BIN).arg("quickstart").output().unwrap();
    assert_ok(&out, "quickstart");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Turin quickstart"));
    assert!(stdout.contains("tour.json"));
    assert!(stdout.contains("Frontmatter fields"));
}

#[test]
fn body_file_flag_reads_body_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let body_path = dir.path().join("notes.md");
    std::fs::write(&body_path, "external body content\nover multiple lines\n").unwrap();

    assert_ok(&turin_new(dir.path()).output().unwrap(), "new");
    assert_ok(
        &turin(dir.path())
            .args([
                "add",
                "--title",
                "External",
                "--file",
                "src/x.rs",
                "--anchor-kind",
                "pattern",
                "--anchor",
                "x",
                "--body-file",
            ])
            .arg(&body_path)
            .output()
            .unwrap(),
        "add with body-file",
    );

    let stop = std::fs::read_to_string(dir.path().join(".turin/external.md")).unwrap();
    assert!(stop.contains("external body content"));
    assert!(stop.contains("over multiple lines"));
}
