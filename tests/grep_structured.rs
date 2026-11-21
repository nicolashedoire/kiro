//! Search records originate in the search, and BSD behavior remains the source of truth.
#![cfg(target_os = "macos")]

use kiro::grep::{self, SearchKind, Syntax};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "kiro-grep-structured-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn compare(&self, args: &[&str]) -> grep::Output {
        let expected = Command::new("/usr/bin/grep")
            .args(args)
            .current_dir(&self.0)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let actual = run(&self.0, args);
        assert_eq!(actual.stdout, expected.stdout, "stdout: {args:?}");
        assert_eq!(
            actual.stderr.as_bytes(),
            expected.stderr,
            "stderr: {args:?}"
        );
        assert_eq!(
            Some(actual.code),
            expected.status.code(),
            "status: {args:?}"
        );
        actual
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(
            self.0.join("root/sealed"),
            std::fs::Permissions::from_mode(0o700),
        );
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(dir: &Path, args: &[&str]) -> grep::Output {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let options = grep::parse(&args, Syntax::Basic).unwrap();
    grep::run(&options, dir, None).unwrap()
}

#[test]
fn conflicting_options_follow_the_last_bsd_option() {
    let fixture = Fixture::new("option-order");
    fixture.write("hit", b"needle\n");
    fixture.write("miss", b"other\n");
    fixture.write("binary", b"needle\0tail\n");
    for flags in [
        vec!["-lL"],
        vec!["-Ll"],
        vec!["-l", "-L"],
        vec!["-L", "-l"],
        vec!["--files-with-matches", "--files-without-match"],
        vec!["--files-without-match", "--files-with-matches"],
        vec!["-aI"],
        vec!["-Ia"],
        vec!["-I", "--text"],
        vec!["--text", "-I"],
        vec!["-aIa"],
        vec!["-IaI"],
        vec!["-lLl"],
        vec!["-LlL"],
    ] {
        for paths in [
            vec!["hit"],
            vec!["miss"],
            vec!["binary"],
            vec!["hit", "miss", "binary"],
        ] {
            let args: Vec<&str> = flags
                .iter()
                .copied()
                .chain(["needle"])
                .chain(paths)
                .collect();
            fixture.compare(&args);
        }
    }
}

#[test]
fn exclude_dir_applies_to_explicit_roots_and_displayed_paths() {
    let fixture = Fixture::new("excluded-root");
    fixture.write("skip/hit", b"needle\n");
    fixture.write("keep/hit", b"needle\n");
    for pattern in ["skip", "*skip", "./skip", ".", "keep", "keep/hit"] {
        let flag = format!("--exclude-dir={pattern}");
        for root in ["skip", "./skip", "keep", ".", "skip/hit"] {
            fixture.compare(&["-rn", &flag, "needle", root]);
        }
        fixture.compare(&["-rn", &flag, "needle", "skip", "keep"]);
    }
}

#[test]
fn unreadable_directories_are_errors_even_when_their_contents_are_unknown() {
    let fixture = Fixture::new("walk-errors");
    fixture.write("root/sealed/hit", b"needle\n");
    fixture.write("root/visible", b"other\n");
    std::fs::set_permissions(
        fixture.0.join("root/sealed"),
        std::fs::Permissions::from_mode(0o0),
    )
    .unwrap();
    for flags in ["-rn", "-rns", "-rnq"] {
        let actual = fixture.compare(&[flags, "needle", "root"]);
        assert_eq!(actual.code, 2, "requires an unprivileged test user");
        if flags != "-rns" {
            assert!(actual.stderr.contains("root/sealed: Permission denied"));
        } else {
            assert!(actual.stderr.is_empty());
        }
    }
    // Quiet mode succeeds on a visible hit even if a traversal error was encountered.
    fixture.write("root/visible", b"needle\n");
    let args: Vec<String> = ["-rnq", "needle", "root"].map(str::to_string).into();
    match grep::run(
        &grep::parse(&args, Syntax::Basic).unwrap(),
        &fixture.0,
        None,
    ) {
        Ok(output) => {
            assert_eq!(output.code, 0);
            assert!(output.stderr.contains("root/sealed: Permission denied"));
        }
        Err(reason) => assert!(reason.contains("quiet recursive search with traversal errors")),
    }
}

#[test]
fn records_preserve_real_paths_kinds_offsets_and_context_gaps() {
    let fixture = Fixture::new("records");
    fixture.write(
        "odd:12-name.txt",
        b"before\nneedle one\nafter\nspare\nspare\nneedle two\n",
    );
    let output = fixture.compare(&["-Hn", "-A1", "needle", "odd:12-name.txt"]);
    let listing = output.search.unwrap();
    assert!(!listing.truncated);
    assert_eq!(listing.records.len(), 3);
    let expected = [
        (2, SearchKind::Match, b"needle one".as_slice()),
        (3, SearchKind::Context, b"after".as_slice()),
        (6, SearchKind::Match, b"needle two".as_slice()),
    ];
    let mut previous = 0;
    for (record, (number, kind, text)) in listing.records.iter().zip(expected) {
        assert_eq!(record.path.as_deref(), Some("odd:12-name.txt"));
        assert_eq!(record.line_number, number);
        assert_eq!(record.kind, kind);
        assert!(record.output_start >= previous);
        assert!(record.output_start <= record.text_start);
        assert_eq!(&output.stdout[record.text_start..record.text_end], text);
        assert_eq!(&output.stdout[record.text_end..record.output_end], b"\n");
        previous = record.output_end;
    }
    let gap = &output.stdout[listing.records[1].output_end..listing.records[2].output_start];
    assert_eq!(gap, b"--\n");
}

#[test]
fn records_exist_without_printed_prefixes_and_for_each_only_match() {
    let fixture = Fixture::new("plain-records");
    fixture.write("source", b"needle needle\n");
    let output = fixture.compare(&["needle", "source"]);
    let record = &output.search.as_ref().unwrap().records[0];
    assert_eq!(record.path.as_deref(), Some("source"));
    assert_eq!(record.line_number, 1);
    assert_eq!(record.output_start, record.text_start);
    for flags in ["-o", "-on"] {
        let output = fixture.compare(&[flags, "needle", "source"]);
        let listing = output.search.unwrap();
        assert_eq!(listing.records.len(), 2);
        for record in listing.records {
            assert_eq!(record.line_number, 1);
            assert_eq!(
                &output.stdout[record.text_start..record.text_end],
                b"needle"
            );
        }
    }
    for flags in ["-c", "-l", "-L", "-q"] {
        assert!(fixture
            .compare(&[flags, "needle", "source"])
            .search
            .is_none());
    }
    fixture.write("binary", b"needle\0tail\n");
    assert!(fixture
        .compare(&["needle", "source", "binary"])
        .search
        .is_none());
}

#[test]
fn metadata_is_bounded_without_truncating_stdout() {
    let fixture = Fixture::new("metadata-bound");
    fixture.write("source", b"x\n".repeat(10_000));
    let output = fixture.compare(&["-Hn", "x", "source"]);
    assert_eq!(
        output.stdout.iter().filter(|b| **b == b'\n').count(),
        10_000
    );
    let listing = output.search.unwrap();
    assert!(listing.truncated);
    assert!(!listing.records.is_empty());
    assert!(listing.records.len() <= 4096);
    assert!(listing.records.last().unwrap().output_end < output.stdout.len());
    let allocated = listing.records.capacity() * std::mem::size_of::<grep::SearchRecord>()
        + listing
            .records
            .iter()
            .map(|record| record.path.as_ref().map_or(0, String::capacity))
            .sum::<usize>();
    assert!(allocated <= 256 * 1024, "allocated metadata: {allocated}");

    // Path allocations also count, independently of the Vec's spare capacity.
    let long_name = "a".repeat(200);
    fixture.write(&long_name, b"x\n".repeat(10_000));
    let output = fixture.compare(&["-Hn", "x", &long_name]);
    let listing = output.search.unwrap();
    assert!(listing.truncated);
    assert!(listing.records.len() < 4096);
    let allocated = listing.records.capacity() * std::mem::size_of::<grep::SearchRecord>()
        + listing
            .records
            .iter()
            .map(|record| record.path.as_ref().map_or(0, String::capacity))
            .sum::<usize>();
    assert!(
        allocated <= 256 * 1024,
        "allocated metadata with long paths: {allocated}"
    );
}
