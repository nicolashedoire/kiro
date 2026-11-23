use kiro::diagnose::{diagnose_report, output_schema, Report, Request};
use kiro::scout::ScoutStatus;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "kiro-diagnose-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, name: &str, contents: impl AsRef<[u8]>) {
        let path = self.0.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn ask(&self, report: &str, terms: &[&str], budget: usize) -> Report {
        diagnose_report(
            &self.0,
            &Request {
                report: report.to_string(),
                terms: terms.iter().map(|term| term.to_string()).collect(),
                paths: Vec::new(),
                budget,
                max_files: 8,
            },
        )
    }

    fn git(&self, arguments: &[&str]) {
        assert!(Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&self.0)
            .args(arguments)
            .status()
            .unwrap()
            .success());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn candidates(report: &Report) -> &[Value] {
    report.structured["candidates"].as_array().unwrap()
}

#[test]
fn report_paths_and_lexical_evidence_are_fused_with_real_guard_context() {
    let fixture = Fixture::new("fusion");
    fixture.write(
        "noise.rs",
        "fn noise() { let _ = \"retryTimeout\"; }\n".repeat(30),
    );
    fixture.write(
        "src/retry.rs",
        "pub fn retryTimeout(attempts: usize) -> Result<(), &'static str> {\n    if attempts > 2 {\n        return Err(\"retryTimeout\");\n    }\n    Ok(())\n}\n",
    );
    let report = fixture.ask("Failure at src/retry.rs:3:9: retryTimeout", &[], 12_000);
    assert_eq!(report.status, ScoutStatus::Complete);
    let first = &candidates(&report)[0];
    assert_eq!(first["path"], "src/retry.rs");
    assert_eq!(first["signals"]["trace_rank"], 1);
    assert!(first["signals"]["lexical_rank"].is_number());
    let evidence = first["evidence"][0]["text"].as_str().unwrap();
    assert!(evidence.contains("if attempts > 2"));
    assert!(evidence.contains("return Err"));
    assert_eq!(first["evidence"][0]["syntax_status"], "parsed");
    assert!(report.text.contains("ranking is not confidence"));
}

#[test]
fn zero_and_small_unicode_budgets_preserve_structured_evidence() {
    let fixture = Fixture::new("unicode");
    fixture.write(
        "src/café.rs",
        "pub fn retryTimeout() { let label = \"é💡\"; }\n",
    );
    for budget in [0, 1, 2, 3, 7, 80, 150, 300] {
        let report = fixture.ask("retryTimeout é💡", &["retryTimeout"], budget);
        assert_eq!(report.status, ScoutStatus::Complete);
        assert!(report.text.len() <= budget);
        assert!(std::str::from_utf8(report.text.as_bytes()).is_ok());
        assert!(!candidates(&report).is_empty());
        assert_eq!(report.structured["display_truncated"], true);
        assert!(
            report.structured["counts"]["matching_lines"]
                .as_u64()
                .unwrap()
                > 0
        );
    }
}

#[test]
fn display_budget_and_top_k_do_not_change_the_scan_status() {
    let fixture = Fixture::new("selection");
    for index in 0..12 {
        fixture.write(
            &format!("src/file{index:02}.js"),
            "export function retryTimeout() {}\n",
        );
    }
    let report = fixture.ask("retryTimeout", &[], 0);
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(candidates(&report).len(), 8);
    assert_eq!(report.structured["selection_truncated"], true);
    assert_eq!(report.structured["counts"]["matching_files"], 12);
    assert!(report.text.is_empty());
}

#[test]
fn negative_search_is_complete_but_invalid_inputs_are_errors() {
    let fixture = Fixture::new("errors");
    fixture.write("src/safe.js", "export const unrelated = 1;\n");
    let negative = fixture.ask("uniqueMissingTerm", &[], 1000);
    assert_eq!(negative.status, ScoutStatus::Complete);
    assert!(candidates(&negative).is_empty());
    assert_eq!(negative.structured["counts"]["read"], 1);
    assert_eq!(fixture.ask("", &[], 0).status, ScoutStatus::Error);
    assert_eq!(
        fixture.ask(&"x".repeat(65_537), &[], 0).status,
        ScoutStatus::Error
    );
    assert_eq!(
        fixture.ask("report", &[&"x".repeat(161)], 0).status,
        ScoutStatus::Error
    );
    let request = Request {
        report: "retryTimeout".into(),
        terms: Vec::new(),
        paths: Vec::new(),
        budget: 0,
        max_files: 0,
    };
    assert_eq!(
        diagnose_report(&fixture.0, &request).status,
        ScoutStatus::Error
    );
    assert_eq!(
        diagnose_report(&fixture.0.join("missing"), &request).status,
        ScoutStatus::Error
    );
}

#[test]
fn outside_paths_symlinks_and_fifo_never_supply_evidence() {
    let fixture = Fixture::new("confined");
    let outside = Fixture::new("outside");
    outside.write(
        "secret.js",
        "export const secretNeedle = 'PRIVATE_OUTSIDE';\n",
    );
    fixture.write("src/safe.js", "export const secretNeedle = 'inside';\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.0.join("secret.js"), fixture.0.join("leak.js")).unwrap();
    assert!(Command::new("/usr/bin/mkfifo")
        .arg(fixture.0.join("pipe.js"))
        .status()
        .unwrap()
        .success());
    let report = fixture.ask(
        &format!(
            "{}:1 ../secret.js:1 leak.js:1 pipe.js:1 secretNeedle",
            outside.0.join("secret.js").display()
        ),
        &["secretNeedle"],
        12_000,
    );
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(candidates(&report).len(), 1);
    assert_eq!(candidates(&report)[0]["path"], "src/safe.js");
    assert!(!serde_json::to_string(&report.structured)
        .unwrap()
        .contains("PRIVATE_OUTSIDE"));
    assert!(
        report.structured["counts"]["ignored_trace_paths"]
            .as_u64()
            .unwrap()
            >= 3
    );
    for scope in ["../", "leak.js", "pipe.js"] {
        let request = Request {
            report: "secretNeedle".into(),
            terms: Vec::new(),
            paths: vec![scope.into()],
            budget: 0,
            max_files: 8,
        };
        let result = diagnose_report(&fixture.0, &request);
        // A FIFO is an explicit scope, but discovery excludes it without reading it.
        assert!(result.status == ScoutStatus::Error || candidates(&result).is_empty());
    }
}

#[test]
fn literal_git_scope_does_not_expand_brackets_or_read_ignored_files() {
    let fixture = Fixture::new("git-literal");
    fixture.git(&["init", "-q"]);
    fixture.write("src/a[1].rs", "fn uniqueNeedle() {}\n");
    fixture.write(
        "src/a1.rs",
        "fn uniqueNeedle() { panic!(\"WRONG_GLOB\"); }\n",
    );
    fixture.write("ignored.js", "const uniqueNeedle = 'IGNORED';\n");
    fixture.write(".gitignore", "ignored.js\n");
    let request = Request {
        report: "uniqueNeedle".into(),
        terms: Vec::new(),
        paths: vec!["src/a[1].rs".into()],
        budget: 12_000,
        max_files: 8,
    };
    let report = diagnose_report(&fixture.0, &request);
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(report.structured["counts"]["read"], 1);
    assert_eq!(candidates(&report)[0]["path"], "src/a[1].rs");
    assert!(!report.text.contains("WRONG_GLOB"));
    let all = fixture.ask("uniqueNeedle", &[], 12_000);
    assert!(!candidates(&all)
        .iter()
        .any(|candidate| candidate["path"] == "ignored.js"));
}

#[test]
fn missing_tracked_candidate_makes_the_scan_partial_at_zero_budget() {
    let fixture = Fixture::new("missing-tracked");
    fixture.git(&["init", "-q"]);
    fixture.write("tracked.js", "function uniqueNeedle() {}\n");
    fixture.git(&["add", "tracked.js"]);
    std::fs::remove_file(fixture.0.join("tracked.js")).unwrap();
    let report = fixture.ask("uniqueNeedle", &[], 0);
    assert_eq!(report.status, ScoutStatus::Partial);
    assert_eq!(report.structured["status"], "partial");
    assert_eq!(report.structured["counts"]["errors"], 1);
    assert!(report.structured["reason"].is_string());
}

#[test]
fn total_source_budget_is_global_across_scout_batches() {
    let fixture = Fixture::new("scan-budget");
    let source = format!("const uniqueNeedle = 1;\n{}", " ".repeat(299_970));
    for index in 0..70 {
        fixture.write(&format!("src/file{index:03}.js"), &source);
    }
    let report = fixture.ask("uniqueNeedle", &[], 0);
    assert_eq!(report.status, ScoutStatus::Partial);
    assert!(report.structured["counts"]["read"].as_u64().unwrap() < 70);
    assert!(
        report.structured["counts"]["matching_files"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn structured_payload_and_excerpts_remain_bounded_on_long_control_character_lines() {
    let fixture = Fixture::new("metadata-budget");
    for index in 0..20 {
        fixture.write(
            &format!("src/file{index:02}.js"),
            format!(
                "function uniqueNeedle() {{\n{}\n}}\n",
                "\u{1}".repeat(30_000)
            ),
        );
    }
    let request = Request {
        report: "uniqueNeedle".into(),
        terms: Vec::new(),
        paths: Vec::new(),
        budget: 65_536,
        max_files: 16,
    };
    let report = diagnose_report(&fixture.0, &request);
    assert_eq!(report.status, ScoutStatus::Complete);
    assert!(serde_json::to_vec(&report.structured).unwrap().len() <= 256 * 1024);
    assert!(report.text.len() <= 65_536);
    assert_eq!(candidates(&report).len(), 16);
    assert!(candidates(&report)
        .iter()
        .all(|candidate| candidate["evidence_truncated"] == true));
}

#[test]
fn path_mentions_without_line_numbers_do_not_invent_trace_lines() {
    let fixture = Fixture::new("path-only");
    fixture.write("src/config.js", "export const mode = 1;\n");
    let report = fixture.ask("Something about src/config.js", &[], 12_000);
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(
        candidates(&report)[0]["signals"]["trace_lines"],
        serde_json::json!([])
    );
    assert_eq!(report.structured["counts"]["trace_anchors"], 0);
    assert!(!candidates(&report)[0]["evidence"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn advertised_schema_is_an_object_and_error_results_keep_required_fields() {
    let fixture = Fixture::new("schema");
    let schema = output_schema();
    assert_eq!(schema["type"], "object");
    let report = fixture.ask("", &[], 0);
    for field in schema["required"].as_array().unwrap() {
        assert!(
            report.structured.get(field.as_str().unwrap()).is_some(),
            "missing {field}"
        );
    }
    assert_eq!(report.structured["schema_version"], 1);
    assert_eq!(report.structured["tool"], "diagnose");
    assert!(Path::new(report.structured["cwd"].as_str().unwrap()).is_absolute());
}

#[test]
fn ambiguous_path_suffixes_and_traversal_never_become_trace_anchors() {
    let fixture = Fixture::new("ambiguous-path");
    for name in [
        "src/spaced name.rs",
        "name.rs",
        "src/a:b.rs",
        "b.rs",
        "src/safe.rs",
    ] {
        fixture.write(name, "fn uniqueNeedle() {}\n");
    }
    for report in [
        "src/spaced name.rs:3",
        "src/a:b.rs:3",
        "../../src/safe.rs:3",
    ] {
        let result = fixture.ask(report, &["uniqueNeedle"], 12_000);
        assert_eq!(result.status, ScoutStatus::Complete);
        assert_eq!(result.structured["counts"]["trace_anchors"], 0, "{report}");
        for candidate in candidates(&result) {
            assert_eq!(
                candidate["signals"]["trace_rank"],
                Value::Null,
                "{report}: {candidate}"
            );
        }
    }
}

#[test]
fn late_file_with_more_distinct_terms_displaces_weaker_retained_candidates() {
    let fixture = Fixture::new("late-file");
    for index in 0..1000 {
        fixture.write(
            &format!("f{index:04}.rs"),
            "fn foo() { let _ = \"needle_00\"; }\n",
        );
    }
    fixture.write(
        "z_best.rs",
        "fn best() { let _ = \"needle_00 needle_01 needle_02 needle_03 needle_04 needle_05\"; }\n",
    );
    let report = fixture.ask(
        "",
        &[
            "needle_00",
            "needle_01",
            "needle_02",
            "needle_03",
            "needle_04",
            "needle_05",
        ],
        12_000,
    );
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(report.structured["counts"]["read"], 1001);
    assert_eq!(candidates(&report)[0]["path"], "z_best.rs");
    assert_eq!(report.structured["metadata_truncated"], true);
}

#[test]
fn excerpts_include_late_distinctive_terms_and_do_not_repeat_overlapping_lines() {
    let fixture = Fixture::new("late-line");
    let mut source = String::from("pub fn investigate() {\n");
    source.push_str(&"    let _ = \"needle_00\";\n".repeat(60));
    source.push_str("    if true {\n        let _ = \"needle_01\";\n    }\n}\n");
    fixture.write("src/late.rs", source);
    let report = fixture.ask("", &["needle_00", "needle_01"], 12_000);
    let rows = candidates(&report)[0]["evidence"].as_array().unwrap();
    assert!(rows
        .iter()
        .any(|row| row["text"].as_str().unwrap().contains("needle_01")));
    let mut covered = std::collections::BTreeSet::new();
    for row in rows {
        for line in row["start_line"].as_u64().unwrap()..=row["end_line"].as_u64().unwrap() {
            assert!(covered.insert(line), "duplicate excerpt source line {line}");
        }
    }
}

#[test]
fn documented_binary_exclusions_do_not_exhaust_the_text_scan_budget() {
    let fixture = Fixture::new("excluded-assets");
    for index in 0..70 {
        let file = std::fs::File::create(fixture.0.join(format!("asset{index:03}.png"))).unwrap();
        file.set_len(300_000).unwrap();
    }
    fixture.write("z_source.rs", "fn uniqueNeedle() {}\n");
    let report = fixture.ask("uniqueNeedle", &[], 0);
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(report.structured["counts"]["skipped"], 70);
    assert_eq!(report.structured["counts"]["read"], 1);
    assert_eq!(candidates(&report)[0]["path"], "z_source.rs");
}

#[test]
fn oversized_invalid_project_name_cannot_expand_error_metadata_without_bound() {
    let request = Request {
        report: "uniqueNeedle".into(),
        terms: Vec::new(),
        paths: Vec::new(),
        budget: 0,
        max_files: 8,
    };
    let report = diagnose_report(Path::new(&"é".repeat(200_000)), &request);
    assert_eq!(report.status, ScoutStatus::Error);
    assert!(report.text.is_empty());
    assert_eq!(report.structured["metadata_truncated"], true);
    assert!(serde_json::to_vec(&report.structured).unwrap().len() <= 256 * 1024);
}

#[test]
fn escaped_paths_and_terms_cannot_displace_file_headers_or_exceed_encoded_budget() {
    let fixture = Fixture::new("encoded-budget");
    let terms: Vec<_> = (0..24)
        .map(|index| format!("needle_{index:02}_{}", "\u{1}".repeat(140)))
        .collect();
    let prefix = (0..8)
        .map(|index| format!("d{index}{}", "\u{1}".repeat(100)))
        .collect::<Vec<_>>()
        .join("/");
    let paths: Vec<_> = (0..16)
        .map(|index| format!("{prefix}/source{index:02}.txt"))
        .collect();
    // Different widths exercise packing boundaries rather than relying on raw
    // byte counts or one fortunate number of detailed excerpts fitting.
    for width in [80, 110, 140] {
        let source = terms
            .iter()
            .map(|term| format!("{term}{}\n", "\u{1}".repeat(width)))
            .collect::<String>();
        for path in &paths {
            fixture.write(path, &source);
        }
        let request = Request {
            report: String::new(),
            terms: terms.clone(),
            paths: Vec::new(),
            budget: 0,
            max_files: 16,
        };
        let result = diagnose_report(&fixture.0, &request);
        assert_eq!(result.status, ScoutStatus::Complete);
        assert_eq!(candidates(&result).len(), 16);
        assert!(serde_json::to_vec(&result.structured).unwrap().len() <= 256 * 1024);
        for path in &paths {
            assert!(candidates(&result)
                .iter()
                .any(|candidate| candidate["path"] == *path));
        }
    }
}
