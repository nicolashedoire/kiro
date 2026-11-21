use kiro::scout::{scout, scout_report, scout_result, Request, ScoutStatus};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "kiro-scout-regression-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ask(dir: &Path, budget: usize) -> String {
    scout(
        dir,
        &Request {
            terms: vec!["needle".to_string()],
            paths: Vec::new(),
            budget,
        },
    )
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("/usr/bin/git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
}

#[test]
fn git_discovery_does_not_run_a_configured_fsmonitor() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("fsmonitor");
    let dir = &fixture.0;
    git(dir, &["init", "--quiet"]);
    std::fs::write(dir.join("tracked.rs"), "pub const needle: u8 = 1;\n").unwrap();
    git(dir, &["add", "tracked.rs"]);
    let hook = dir.join(".git/monitor.sh");
    let marker = dir.join(".git/monitor.sh.marker");
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf ran > \"$0.marker\"\nprintf 'token\\0'\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    git(dir, &["config", "core.fsmonitor", hook.to_str().unwrap()]);

    // Establish that the fixture would execute its hook without scout's overrides.
    git(
        dir,
        &["ls-files", "--cached", "--others", "--exclude-standard"],
    );
    assert!(marker.exists(), "control Git command did not run the hook");
    std::fs::remove_file(&marker).unwrap();

    let answer = ask(dir, 4000);
    assert!(answer.contains("tracked.rs"), "{answer}");
    assert!(!marker.exists(), "scout ran the fsmonitor hook");
}

#[test]
fn an_empty_git_listing_does_not_search_ignored_files() {
    let fixture = Fixture::new("ignored");
    git(&fixture.0, &["init", "--quiet"]);
    std::fs::write(fixture.0.join(".gitignore"), "*\n").unwrap();
    std::fs::write(fixture.0.join("ignored.txt"), "needle\n").unwrap();
    let answer = ask(&fixture.0, 4000);
    assert!(answer.starts_with("scout: nothing found"), "{answer}");
    assert!(!answer.contains("ignored.txt"), "{answer}");
}

#[test]
fn the_answer_budget_is_strict_for_unicode_and_extreme_values() {
    let fixture = Fixture::new("budget");
    std::fs::write(
        fixture.0.join("unicode.rs"),
        format!("pub fn needle() {{\n  // {}\n}}\n", "é💡".repeat(100)),
    )
    .unwrap();
    for budget in [0, 1, 10, 42, 128, 512, usize::MAX] {
        let answer = ask(&fixture.0, budget);
        assert!(
            answer.len() <= budget.min(64 * 1024),
            "{budget}: {}",
            answer.len()
        );
        if (42..512).contains(&budget) {
            assert!(answer.contains("truncated"), "{answer}");
        }
    }
}

#[test]
fn large_scans_are_reported_as_incomplete() {
    let fixture = Fixture::new("scan-limit");
    let mut contents = vec![b' '; 400_000];
    contents[..7].copy_from_slice(b"needle\n");
    for index in 0..50 {
        std::fs::write(fixture.0.join(format!("file-{index:02}.rs")), &contents).unwrap();
    }
    let answer = ask(&fixture.0, 4000);
    assert!(answer.starts_with("[search incomplete:"), "{answer}");
    assert!(answer.len() <= 4000);
    assert_eq!(
        scout_result(
            &fixture.0,
            &Request {
                terms: vec!["needle".to_string()],
                paths: Vec::new(),
                budget: 0,
            }
        ),
        Err(String::new()),
        "a resource-limited scan must retain its failure status even without display text"
    );
}

#[test]
fn oversized_queries_are_refused_instead_of_silently_dropping_terms() {
    let fixture = Fixture::new("query-limit");
    for terms in [vec!["needle".to_string(); 61], vec!["x".repeat(513)]] {
        let answer = scout(
            &fixture.0,
            &Request {
                terms,
                paths: Vec::new(),
                budget: 4000,
            },
        );
        assert!(
            answer.starts_with("scout: too many or oversized terms"),
            "{answer}"
        );
    }
}

#[test]
fn unreadable_text_is_an_incomplete_scan_not_a_successful_negative() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("unreadable");
    let secret = fixture.0.join("secret.rs");
    std::fs::write(&secret, "pub const needle: u8 = 1;\n").unwrap();
    std::fs::write(
        fixture.0.join("readable.rs"),
        "pub const unrelated: u8 = 2;\n",
    )
    .unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&secret).is_ok() {
        // Root and some privileged CI environments bypass these permission bits. Missing
        // tracked files and broken links below still exercise an unconditional I/O failure.
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        eprintln!("permission test skipped: this account can read mode-000 files");
        return;
    }
    let answer = ask(&fixture.0, 4000);
    let status = scout_result(
        &fixture.0,
        &Request {
            terms: vec!["needle".to_string()],
            paths: Vec::new(),
            budget: 0,
        },
    );
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        status,
        Err(String::new()),
        "a zero budget must not hide the failure status"
    );
    assert!(answer.starts_with("[search incomplete:"), "{answer}");
    assert!(answer.contains("1 files could not be read"), "{answer}");
    assert!(answer.contains("secret.rs"), "{answer}");
    assert!(answer.contains("nothing found"), "{answer}");
    assert!(
        answer.contains("1 text files read (0 skipped; 1 read errors)"),
        "{answer}"
    );
}

#[test]
fn missing_tracked_files_and_broken_links_are_reported_separately_from_matches() {
    let fixture = Fixture::new("missing-tracked");
    git(&fixture.0, &["init", "--quiet"]);
    std::fs::write(fixture.0.join("readable.rs"), "pub const needle: u8 = 1;\n").unwrap();
    std::fs::write(fixture.0.join("removed.rs"), "needle\n").unwrap();
    std::os::unix::fs::symlink("no-such-target", fixture.0.join("broken.rs")).unwrap();
    git(
        &fixture.0,
        &["add", "readable.rs", "removed.rs", "broken.rs"],
    );
    std::fs::remove_file(fixture.0.join("removed.rs")).unwrap();
    let answer = ask(&fixture.0, 4000);
    assert!(answer.starts_with("[search incomplete:"), "{answer}");
    assert!(answer.contains("2 files could not be read"), "{answer}");
    assert!(
        answer.contains("removed.rs") && answer.contains("broken.rs"),
        "{answer}"
    );
    assert!(
        answer.contains("1 text files read; 0 skipped; 2 read errors"),
        "{answer}"
    );
    assert!(answer.contains("## readable.rs"), "{answer}");
    assert_eq!(
        scout_result(
            &fixture.0,
            &Request {
                terms: vec!["needle".to_string()],
                paths: Vec::new(),
                budget: 0,
            }
        ),
        Err(String::new())
    );
}

#[test]
fn complete_absence_and_invalid_input_have_distinct_statuses_at_any_budget() {
    let fixture = Fixture::new("result-status");
    std::fs::write(
        fixture.0.join("readable.rs"),
        "pub const unrelated: u8 = 1;\n",
    )
    .unwrap();
    for budget in [0, 4000] {
        let complete = scout_result(
            &fixture.0,
            &Request {
                terms: vec!["needle".to_string()],
                paths: Vec::new(),
                budget,
            },
        )
        .expect("a complete negative result is successful");
        assert!(complete.len() <= budget);
        if budget > 0 {
            assert!(complete.starts_with("scout: nothing found"), "{complete}");
        }
        assert!(scout_result(
            &fixture.0,
            &Request {
                terms: Vec::new(),
                paths: Vec::new(),
                budget,
            }
        )
        .is_err());
        assert!(scout_result(
            &fixture.0,
            &Request {
                terms: vec!["needle".to_string()],
                paths: vec!["missing-root".to_string()],
                budget,
            }
        )
        .is_err());
    }
}

#[test]
fn documented_exclusions_are_counted_without_becoming_io_errors() {
    let fixture = Fixture::new("skipped");
    std::fs::write(
        fixture.0.join("readable.rs"),
        "pub const unrelated: u8 = 2;\n",
    )
    .unwrap();
    std::fs::write(fixture.0.join("binary.dat"), b"\0needle\n").unwrap();
    std::fs::write(fixture.0.join("generated.min.js"), "needle\n").unwrap();
    std::fs::File::create(fixture.0.join("large.rs"))
        .unwrap()
        .set_len(400_001)
        .unwrap();
    let answer = ask(&fixture.0, 4000);
    assert!(answer.starts_with("scout: nothing found"), "{answer}");
    assert!(
        answer.contains("1 text files read (3 skipped; 0 read errors)"),
        "{answer}"
    );
    assert!(!answer.contains("incomplete"), "{answer}");
}

#[test]
fn a_nonexistent_explicit_root_reports_discovery_failure() {
    let fixture = Fixture::new("missing-root");
    let answer = scout(
        &fixture.0,
        &Request {
            terms: vec!["needle".to_string()],
            paths: vec!["does-not-exist".to_string()],
            budget: 4000,
        },
    );
    assert!(answer.starts_with("[search incomplete:"), "{answer}");
    assert!(!answer.contains("nothing found"), "{answer}");
}

#[test]
fn long_line_previews_show_a_late_match_and_explicit_omissions() {
    let fixture = Fixture::new("long-lines");
    for (index, prefix) in ["x".repeat(600), "漢é💡".repeat(120)].iter().enumerate() {
        std::fs::write(
            fixture.0.join(format!("line-{index}.rs")),
            format!("{prefix}NeEdLe{}\n", "界💡".repeat(100)),
        )
        .unwrap();
    }
    let answer = ask(&fixture.0, 6000);
    let matching: Vec<_> = answer
        .lines()
        .filter(|line| line.starts_with("    1:"))
        .collect();
    assert_eq!(matching.len(), 2, "{answer}");
    for line in matching {
        assert!(line.contains("NeEdLe"), "{line}");
        assert!(
            line.contains('…') && line.contains("[line clipped; read full line]"),
            "{line}"
        );
        assert!(!line.contains('\u{fffd}'), "{line}");
    }
    assert!(answer.len() <= 6000);
}

#[test]
fn separated_matches_on_a_long_line_do_not_claim_the_whole_line_is_shown() {
    let fixture = Fixture::new("separated-matches");
    std::fs::write(
        fixture.0.join("line.rs"),
        format!("needle{}other-term\n", "x".repeat(600)),
    )
    .unwrap();
    let answer = scout(
        &fixture.0,
        &Request {
            terms: vec!["needle".to_string(), "other-term".to_string()],
            paths: Vec::new(),
            budget: 4000,
        },
    );
    let line = answer
        .lines()
        .find(|line| line.starts_with("    1:"))
        .unwrap();
    assert!(line.contains("needle"), "{answer}");
    assert!(line.contains("[line clipped; read full line]"), "{answer}");
    assert!(
        answer.contains("\"other-term\": 1 lines in 1 files"),
        "{answer}"
    );
}

#[test]
fn structured_references_are_direct_and_survive_a_zero_display_budget() {
    let fixture = Fixture::new("structured-zero");
    std::fs::write(
        fixture.0.join("strange:17-name.rs"),
        "unrelated\nNeEdLe first\nneedle other\n",
    )
    .unwrap();
    let report = scout_report(
        &fixture.0,
        &Request {
            terms: vec!["  needle  ".into(), "other".into()],
            paths: Vec::new(),
            budget: 0,
        },
    );
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(report.status.as_str(), "complete");
    assert!(report.reason.is_none());
    assert_eq!(report.text, "");
    assert!(report.display_truncated);
    assert!(!report.metadata_truncated);
    assert_eq!(report.terms, ["needle", "other"]);
    assert_eq!(report.counts.candidates, 1);
    assert_eq!(report.counts.read, 1);
    assert_eq!(report.counts.errors, 0);
    assert_eq!(report.counts.matching_files, 1);
    assert_eq!(report.counts.matching_lines, 2);
    assert_eq!(report.matches.len(), 2);
    assert_eq!(report.matches[0].path, "strange:17-name.rs");
    assert_eq!(report.matches[0].line_number, 2);
    assert_eq!(report.matches[0].term_indices, [0]);
    assert_eq!(report.matches[1].line_number, 3);
    assert_eq!(report.matches[1].term_indices, [0, 1]);
}

#[test]
fn structured_absence_invalid_input_and_discovery_failure_remain_distinct() {
    let fixture = Fixture::new("structured-status");
    std::fs::write(fixture.0.join("plain.rs"), "unrelated\n").unwrap();
    for budget in [0, 4000] {
        let mut request = Request {
            terms: vec!["needle".into()],
            paths: Vec::new(),
            budget,
        };
        let report = scout_report(&fixture.0, &request);
        assert_eq!(report.status, ScoutStatus::Complete);
        assert_eq!(report.counts.candidates, 1);
        assert_eq!(report.counts.read, 1);
        assert_eq!(report.counts.matching_files, 0);
        assert_eq!(report.counts.matching_lines, 0);
        assert!(report.matches.is_empty());
        assert!(!report.metadata_truncated);
        assert_eq!(report.display_truncated, budget == 0);
        request.terms.clear();
        let report = scout_report(&fixture.0, &request);
        assert_eq!(report.status, ScoutStatus::Error);
        assert!(report.reason.unwrap().contains("at least one term"));
        assert_eq!(report.counts.candidates, 0);
        request.terms.push("needle".into());
        request.paths.push("nonexistent".into());
        let report = scout_report(&fixture.0, &request);
        assert_eq!(report.status, ScoutStatus::Error);
        assert!(report.reason.unwrap().contains("discovery failed"));
        assert_eq!(report.counts.read, 0);
    }
}

#[test]
fn structured_read_failures_keep_partial_counts_and_references_at_zero_budget() {
    let fixture = Fixture::new("structured-partial");
    git(&fixture.0, &["init", "--quiet"]);
    std::fs::write(fixture.0.join("present.rs"), "needle\n").unwrap();
    std::fs::write(fixture.0.join("removed.rs"), "needle\n").unwrap();
    git(&fixture.0, &["add", "present.rs", "removed.rs"]);
    std::fs::remove_file(fixture.0.join("removed.rs")).unwrap();
    let report = scout_report(
        &fixture.0,
        &Request {
            terms: vec!["needle".into()],
            paths: Vec::new(),
            budget: 0,
        },
    );
    assert_eq!(report.status, ScoutStatus::Partial);
    assert_eq!(report.status.as_str(), "partial");
    assert!(report.reason.unwrap().contains("1 files could not be read"));
    assert_eq!(report.text, "");
    assert_eq!(report.counts.candidates, 2);
    assert_eq!(report.counts.read, 1);
    assert_eq!(report.counts.errors, 1);
    assert_eq!(report.counts.matching_files, 1);
    assert_eq!(report.counts.matching_lines, 1);
    assert_eq!(report.matches[0].path, "present.rs");
}

#[test]
fn structured_reference_limits_do_not_stop_scanning_or_hide_total_matches() {
    let fixture = Fixture::new("structured-metadata-limit");
    // More records than either metadata cap can retain, still below the file/input caps.
    std::fs::write(fixture.0.join("many.rs"), "needle\n".repeat(6000)).unwrap();
    let report = scout_report(
        &fixture.0,
        &Request {
            terms: vec!["needle".into()],
            paths: Vec::new(),
            budget: 0,
        },
    );
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(report.counts.read, 1);
    assert_eq!(report.counts.matching_lines, 6000);
    assert!(report.metadata_truncated);
    assert!(report.matches.len() <= 4096);
    assert!(!report.matches.is_empty());
    let allocated = report.matches.capacity() * std::mem::size_of_val(&report.matches[0])
        + report
            .matches
            .iter()
            .map(|item| {
                item.path.capacity() + item.term_indices.capacity() * std::mem::size_of::<usize>()
            })
            .sum::<usize>();
    assert!(
        allocated <= 256 * 1024,
        "metadata allocated {allocated} bytes"
    );
    for (index, item) in report.matches.iter().enumerate() {
        assert_eq!(item.line_number, index + 1);
        assert_eq!(item.term_indices, [0]);
    }
}

#[test]
fn structured_resource_limit_remains_partial_without_display_text() {
    let fixture = Fixture::new("structured-input-limit");
    let mut contents = vec![b' '; 400_000];
    contents[..7].copy_from_slice(b"needle\n");
    for index in 0..50 {
        std::fs::write(fixture.0.join(format!("file-{index:02}.rs")), &contents).unwrap();
    }
    let report = scout_report(
        &fixture.0,
        &Request {
            terms: vec!["needle".into()],
            paths: Vec::new(),
            budget: 0,
        },
    );
    assert_eq!(report.status, ScoutStatus::Partial);
    assert!(report.reason.unwrap().contains("resource limit reached"));
    assert_eq!(report.counts.candidates, 50);
    assert!(report.counts.read > 0 && report.counts.read < 50);
    assert_eq!(report.counts.matching_files, report.counts.read);
    assert_eq!(report.counts.errors, 0);
    assert_eq!(report.text, "");
    assert!(report.display_truncated);
}
