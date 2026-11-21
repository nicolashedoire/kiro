//! Regressions found while comparing Scout before and after Tree-sitter.
use kiro::engine::Session;
use kiro::mcp::respond;
use kiro::scout::{scout_report, Request, ScoutReport, ScoutStatus};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("kiro-scout-context-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, name: &str, source: impl AsRef<[u8]>) {
        std::fs::write(self.0.join(name), source).unwrap();
    }

    fn ask(&self, budget: usize) -> ScoutReport {
        scout_report(
            &self.0,
            &Request {
                terms: vec!["needle".to_string()],
                paths: Vec::new(),
                budget,
            },
        )
    }

    fn mcp(&self, budget: usize) -> Value {
        respond(
            &mut Session {
                dir: self.0.clone(),
            },
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":"scout","arguments":{"terms":["needle"],"budget":budget}}}),
        )
        .unwrap()["result"]
            .clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn long_signature_fixture(fixture: &Fixture) {
    let mut source = String::new();
    for function in 0..14 {
        source.push_str(&format!("function verbose_{function}(\n"));
        for parameter in 0..12 {
            source.push_str(&format!("  parameter_{parameter}_{},\n", "x".repeat(160)));
        }
        source.push_str(") {\n  return \"needle\";\n}\n\n");
    }
    fixture.write("a-verbose.js", source);
    fixture.write(
        "b-critical.js",
        "export function critical() {\n  return \"needle\";\n}\n",
    );
}

#[test]
fn long_first_file_keeps_other_matching_paths_and_real_hits_visible() {
    let fixture = Fixture::new("long-first-file");
    long_signature_fixture(&fixture);
    let report = fixture.ask(12_000);
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(report.counts.matching_files, 2);
    assert_eq!(report.counts.matching_lines, 15);
    assert_eq!(report.matches.len(), 15);
    assert!(report.text.len() <= 12_000);
    for path in ["a-verbose.js", "b-critical.js"] {
        assert!(
            report.text.contains(path),
            "missing {path}: {}",
            report.text
        );
    }
    assert!(
        report.text.contains("return \"needle\";"),
        "{}",
        report.text
    );

    let result = fixture.mcp(12_000);
    assert_eq!(result["isError"], false);
    assert_eq!(result["structuredContent"]["schema_version"], 2);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("b-critical.js"));
    assert!(text.len() <= 12_000);
}

#[test]
fn cli_scout_keeps_the_secondary_path_in_its_default_budget() {
    let fixture = Fixture::new("cli-default");
    long_signature_fixture(&fixture);
    let result = Command::new(env!("CARGO_BIN_EXE_kiro"))
        .args(["scout", "--project"])
        .arg(&fixture.0)
        .arg("needle")
        .output()
        .unwrap();
    assert!(result.status.success(), "{:?}", result.stderr);
    let text = std::str::from_utf8(&result.stdout).unwrap();
    // The CLI may append its own final newline after the bounded Scout answer.
    assert!(text.strip_suffix('\n').unwrap_or(text).len() <= 12_000);
    assert!(text.contains("a-verbose.js"));
    assert!(text.contains("b-critical.js"), "{text}");
}

#[test]
fn small_unicode_budgets_are_strict_and_omissions_are_observable() {
    let fixture = Fixture::new("unicode-budgets");
    let paths: Vec<String> = (0..24)
        .map(|index| format!("module-{index:02}-{}.js", "é💡".repeat(12)))
        .collect();
    for path in &paths {
        fixture.write(
            path,
            "export function investigate() {\n  return 'needle é💡';\n}\n",
        );
    }
    for budget in [0, 1, 32, 128, 512, 1024, 12_000, 65_536, usize::MAX] {
        let report = fixture.ask(budget);
        assert_eq!(report.status, ScoutStatus::Complete, "budget {budget}");
        assert_eq!(report.counts.matching_files, 24);
        assert_eq!(report.counts.matching_lines, 24);
        assert_eq!(report.matches.len(), 24);
        assert!(report.text.len() <= budget.min(65_536), "budget {budget}");
        if budget <= 1 {
            assert!(report.display_truncated, "budget {budget}");
        } else if paths.iter().any(|path| !report.text.contains(path)) {
            let lower = report.text.to_ascii_lowercase();
            assert!(
                report.display_truncated
                    || lower.contains("more file")
                    || lower.contains("omitted")
                    || lower.contains("truncat"),
                "omissions need a notice at budget {budget}: {}",
                report.text
            );
        }
    }
}

#[test]
fn parsed_files_without_recognized_scopes_keep_marked_heuristic_context() {
    for (name, filename, source, header) in [
        (
            "commonjs",
            "config.js",
            "module.exports = {\n  schema: 'needle',\n};\n",
            "module.exports = {",
        ),
        (
            "exports-call",
            "config.js",
            "exports.config = configure({\n  schema: 'needle',\n});\n",
            "exports.config = configure({",
        ),
        (
            "macro-definition",
            "routes.rs",
            "macro_rules! register {\n    () => {\n        fn investigate() {\n            let _ = \"needle\";\n        }\n    };\n}\n",
            "fn investigate() {",
        ),
        (
            "macro-invocation",
            "routes.rs",
            "register! {\n    fn investigate() {\n        let _ = \"needle\";\n    }\n}\n",
            "fn investigate() {",
        ),
    ] {
        let fixture = Fixture::new(name);
        fixture.write(filename, source);
        let report = fixture.ask(8000);
        assert_eq!(report.status, ScoutStatus::Complete, "{name}");
        assert_eq!(report.counts.matching_lines, 1, "{name}");
        assert_eq!(report.structure[0].status.as_str(), "parsed", "{name}");
        assert!(report.text.contains(header), "{name}: {}", report.text);
        assert!(
            report.text.to_ascii_lowercase().contains("heuristic"),
            "fallback must be explicit for {name}: {}",
            report.text
        );
    }
}

#[test]
fn sibling_scopes_on_one_line_do_not_invent_an_enclosing_heuristic_header() {
    let fixture = Fixture::new("ambiguous-siblings");
    fixture.write(
        "ambiguous.js",
        "if (true) {\n  function first() { return 'needle'; } function second() { return 'needle'; }\n}\n",
    );
    let report = fixture.ask(8000);
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(report.counts.matching_lines, 1);
    assert!(report.text.contains("function first()"));
    assert!(report.text.contains("function second()"));
    assert!(
        !report.text.contains("if (true) {"),
        "the unowned line must not acquire a guessed parent: {}",
        report.text
    );
}

#[test]
fn anonymous_callbacks_keep_a_named_parent_and_the_callback_without_duplicates() {
    for (name, filename, source, parent, callback) in [
        (
            "react-effect",
            "checkout.tsx",
            "export function Checkout() {\n  useEffect(() => {\n    log('needle');\n  }, []);\n  return null;\n}\n",
            "export function Checkout() {",
            "useEffect(() => {",
        ),
        (
            "async-job",
            "payments.js",
            "export async function startPayments() {\n  await queue.process(async (job) => {\n    log('needle', job);\n  });\n}\n",
            "export async function startPayments() {",
            "queue.process(async (job) => {",
        ),
        (
            "rust-closure",
            "payments.rs",
            "pub fn start_payments() {\n    queue(|job| {\n        let _ = \"needle\";\n    });\n}\n",
            "pub fn start_payments() {",
            "queue(|job| {",
        ),
    ] {
        let fixture = Fixture::new(name);
        fixture.write(filename, source);
        let report = fixture.ask(8000);
        assert_eq!(report.status, ScoutStatus::Complete);
        assert_eq!(report.structure[0].status.as_str(), "parsed", "{name}");
        for header in [parent, callback] {
            assert_eq!(
                report.text.matches(header).count(),
                1,
                "header must occur once for {name}: {}",
                report.text
            );
        }
        assert_eq!(report.counts.matching_lines, 1);
        let expected_hit = if filename.ends_with(".rs") {
            "let _ = \"needle\";"
        } else {
            "log('needle'"
        };
        assert!(report.text.contains(expected_hit), "{name}: {}", report.text);
    }
}

#[test]
fn successive_anonymous_callbacks_share_the_named_parent_only_once() {
    let fixture = Fixture::new("successive-callbacks");
    fixture.write(
        "checkout.tsx",
        "export function Checkout() {\n  useEffect(() => {\n    log('needle first');\n  }, []);\n  queue.then(() => {\n    log('needle second');\n  });\n}\n",
    );
    let report = fixture.ask(8000);
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(report.counts.matching_lines, 2);
    for header in [
        "export function Checkout() {",
        "useEffect(() => {",
        "queue.then(() => {",
    ] {
        assert_eq!(report.text.matches(header).count(), 1, "{}", report.text);
    }
    assert!(report.text.contains("log('needle first');"));
    assert!(report.text.contains("log('needle second');"));
}

#[test]
fn transparent_arrow_wrappers_keep_the_binding_and_exact_source_ranges() {
    for (name, filename, source, callback) in [
        (
            "parenthesized-arrow",
            "service.js",
            "export const investigate = (\n  (value) => {\n    return 'needle';\n  }\n);\n",
            "(value) => {",
        ),
        (
            "satisfies-arrow",
            "service.ts",
            "export const investigate = (\n  (value: string) => {\n    return 'needle';\n  }\n) satisfies Handler;\n",
            "(value: string) => {",
        ),
        (
            "as-arrow",
            "service.ts",
            "export const investigate = (\n  (value: string) => {\n    return 'needle';\n  }\n) as Handler;\n",
            "(value: string) => {",
        ),
        (
            "nonnull-arrow",
            "service.ts",
            "export const investigate = (\n  (value: string) => {\n    return 'needle';\n  }\n)!;\n",
            "(value: string) => {",
        ),
    ] {
        let fixture = Fixture::new(name);
        fixture.write(filename, source);
        let report = fixture.ask(8000);
        assert_eq!(report.structure[0].status.as_str(), "parsed", "{name}");
        for header in ["export const investigate = (", callback] {
            assert_eq!(report.text.matches(header).count(), 1, "{name}: {}", report.text);
        }
        for definition in &report.structure[0].definitions {
            assert!(source.is_char_boundary(definition.start_byte));
            assert!(source.is_char_boundary(definition.end_byte));
            let span = &source[definition.start_byte..definition.end_byte];
            assert!(!span.is_empty());
            if let Some(name) = definition.name.as_deref() {
                assert!(span.contains(name), "name {name} is outside {span:?}");
            }
        }
    }
}

#[test]
fn callback_context_does_not_change_structured_evidence_or_exceed_signature_bounds() {
    let fixture = Fixture::new("long-parent-signature");
    let parameters = (0..20)
        .map(|i| format!("parameter_{i},\n"))
        .collect::<String>();
    fixture.write(
        "long.js",
        format!("export function Checkout(\n{parameters}) {{\n  queue.then(() => {{\n    log('needle');\n  }});\n}}\n"),
    );
    let report = fixture.ask(8000);
    assert!(
        report.text.contains("export function Checkout("),
        "{}",
        report.text
    );
    assert!(
        report.text.contains("queue.then(() => {"),
        "{}",
        report.text
    );
    assert!(report.text.contains("log('needle');"), "{}", report.text);
    assert!(
        report.text.contains("signature abbreviated"),
        "{}",
        report.text
    );
    assert!(!report.text.contains("parameter_7,"));
    assert!(report.text.len() <= 8000);
    let hidden = fixture.mcp(0);
    let visible = fixture.mcp(12_000);
    assert_eq!(hidden["content"][0]["text"], "");
    assert_eq!(hidden["isError"], false);
    for field in [
        "schema_version",
        "status",
        "counts",
        "matches",
        "structure",
        "metadata_truncated",
        "structure_truncated",
    ] {
        assert_eq!(
            hidden["structuredContent"][field], visible["structuredContent"][field],
            "{field}"
        );
    }
    assert_eq!(hidden["structuredContent"]["schema_version"], 2);
}
