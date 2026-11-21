//! Tree-sitter should improve context without changing Scout's lexical evidence.
use kiro::engine::Session;
use kiro::mcp::respond;
use kiro::scout::{scout_report, Request, ScoutReport, ScoutStatus};
use serde_json::{json, Value};
use std::path::PathBuf;

struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("kiro-scout-syntax-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn write(&self, name: &str, source: impl AsRef<[u8]>) {
        std::fs::write(self.0.join(name), source).unwrap();
    }
    fn ask(&self, terms: &[&str], budget: usize) -> ScoutReport {
        scout_report(
            &self.0,
            &Request {
                terms: terms.iter().map(|s| s.to_string()).collect(),
                paths: Vec::new(),
                budget,
            },
        )
    }
    fn mcp(&self, terms: &[&str], budget: usize) -> Value {
        respond(
            &mut Session {
                dir: self.0.clone(),
            },
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":"scout","arguments":{"terms":terms,"budget":budget}}}),
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

#[test]
fn rust_multiline_signature_is_found_without_indentation() {
    let f = Fixture::new("rust-signature");
    let source = "pub async fn actual<T>(\nvalue: T,\n) -> &'static str\nwhere T: Copy,\n{\nlet decoy = \"fn fake() {\";\nlet result = \"needle\";\nresult\n}\n";
    f.write("service.rs", source);
    let report = f.ask(&["needle"], 8000);
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(report.counts.matching_lines, 1);
    let file = report
        .structure
        .iter()
        .find(|s| s.path == "service.rs")
        .unwrap();
    assert_eq!(file.language, Some("rust"));
    assert_eq!(file.status.as_str(), "parsed");
    let scope = file
        .definitions
        .iter()
        .find(|s| s.name.as_deref() == Some("actual"))
        .unwrap();
    assert_eq!((scope.start_line, scope.end_line), (1, 9));
    assert!(scope.header_end_line >= 4);
    assert_eq!(&source[scope.start_byte..scope.end_byte], source.trim_end());
    assert!(
        report.text.contains("pub async fn actual<T>("),
        "{}",
        report.text
    );
    assert!(report.text.contains("where T: Copy,"), "{}", report.text);
    assert!(!file
        .definitions
        .iter()
        .any(|s| s.name.as_deref() == Some("fake")));
}

#[test]
fn javascript_strings_and_comments_do_not_create_fake_scopes() {
    let f = Fixture::new("js-decoys");
    let source = "// function bogus() {\nconst decoy = 'function imagined() {';\nexport function real(\nvalue,\n) {\nconst braces = '} } {';\nreturn 'needle';\n}\n";
    f.write("service.mjs", source);
    let report = f.ask(&["needle"], 8000);
    let file = report
        .structure
        .iter()
        .find(|s| s.path == "service.mjs")
        .unwrap();
    assert_eq!(file.language, Some("javascript"));
    assert_eq!(file.status.as_str(), "parsed");
    assert!(file
        .definitions
        .iter()
        .any(|s| s.name.as_deref() == Some("real") && s.end_line == 8));
    assert!(!file
        .definitions
        .iter()
        .any(|s| matches!(s.name.as_deref(), Some("bogus" | "imagined"))));
    assert!(
        report.text.contains("export function real("),
        "{}",
        report.text
    );
    assert!(report.text.contains("value,"), "{}", report.text);
}

#[test]
fn typescript_nested_methods_and_arrows_have_independent_source_ranges() {
    let f = Fixture::new("ts-nested");
    let source = "export class Service {\nasync execute<T>(\nvalue: T,\n): Promise<string> {\nconst handler = (input: T): string => {\nreturn 'needle';\n};\nreturn handler(value);\n}\n}\n";
    f.write("service.ts", source);
    let report = f.ask(&["needle"], 8000);
    let file = report
        .structure
        .iter()
        .find(|s| s.path == "service.ts")
        .unwrap();
    assert_eq!(file.language, Some("typescript"));
    assert_eq!(file.status.as_str(), "parsed");
    for (name, start, end) in [("Service", 1, 10), ("execute", 2, 9), ("handler", 5, 7)] {
        let scope = file
            .definitions
            .iter()
            .find(|s| s.name.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("missing {name}: {:?}", file.definitions));
        assert_eq!((scope.start_line, scope.end_line), (start, end), "{name}");
        assert!(
            source.is_char_boundary(scope.start_byte) && source.is_char_boundary(scope.end_byte)
        );
        assert!(
            scope.start_line <= report.matches[0].line_number
                && scope.end_line >= report.matches[0].line_number
        );
    }
    assert_eq!(report.counts.matching_lines, 1);
}

#[test]
fn tsx_components_keep_jsx_and_string_braces_inside_the_function() {
    let f = Fixture::new("tsx");
    let source = "export function Card(\nprops: {label: string},\n) {\nreturn <section data-pattern={'{}'}>{'needle'}{props.label}</section>;\n}\n";
    f.write("card.tsx", source);
    let report = f.ask(&["needle"], 8000);
    let file = report
        .structure
        .iter()
        .find(|s| s.path == "card.tsx")
        .unwrap();
    assert_eq!(file.language, Some("tsx"));
    assert_eq!(file.status.as_str(), "parsed");
    let scope = file
        .definitions
        .iter()
        .find(|s| s.name.as_deref() == Some("Card"))
        .unwrap();
    assert_eq!((scope.start_line, scope.end_line), (1, 5));
    assert!(
        report.text.contains("export function Card("),
        "{}",
        report.text
    );
    assert!(
        report.text.contains("props: {label: string}"),
        "{}",
        report.text
    );
}

#[test]
fn unicode_crlf_and_unterminated_eof_keep_original_byte_offsets() {
    let f = Fixture::new("crlf");
    let source = "// café 💡\r\nfunction été(\r\nvalue,\r\n) {\r\nreturn 'needle é';\r\n}";
    f.write("unicode.js", source);
    let report = f.ask(&["needle"], 0);
    assert!(report.text.is_empty());
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(report.matches[0].line_number, 5);
    let file = report
        .structure
        .iter()
        .find(|s| s.path == "unicode.js")
        .unwrap();
    assert_eq!(file.status.as_str(), "parsed");
    let scope = file
        .definitions
        .iter()
        .find(|s| s.name.as_deref() == Some("été"))
        .unwrap();
    assert_eq!((scope.start_line, scope.end_line), (2, 6));
    assert_eq!(scope.start_byte, source.find("function").unwrap());
    assert_eq!(scope.end_byte, source.len());
    assert_eq!(
        &source[scope.start_byte..scope.end_byte],
        &source[source.find("function").unwrap()..]
    );
}

#[test]
fn malformed_supported_source_preserves_hits_and_reports_incomplete_structure() {
    let f = Fixture::new("malformed");
    f.write("broken.ts", "export function broken( {\nreturn 'needle';\n");
    let report = f.ask(&["needle"], 8000);
    assert_eq!(
        report.status,
        ScoutStatus::Complete,
        "syntax errors do not invalidate a completed lexical search"
    );
    assert_eq!(report.counts.matching_lines, 1);
    let file = report
        .structure
        .iter()
        .find(|s| s.path == "broken.ts")
        .unwrap();
    assert_eq!(file.status.as_str(), "partial");
    assert!(report.text.contains("needle"));
    assert!(
        report.text.to_lowercase().contains("partial")
            || report.text.to_lowercase().contains("fallback"),
        "{}",
        report.text
    );
}

#[test]
fn unknown_languages_and_invalid_utf8_have_explained_lexical_fallback() {
    let f = Fixture::new("fallback");
    f.write("legacy.xyz", "function legacy() {\n  needle\n}\n");
    f.write("bytes.rs", b"fn damaged() {\n  // needle \xff\n}\n");
    let report = f.ask(&["needle"], 8000);
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(report.counts.matching_lines, 2);
    let unknown = report
        .structure
        .iter()
        .find(|s| s.path == "legacy.xyz")
        .unwrap();
    assert_eq!(unknown.status.as_str(), "unsupported");
    assert!(unknown.definitions.is_empty());
    let damaged = report
        .structure
        .iter()
        .find(|s| s.path == "bytes.rs")
        .unwrap();
    assert_ne!(damaged.status.as_str(), "parsed");
    assert!(damaged.definitions.is_empty());
    assert!(report.text.contains("function legacy()"), "{}", report.text);
    assert!(
        report.text.to_lowercase().contains("fallback")
            || report.text.to_lowercase().contains("lexical heuristic"),
        "{}",
        report.text
    );
}

#[test]
fn source_size_and_display_budgets_preserve_lexical_limits() {
    let f = Fixture::new("bounds");
    f.write("small.rs", "fn source() {\nlet needle = 1;\n}\n");
    f.write("large.rs", format!("// needle\n{}", " ".repeat(400_000)));
    for budget in [0, 1, 42, 256, 8000, usize::MAX] {
        let report = f.ask(&["needle"], budget);
        assert_eq!(report.status, ScoutStatus::Complete);
        assert_eq!(report.counts.matching_files, 1);
        assert_eq!(report.counts.skipped, 1);
        assert!(report.text.len() <= budget.min(65_536));
        assert_eq!(report.structure.len(), 1);
        assert_eq!(report.structure[0].status.as_str(), "parsed");
        assert!(report.structure[0]
            .definitions
            .iter()
            .any(|s| s.name.as_deref() == Some("source")));
    }
}

#[test]
fn structure_definitions_are_relevant_and_deduplicated_per_file() {
    let f = Fixture::new("dedup");
    let source = "function relevant() {\nreturn ['needle',\n'needle'];\n}\nfunction unrelated() { return 2; }\n";
    f.write("service.js", source);
    let report = f.ask(&["needle"], 8000);
    assert_eq!(report.counts.matching_lines, 2);
    let definitions = &report.structure[0].definitions;
    assert_eq!(
        definitions
            .iter()
            .filter(|s| s.name.as_deref() == Some("relevant"))
            .count(),
        1
    );
    assert!(!definitions
        .iter()
        .any(|s| s.name.as_deref() == Some("unrelated")));
}

#[test]
fn mcp_exposes_scout_v2_structure_even_when_text_budget_is_zero() {
    let f = Fixture::new("mcp");
    f.write("service.rs", "fn explain() {\nlet needle = 1;\n}\n");
    let result = f.mcp(&["needle"], 0);
    assert_eq!(result["isError"], false);
    assert_eq!(result["content"][0]["text"], "");
    let data = &result["structuredContent"];
    assert_eq!(data["schema_version"], 2);
    assert_eq!(data["status"], "complete");
    assert_eq!(data["structure_truncated"], false);
    assert_eq!(data["structure"][0]["path"], "service.rs");
    assert_eq!(data["structure"][0]["language"], "rust");
    assert_eq!(data["structure"][0]["status"], "parsed");
    assert!(data["structure"][0]["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["name"] == "explain"));
    let list = respond(
        &mut Session { dir: f.0.clone() },
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    let scout = tools.iter().find(|t| t["name"] == "scout").unwrap();
    let run = tools.iter().find(|t| t["name"] == "run").unwrap();
    assert_eq!(
        scout["outputSchema"]["properties"]["schema_version"]["const"],
        2
    );
    assert_eq!(
        run["outputSchema"]["properties"]["schema_version"]["const"],
        1
    );
    assert!(scout["outputSchema"]["properties"]["structure"].is_object());
    let invalid = f.mcp(&[" "], 0);
    assert_eq!(invalid["isError"], true);
    assert_eq!(invalid["structuredContent"]["schema_version"], 2);
    assert_eq!(invalid["structuredContent"]["structure"], json!([]));
}

#[test]
fn syntax_file_budget_does_not_shorten_the_lexical_search() {
    let f = Fixture::new("syntax-file-budget");
    for index in 0..260 {
        f.write(
            &format!("file-{index:03}.js"),
            "function item() { return 'needle'; }\n",
        );
    }
    let report = f.ask(&["needle"], 0);
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(report.counts.matching_files, 260);
    assert_eq!(report.counts.matching_lines, 260);
    assert_eq!(report.matches.len(), 260);
    assert!(report.structure.len() <= 256);
    assert!(report.structure_truncated);
    assert!(
        report
            .structure
            .iter()
            .filter(|file| file.status.as_str() == "parsed")
            .count()
            <= 256
    );
}

#[test]
fn syntax_byte_budget_is_distinct_from_complete_lexical_coverage() {
    let f = Fixture::new("syntax-byte-budget");
    let source = format!("fn item() {{ let needle = 1; }}\n//{}", " ".repeat(350_000));
    for index in 0..16 {
        f.write(&format!("file-{index:02}.rs"), &source);
    }
    let report = f.ask(&["needle"], 0);
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(report.counts.matching_files, 16);
    assert_eq!(report.counts.matching_lines, 16);
    assert_eq!(report.matches.len(), 16);
    assert_eq!(report.structure.len(), 16);
    assert!(!report.structure_truncated);
    assert!(report
        .structure
        .iter()
        .any(|file| file.status.as_str() == "limited"));
    for file in &report.structure {
        if file.status.as_str() == "limited" {
            assert!(file.definitions.is_empty());
        }
    }
}

#[test]
fn overly_long_symbol_names_are_omitted_instead_of_misreported() {
    let f = Fixture::new("long-symbol");
    let name = "named".repeat(40);
    let source = format!("function {name}() {{ return 'needle'; }}\n");
    f.write("service.js", &source);
    let report = f.ask(&["needle"], 0);
    assert_eq!(report.status, ScoutStatus::Complete);
    let file = &report.structure[0];
    assert_eq!(file.status.as_str(), "parsed");
    assert_eq!(file.definitions.len(), 1);
    assert!(file.definitions[0].name.is_none());
    assert_eq!(
        &source[file.definitions[0].start_byte..file.definitions[0].end_byte],
        source.trim_end()
    );
}

#[test]
fn configuration_bindings_keep_their_declaring_header() {
    let f = Fixture::new("configuration");
    let source = "export const schema = z.object({\n  retries: z\n    .array(z.number())\n    .max(2),\n})\n";
    f.write("schema.ts", source);
    let report = f.ask(&["retries"], 8000);
    assert_eq!(report.structure[0].status.as_str(), "parsed");
    let definition = report.structure[0]
        .definitions
        .iter()
        .find(|scope| scope.name.as_deref() == Some("schema"))
        .unwrap();
    assert_eq!(definition.kind, "variable");
    assert_eq!((definition.start_line, definition.end_line), (1, 5));
    assert!(
        report.text.contains("export const schema = z.object({"),
        "{}",
        report.text
    );
    assert!(report.text.contains(".max(2)"));
}

#[test]
fn long_signatures_are_explicitly_abbreviated_without_hiding_the_hit() {
    let f = Fixture::new("long-signature");
    let parameters = (0..20)
        .map(|i| format!("parameter_{i},\n"))
        .collect::<String>();
    let source = format!("function investigate(\n{parameters}) {{\nreturn 'needle';\n}}\n");
    f.write("long.js", &source);
    let report = f.ask(&["needle"], 8000);
    assert_eq!(report.status, ScoutStatus::Complete);
    assert!(report.text.contains("function investigate("));
    assert!(report.text.contains("[signature abbreviated; read lines"));
    assert!(report.text.contains("return 'needle'"));
    assert!(!report.text.contains("parameter_15"));
    assert_eq!(report.structure[0].definitions[0].header_end_line, 22);
}

#[test]
fn shared_metadata_allocations_remain_bounded_under_many_declarations_and_hits() {
    use kiro::scout::{ScoutFileStructure, ScoutMatch};
    use kiro::syntax::Scope;
    let f = Fixture::new("shared-metadata");
    let source = (0..800)
        .map(|i| {
            format!(
                "function f_{i}_{}() {{\n{}}}\n",
                "n".repeat(96),
                "void 'needle';\n".repeat(5)
            )
        })
        .collect::<String>();
    f.write("many.js", &source);
    let report = f.ask(&["needle"], 0);
    assert_eq!(report.status, ScoutStatus::Complete);
    assert_eq!(report.counts.matching_lines, 4000);
    assert!(report.metadata_truncated || report.structure_truncated);
    let allocated = report.matches.capacity() * std::mem::size_of::<ScoutMatch>()
        + report
            .matches
            .iter()
            .map(|hit| {
                hit.path.capacity() + hit.term_indices.capacity() * std::mem::size_of::<usize>()
            })
            .sum::<usize>()
        + report.structure.capacity() * std::mem::size_of::<ScoutFileStructure>()
        + report
            .structure
            .iter()
            .map(|file| {
                file.path.capacity()
                    + file.definitions.capacity() * std::mem::size_of::<Scope>()
                    + file
                        .definitions
                        .iter()
                        .filter_map(|scope| scope.name.as_ref())
                        .map(String::capacity)
                        .sum::<usize>()
            })
            .sum::<usize>();
    assert!(
        allocated <= 256 * 1024,
        "retained metadata allocations: {allocated}"
    );
}
