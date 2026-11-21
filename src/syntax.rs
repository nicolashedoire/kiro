//! Bounded, optional syntactic context for Scout. No source code is executed.
//!
//! Syntax is evidence about declarations and their boundaries, not name resolution or
//! semantic relevance. Trees are transient; only small source spans and names survive.

use std::ops::ControlFlow;
use std::time::{Duration, Instant};
use tree_sitter::{Language, Node, ParseOptions, Parser};

const MAX_SOURCE_BYTES: usize = 400_000;
const MAX_PARSE_TIME: Duration = Duration::from_millis(100);
const MAX_VISITED_NODES: usize = 100_000;
const MAX_SCOPES: usize = 1024;
const MAX_RETAINED_BYTES: usize = 256 * 1024;
const MAX_NAME_BYTES: usize = 160;
const MAX_TRANSPARENT_WRAPPERS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Parsed,
    Partial,
    Unsupported,
    InvalidUtf8,
    Limited,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parsed => "parsed",
            Self::Partial => "partial",
            Self::Unsupported => "unsupported",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::Limited => "limited",
        }
    }
}

/// Exact source bounds: one-based inclusive lines, zero-based half-open byte offsets.
/// A name longer than the name budget is omitted, never presented as a truncated symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub kind: &'static str,
    pub name: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub header_end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Debug)]
pub struct Analysis {
    pub language: Option<&'static str>,
    pub status: Status,
    pub scopes: Vec<Scope>,
}

/// A missing declaration permits an explicitly heuristic context. Ambiguous sibling
/// declarations do not: line-only matches cannot establish which sibling owns a hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context<'a> {
    Unavailable,
    Missing,
    Ambiguous,
    Found {
        inner: &'a Scope,
        /// An anonymous callback keeps its closest useful named ancestor in view.
        named_parent: Option<&'a Scope>,
    },
}

impl Analysis {
    pub fn limited(language: Option<&'static str>) -> Self {
        Self {
            language,
            status: Status::Limited,
            scopes: Vec::new(),
        }
    }

    /// Allocations retained by this analysis, including spare vector/name capacities.
    pub fn allocated_bytes(&self) -> usize {
        self.scopes.capacity() * std::mem::size_of::<Scope>()
            + self
                .scopes
                .iter()
                .filter_map(|scope| scope.name.as_ref())
                .map(String::capacity)
                .sum::<usize>()
    }

    /// Inspect line context without conflating absent syntax, unsupported declarations
    /// and ambiguity. The lookup borrows the bounded scopes and allocates nothing.
    pub fn context(&self, line: usize) -> Context<'_> {
        if matches!(
            self.status,
            Status::Limited | Status::Unsupported | Status::InvalidUtf8
        ) {
            return Context::Unavailable;
        }
        let mut best: Option<&Scope> = None;
        for scope in &self.scopes {
            if !(scope.start_line..=scope.end_line).contains(&line) {
                continue;
            }
            if let Some(previous) = best {
                if scope.start_byte >= previous.start_byte && scope.end_byte <= previous.end_byte {
                    best = Some(scope);
                } else if !(previous.start_byte >= scope.start_byte
                    && previous.end_byte <= scope.end_byte)
                {
                    return Context::Ambiguous;
                }
            } else {
                best = Some(scope);
            }
        }
        let Some(inner) = best else {
            return Context::Missing;
        };
        let mut named_parent: Option<&Scope> = None;
        if inner.name.is_none() {
            for candidate in &self.scopes {
                if candidate.name.is_none() || !strictly_contains(candidate, inner) {
                    continue;
                }
                let preferred = named_parent.is_none_or(|previous| {
                    parent_priority(candidate) < parent_priority(previous)
                        || (parent_priority(candidate) == parent_priority(previous)
                            && strictly_contains(previous, candidate))
                });
                if preferred {
                    named_parent = Some(candidate);
                }
            }
        }
        Context::Found {
            inner,
            named_parent,
        }
    }

    /// Compatibility view returning only the innermost unambiguous declaration.
    pub fn enclosing(&self, line: usize) -> Option<&Scope> {
        match self.context(line) {
            Context::Found { inner, .. } => Some(inner),
            _ => None,
        }
    }
}

fn strictly_contains(outer: &Scope, inner: &Scope) -> bool {
    outer.start_byte <= inner.start_byte
        && outer.end_byte >= inner.end_byte
        && (outer.start_byte < inner.start_byte || outer.end_byte > inner.end_byte)
}

fn parent_priority(scope: &Scope) -> u8 {
    match scope.kind {
        "function" | "method" | "arrow_function" | "closure" => 0,
        "variable" | "constant" | "static" => 1,
        _ => 2,
    }
}

#[derive(Clone, Copy)]
enum Grammar {
    Rust,
    JavaScript,
    TypeScript,
    Tsx,
}

impl Grammar {
    fn for_path(path: &str) -> Option<Self> {
        match path.rsplit_once('.')?.1.to_ascii_lowercase().as_str() {
            "rs" => Some(Self::Rust),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
        }
    }
    fn language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }
}

/// Language selection is independent of parsing and available to caller-wide budgets.
pub fn language_name(path: &str) -> Option<&'static str> {
    Grammar::for_path(path).map(Grammar::name)
}

fn end_line(node: Node<'_>) -> usize {
    let end = node.end_position();
    if end.column == 0 && end.row > node.start_position().row {
        end.row
    } else {
        end.row + 1
    }
}

fn short_name<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    let text = source.get(node.byte_range())?;
    (text.len() <= MAX_NAME_BYTES && !text.contains(['\n', '\r'])).then_some(text)
}

/// The value child of a purely syntactic wrapper. Calls, sequences and other runtime
/// expressions are deliberately not transparent. Extra comments and types are ignored
/// only in the explicitly known grammar positions, with bounded child inspection.
fn transparent_value(node: Node<'_>) -> Option<Node<'_>> {
    let kind = node.kind();
    if !matches!(
        kind,
        "parenthesized_expression"
            | "as_expression"
            | "satisfies_expression"
            | "type_assertion"
            | "non_null_expression"
    ) || node.has_error()
    {
        return None;
    }
    let mut cursor = node.walk();
    let mut first = None;
    let mut last = None;
    let mut count = 0;
    let annotation = node.child_by_field_name("type");
    for (visited, child) in node.named_children(&mut cursor).enumerate() {
        if visited >= 16 {
            return None;
        }
        if child.is_extra() || annotation == Some(child) {
            continue;
        }
        first.get_or_insert(child);
        last = Some(child);
        count += 1;
    }
    match kind {
        "parenthesized_expression" | "non_null_expression" if count == 1 => first,
        "as_expression" | "satisfies_expression" if count == 2 => first,
        "type_assertion" if count == 2 => last,
        _ => None,
    }
}

fn through_wrappers(mut node: Node<'_>) -> Node<'_> {
    for _ in 0..MAX_TRANSPARENT_WRAPPERS {
        let Some(parent) = node.parent() else { break };
        if transparent_value(parent) != Some(node) {
            break;
        }
        node = parent;
    }
    node
}

fn without_wrappers(mut node: Node<'_>) -> Node<'_> {
    for _ in 0..MAX_TRANSPARENT_WRAPPERS {
        let Some(value) = transparent_value(node) else {
            break;
        };
        node = value;
    }
    node
}

/// Find a directly assigned function's binding without resolving identifiers. Only direct
/// variable/object-field assignments and transparent syntax wrappers are recognized.
fn assigned_name<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    let node = through_wrappers(node);
    let parent = node.parent()?;
    match parent.kind() {
        "variable_declarator" | "public_field_definition" => (parent.child_by_field_name("value")
            == Some(node))
        .then(|| parent.child_by_field_name("name"))
        .flatten(),
        // JavaScript and TypeScript use different field names for the same binding:
        // JS field_definition.property versus TS public_field_definition.name.
        "field_definition" => (parent.child_by_field_name("value") == Some(node))
            .then(|| parent.child_by_field_name("property"))
            .flatten(),
        "pair" => (parent.child_by_field_name("value") == Some(node))
            .then(|| parent.child_by_field_name("key"))
            .flatten(),
        "let_declaration" => (parent.child_by_field_name("value") == Some(node))
            .then(|| parent.child_by_field_name("pattern"))
            .flatten(),
        _ => None,
    }
}

/// Ignore comment extras when identifying an unambiguous single binding.
fn single_declarator(declaration: Node<'_>) -> bool {
    let mut cursor = declaration.walk();
    let mut bindings = 0;
    // Comments are named extras. Bound this wrapper-only inspection independently so
    // a declaration with thousands of comments cannot make each function costly.
    for (visited, child) in declaration.named_children(&mut cursor).enumerate() {
        if visited >= 256 {
            return false;
        }
        if child.kind() == "variable_declarator" {
            bindings += 1;
            if bindings > 1 {
                return false;
            }
        } else if !child.is_extra() {
            return false;
        }
    }
    bindings == 1
}

/// Keep the declaration/export introducer in a multiline signature's source range.
fn declaration_node(node: Node<'_>) -> Node<'_> {
    let mut node = through_wrappers(node);
    if let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "variable_declarator"
                | "pair"
                | "field_definition"
                | "public_field_definition"
                | "let_declaration"
        ) && parent.child_by_field_name("value") == Some(node)
        {
            node = parent;
        }
    }
    if node.kind() == "variable_declarator" {
        if let Some(declaration) = node.parent() {
            if matches!(
                declaration.kind(),
                "lexical_declaration" | "variable_declaration"
            ) && single_declarator(declaration)
            {
                node = declaration;
            }
        }
    }
    if let Some(parent) = node.parent() {
        if parent.kind() == "export_statement"
            && (parent.child_by_field_name("declaration") == Some(node)
                || parent.child_by_field_name("value") == Some(node))
        {
            node = parent;
        }
    }
    node
}

fn scope(node: Node<'_>, source: &str) -> Option<Scope> {
    if !node.is_named() || node.is_error() || node.is_missing() || node.has_error() {
        return None;
    }
    let kind = match node.kind() {
        "function_item" | "function_signature_item" => {
            if node
                .parent()
                .and_then(|n| n.parent())
                .is_some_and(|n| matches!(n.kind(), "impl_item" | "trait_item"))
            {
                "method"
            } else {
                "function"
            }
        }
        "function_declaration"
        | "function_expression"
        | "function_signature"
        | "generator_function"
        | "generator_function_declaration" => "function",
        "arrow_function" => "arrow_function",
        "closure_expression" => "closure",
        "method_definition" | "method_signature" | "abstract_method_signature" => "method",
        "struct_item" => "struct",
        "enum_item" | "enum_declaration" => "enum",
        "trait_item" => "trait",
        "impl_item" => "impl",
        "mod_item" | "internal_module" => "module",
        "type_item" | "type_alias_declaration" => "type",
        "interface_declaration" => "interface",
        "class" | "class_declaration" | "abstract_class_declaration" => "class",
        "const_item" => "constant",
        "static_item" => "static",
        "variable_declarator" => {
            // The function/class node already covers a direct callable binding and
            // owns its inferred name. Do not emit a second identical variable scope.
            if node.child_by_field_name("value").is_some_and(|value| {
                matches!(
                    without_wrappers(value).kind(),
                    "arrow_function" | "function_expression" | "generator_function" | "class"
                )
            }) {
                return None;
            }
            "variable"
        }
        _ => return None,
    };
    let name_node = node
        .child_by_field_name("name")
        .or_else(|| {
            (node.kind() == "impl_item")
                .then(|| node.child_by_field_name("type"))
                .flatten()
        })
        .or_else(|| assigned_name(node));
    let name = name_node
        .and_then(|n| short_name(n, source))
        .map(String::from);
    let range = declaration_node(node);
    // A wrapper's syntax must be as trustworthy as the inner declaration's syntax.
    if range.has_error() {
        return None;
    }
    let header = node
        .child_by_field_name("body")
        .or_else(|| node.child_by_field_name("value"))
        .or_else(|| {
            (node.kind() == "type_item")
                .then(|| node.child_by_field_name("type"))
                .flatten()
        });
    Some(Scope {
        kind,
        name,
        start_line: range.start_position().row + 1,
        end_line: end_line(range),
        header_end_line: header
            .map_or_else(|| end_line(range), |body| body.start_position().row + 1)
            .min(end_line(range)),
        start_byte: range.start_byte(),
        end_byte: range.end_byte(),
    })
}

/// Parse exact UTF-8 source once. The cooperative deadline bounds parser/visitor work;
/// input, visited-node and retained-scope bounds are independent. Tree-sitter's transient
/// C allocations are not a hard allocator sandbox, and the tree is dropped before return.
pub fn analyze(path: &str, source: &str, time_budget: Duration) -> Analysis {
    let Some(grammar) = Grammar::for_path(path) else {
        return Analysis {
            language: None,
            status: Status::Unsupported,
            scopes: Vec::new(),
        };
    };
    let language = Some(grammar.name());
    let started = Instant::now();
    let budget = time_budget.min(MAX_PARSE_TIME);
    if source.len() > MAX_SOURCE_BYTES || budget.is_zero() {
        return Analysis::limited(language);
    }
    let mut parser = Parser::new();
    if parser.set_language(&grammar.language()).is_err() {
        return Analysis::limited(language);
    }
    let mut progress = |_: &tree_sitter::ParseState| {
        if started.elapsed() >= budget {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let tree = parser.parse_with_options(
        &mut |offset, _| source.as_bytes().get(offset..).unwrap_or_default(),
        None,
        Some(ParseOptions::new().progress_callback(&mut progress)),
    );
    let Some(tree) = tree else {
        return Analysis::limited(language);
    };
    let mut result = Analysis {
        language,
        status: if tree.root_node().has_error() {
            Status::Partial
        } else {
            Status::Parsed
        },
        scopes: Vec::new(),
    };
    let mut cursor = tree.walk();
    let mut visited = 0;
    let mut owned = 0;
    loop {
        visited += 1;
        if visited > MAX_VISITED_NODES || started.elapsed() >= budget {
            result.status = Status::Limited;
            break;
        }
        if let Some(scope) = scope(cursor.node(), source) {
            let name_bytes = scope.name.as_ref().map_or(0, String::capacity);
            let capacity = if result.scopes.len() == result.scopes.capacity() {
                result
                    .scopes
                    .capacity()
                    .max(4)
                    .saturating_mul(2)
                    .min(MAX_SCOPES)
            } else {
                result.scopes.capacity()
            };
            if result.scopes.len() == MAX_SCOPES
                || capacity * std::mem::size_of::<Scope>() + owned + name_bytes > MAX_RETAINED_BYTES
            {
                result.status = Status::Limited;
                break;
            }
            if capacity > result.scopes.capacity()
                && (result
                    .scopes
                    .try_reserve_exact(capacity - result.scopes.len())
                    .is_err()
                    || result.scopes.capacity() * std::mem::size_of::<Scope>() + owned + name_bytes
                        > MAX_RETAINED_BYTES)
            {
                result.status = Status::Limited;
                break;
            }
            owned += name_bytes;
            result.scopes.push(scope);
        }
        // Recovery nodes can contain declaration-shaped fragments with no trustworthy
        // boundary. Do not promote their descendants into source context.
        if !cursor.node().is_error() && !cursor.node().is_missing() && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return result;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    const TIME: Duration = Duration::from_millis(100);

    #[test]
    fn rust_scopes_have_exact_utf8_ranges_and_nested_methods() {
        let source = "// été\r\npub struct Worker {}\r\nimpl Worker {\r\n pub fn execute(\r\n  &self,\r\n ) -> usize {\r\n  42\r\n }\r\n}\r\n";
        let result = analyze("source.rs", source, TIME);
        assert_eq!(result.status, Status::Parsed);
        let method = result.enclosing(7).unwrap();
        assert_eq!(
            (method.kind, method.name.as_deref()),
            ("method", Some("execute"))
        );
        assert_eq!(
            (method.start_line, method.header_end_line, method.end_line),
            (4, 6, 8)
        );
        assert_eq!(
            &source[method.start_byte..method.end_byte],
            "pub fn execute(\r\n  &self,\r\n ) -> usize {\r\n  42\r\n }"
        );
        assert_eq!(result.enclosing(3).unwrap().kind, "impl");
    }

    #[test]
    fn javascript_arrow_and_object_methods_keep_multiline_introducers() {
        let source = "export const\nhandle = async (\n input\n) => {\nreturn input\n};\nconst object = {\n method() {\nreturn 'value'\n },\n arrow: () => {\nreturn 'arrow'\n }\n};\n";
        let result = analyze("source.mjs", source, TIME);
        assert_eq!(result.status, Status::Parsed);
        let arrow = result.enclosing(5).unwrap();
        assert_eq!(
            (arrow.kind, arrow.name.as_deref()),
            ("arrow_function", Some("handle"))
        );
        assert_eq!(
            (arrow.start_line, arrow.header_end_line, arrow.end_line),
            (1, 4, 6)
        );
        assert_eq!(result.enclosing(9).unwrap().name.as_deref(), Some("method"));
        assert_eq!(result.enclosing(12).unwrap().name.as_deref(), Some("arrow"));
    }

    #[test]
    fn declaration_comments_preserve_export_but_multiple_bindings_do_not_widen() {
        let source =
            "export const\n// declaration comment\nhandler = (input) => {\nreturn 'needle';\n};\n";
        let result = analyze("source.js", source, TIME);
        assert_eq!(result.status, Status::Parsed);
        let handler = result.enclosing(4).unwrap();
        assert_eq!(handler.name.as_deref(), Some("handler"));
        assert_eq!(
            (
                handler.start_line,
                handler.header_end_line,
                handler.end_line
            ),
            (1, 3, 5)
        );
        assert_eq!(
            &source[handler.start_byte..handler.end_byte],
            source.trim_end()
        );

        let source = "export const\n// first declaration\nfirst = () => {\nreturn 'first';\n},\n// second declaration\nsecond = () => {\nreturn 'second';\n};\n";
        let result = analyze("source.js", source, TIME);
        assert_eq!(result.status, Status::Parsed);
        assert_eq!(result.enclosing(4).unwrap().start_line, 3);
        assert_eq!(result.enclosing(8).unwrap().start_line, 7);
    }

    #[test]
    fn configuration_and_nonfunction_bindings_have_real_declaration_context() {
        let source = "export const schema = z.object({\n  retries: z\n    .array(z.number())\n    .max(2),\n})\n";
        let result = analyze("schema.ts", source, TIME);
        assert_eq!(result.status, Status::Parsed);
        let binding = result.enclosing(2).unwrap();
        assert_eq!(
            (binding.kind, binding.name.as_deref()),
            ("variable", Some("schema"))
        );
        assert_eq!(
            (
                binding.start_line,
                binding.header_end_line,
                binding.end_line
            ),
            (1, 1, 5)
        );
        assert_eq!(
            &source[binding.start_byte..binding.end_byte],
            source.trim_end()
        );

        for source in [
            "export const values = [\n 'needle'\n];\n",
            "export const config = {\n retries: 'needle'\n};\n",
            "export let value =\n 'needle';\n",
        ] {
            let result = analyze("config.js", source, TIME);
            assert_eq!(result.status, Status::Parsed);
            assert_eq!(result.enclosing(2).unwrap().kind, "variable");
            assert_eq!(result.enclosing(2).unwrap().start_line, 1);
        }

        let source = "export const\n// comment\nfirst = {\n value: 'needle'\n},\nsecond = [\n 'needle'\n];\n";
        let result = analyze("config.js", source, TIME);
        assert_eq!(result.status, Status::Parsed);
        assert_eq!(result.enclosing(4).unwrap().start_line, 3);
        assert_eq!(result.enclosing(7).unwrap().start_line, 6);

        let source = "export const handler = () => {\nreturn 'needle';\n};\nconst expression = function() {\nreturn 'needle';\n};\nconst ClassValue = class {\n method() { return 'needle'; }\n};\n";
        let result = analyze("config.js", source, TIME);
        assert_eq!(result.status, Status::Parsed);
        assert_eq!(result.enclosing(2).unwrap().kind, "arrow_function");
        assert_eq!(result.enclosing(5).unwrap().kind, "function");
        assert_eq!(result.enclosing(8).unwrap().kind, "method");
        assert!(!result.scopes.iter().any(|scope| scope.kind == "variable"));
    }

    #[test]
    fn typescript_and_tsx_use_their_own_grammars() {
        let source = "export interface Request {\n value: string\n}\nexport abstract class Handler {\n run<T>(value: T): T {\nreturn value\n }\n}\ntype Alias = {\n value: string\n};\n";
        let result = analyze("source.cts", source, TIME);
        assert_eq!(result.status, Status::Parsed);
        assert_eq!(result.enclosing(2).unwrap().kind, "interface");
        assert_eq!(result.enclosing(6).unwrap().name.as_deref(), Some("run"));
        assert_eq!(result.enclosing(10).unwrap().kind, "type");
        // The anonymous `class` keyword token has the same kind spelling as a class
        // expression, but must never become a second, fictitious declaration.
        assert_eq!(
            result.scopes.iter().filter(|s| s.kind == "class").count(),
            1
        );
        assert_eq!(
            result.enclosing(4).unwrap().name.as_deref(),
            Some("Handler")
        );
        let jsx = "export const View = (props: {text:string}) => (\n <section>{props.text}</section>\n);\n";
        let result = analyze("source.tsx", jsx, TIME);
        assert_eq!(result.status, Status::Parsed);
        assert_eq!(result.language, Some("tsx"));
        assert_eq!(result.enclosing(2).unwrap().name.as_deref(), Some("View"));
        assert_eq!(
            analyze("source.jsx", "function View(){ return <div/> }", TIME).status,
            Status::Parsed
        );
    }

    #[test]
    fn class_field_function_names_follow_javascript_and_typescript_grammars() {
        for (path, source) in [
            ("source.js", "class Service {\n handler = () => {\nreturn 'needle'\n };\n}\n"),
            ("source.ts", "class Service {\n readonly handler: () => string = () => {\nreturn 'needle'\n };\n}\n"),
        ] {
            let result = analyze(path, source, TIME);
            assert_eq!(result.status, Status::Parsed, "{path}");
            let arrow = result.enclosing(3).unwrap();
            assert_eq!((arrow.kind, arrow.name.as_deref()), ("arrow_function", Some("handler")), "{path}");
            assert_eq!((arrow.start_line, arrow.end_line), (2,4), "{path}");
            assert!(source[arrow.start_byte..arrow.end_byte].contains("handler"), "{path}");
        }
        let result = analyze(
            "source.js",
            "class Service {\n #privateHandler = function () {\nreturn 'needle'\n };\n}\n",
            TIME,
        );
        assert_eq!(result.status, Status::Parsed);
        assert_eq!(
            result.enclosing(3).unwrap().name.as_deref(),
            Some("#privateHandler")
        );
    }

    #[test]
    fn syntax_errors_and_sibling_definitions_do_not_invent_context() {
        let source = "function good() {\n return 1\n}\nfunction broken( {\n";
        let result = analyze("source.js", source, TIME);
        assert_eq!(result.status, Status::Partial);
        assert_eq!(result.enclosing(2).unwrap().name.as_deref(), Some("good"));
        assert!(result.enclosing(4).is_none());
        let result = analyze("source.js", "function a(){} function b(){}", TIME);
        assert_eq!(result.status, Status::Parsed);
        assert!(result.enclosing(1).is_none());
    }

    #[test]
    fn context_distinguishes_missing_declarations_ambiguity_and_unavailable_syntax() {
        let commonjs = analyze(
            "source.js",
            "module.exports = {\n schema: 'needle'\n};\n",
            TIME,
        );
        assert_eq!(commonjs.status, Status::Parsed);
        assert_eq!(commonjs.context(2), Context::Missing);
        assert!(commonjs.enclosing(2).is_none());

        let siblings = analyze("source.js", "function a(){} function b(){}", TIME);
        assert_eq!(siblings.context(1), Context::Ambiguous);
        assert!(siblings.enclosing(1).is_none());
        let partial = analyze("source.js", "function broken(", TIME);
        assert_eq!(partial.status, Status::Partial);
        assert_eq!(partial.context(1), Context::Missing);
        for status in [Status::Limited, Status::Unsupported, Status::InvalidUtf8] {
            let unavailable = Analysis {
                language: None,
                status,
                scopes: Vec::new(),
            };
            assert_eq!(unavailable.context(1), Context::Unavailable);
            assert!(unavailable.enclosing(1).is_none());
        }
    }

    #[test]
    fn anonymous_callbacks_keep_named_callable_ancestors_without_mutating_metadata() {
        for (path, source, line, expected) in [
            ("source.tsx", "export function Checkout() {\n useEffect(() => {\n  log('needle')\n });\n}\n", 3, "Checkout"),
            ("source.js", "export function startPayments() {\n const pending = queue.run(async (job) => {\n  log('needle')\n });\n}\n", 3, "startPayments"),
            ("source.ts", "class Service {\n execute() {\n  jobs.forEach(() => {\n   log('needle')\n  });\n }\n}\n", 4, "execute"),
            ("source.rs", "fn start_payments() {\n jobs.for_each(|job| {\n  log(\"needle\");\n });\n}\n", 3, "start_payments"),
        ] {
            let result = analyze(path, source, TIME);
            assert_eq!(result.status, Status::Parsed, "{path}");
            let before = result.scopes.clone();
            let Context::Found { inner, named_parent: Some(parent) } = result.context(line) else {
                panic!("missing named parent: {path}: {:?}", result.context(line));
            };
            assert!(inner.name.is_none(), "{path}");
            assert_eq!(parent.name.as_deref(), Some(expected), "{path}");
            assert!(strictly_contains(parent, inner));
            assert_eq!(result.enclosing(line), Some(inner));
            assert_eq!(result.scopes, before);
        }
    }

    #[test]
    fn anonymous_callbacks_retain_binding_or_class_when_no_named_callable_contains_them() {
        let source =
            "export const Card = forwardRef((props, ref) => {\n return <div>needle</div>;\n});\n";
        let result = analyze("source.tsx", source, TIME);
        let Context::Found {
            inner,
            named_parent: Some(parent),
        } = result.context(2)
        else {
            panic!("expected component binding: {:?}", result.context(2));
        };
        assert!(
            inner.name.is_none(),
            "a call must not become a direct assignment"
        );
        assert_eq!(
            (parent.kind, parent.name.as_deref()),
            ("variable", Some("Card"))
        );
        assert!(strictly_contains(parent, inner));

        let source =
            "class Service {\n static {\n  jobs.forEach(() => {\n   log('needle')\n  });\n }\n}\n";
        let result = analyze("source.js", source, TIME);
        assert_eq!(result.status, Status::Parsed);
        let Context::Found {
            named_parent: Some(parent),
            ..
        } = result.context(4)
        else {
            panic!("expected class context: {:?}", result.context(4));
        };
        assert_eq!(
            (parent.kind, parent.name.as_deref()),
            ("class", Some("Service"))
        );
    }

    #[test]
    fn transparent_callable_wrappers_preserve_exact_binding_name_and_declaration_range() {
        for (path, source) in [
            ("source.js", "export const investigate = (\n(value) => {\n return 'needle';\n}\n);\n"),
            ("source.ts", "export const investigate = (\n(value: string) => {\n return 'needle';\n}\n) satisfies (value: string) => string;\n"),
            ("source.ts", "export const investigate = (\n(value: string) => {\n return 'needle';\n}\n) as (value: string) => string;\n"),
            ("source.ts", "export const investigate = (\n(value: string) => {\n return 'needle';\n}\n)!;\n"),
            ("source.ts", "export const investigate = <(value: string) => string>(\n(value: string) => {\n return 'needle';\n}\n);\n"),
        ] {
            let result = analyze(path, source, TIME);
            assert_eq!(result.status, Status::Parsed, "{source}");
            let Context::Found { inner, named_parent } = result.context(3) else {
                panic!("expected wrapped callable: {source}");
            };
            assert_eq!((inner.kind, inner.name.as_deref()), ("arrow_function", Some("investigate")), "{source}");
            assert_eq!(inner.start_line, 1, "{source}");
            assert_eq!(&source[inner.start_byte..inner.end_byte], source.trim_end(), "{source}");
            assert!(named_parent.is_none());
            assert_eq!(result.scopes.len(), 1, "wrapped callable must not duplicate a variable scope: {source}");
        }
        let source =
            "fn outer() {\n let investigate = (\n  |value: &str| {\n   value\n  }\n );\n}\n";
        let result = analyze("source.rs", source, TIME);
        assert_eq!(result.status, Status::Parsed);
        let closure = result.enclosing(4).unwrap();
        assert_eq!(
            (closure.kind, closure.name.as_deref()),
            ("closure", Some("investigate"))
        );
        assert!(source[closure.start_byte..closure.end_byte].starts_with("let investigate"));
    }

    #[test]
    fn runtime_expression_wrappers_do_not_invent_callable_bindings() {
        for source in [
            "export const investigate = wrap(\n(value) => {\n return 'needle';\n}\n);\n",
            "export const investigate = (other,\n(value) => {\n return 'needle';\n}\n);\n",
        ] {
            let result = analyze("source.js", source, TIME);
            assert_eq!(result.status, Status::Parsed);
            let Context::Found {
                inner,
                named_parent: Some(parent),
            } = result.context(3)
            else {
                panic!("expected anonymous callable with nearby binding: {source}");
            };
            assert_eq!(inner.kind, "arrow_function");
            assert!(inner.name.is_none());
            assert_eq!(
                (parent.kind, parent.name.as_deref()),
                ("variable", Some("investigate"))
            );
        }
    }

    #[test]
    fn unsupported_limits_and_name_storage_are_explicit() {
        assert_eq!(
            analyze("source.py", "def f(): pass", TIME).status,
            Status::Unsupported
        );
        assert_eq!(
            analyze("source.rs", "fn f(){}", Duration::ZERO).status,
            Status::Limited
        );
        assert_eq!(
            analyze("source.rs", &" ".repeat(MAX_SOURCE_BYTES + 1), TIME).status,
            Status::Limited
        );
        let name = "a".repeat(MAX_NAME_BYTES + 1);
        let result = analyze("source.rs", &format!("fn {name}(){{}}"), TIME);
        assert_eq!(result.status, Status::Parsed);
        assert!(result.scopes[0].name.is_none());
        let many = (0..1500)
            .map(|i| format!("fn f{i}(){{}}\n"))
            .collect::<String>();
        let result = analyze("source.rs", &many, TIME);
        assert_eq!(result.status, Status::Limited);
        assert!(result.scopes.len() <= MAX_SCOPES);
        assert!(result.allocated_bytes() <= MAX_RETAINED_BYTES);
        assert!(result.enclosing(1).is_none());
    }

    #[test]
    fn deep_source_uses_iterative_traversal_and_cooperative_cancellation() {
        let source = format!(
            "function deep() {{ return {}1{}; }}",
            "(".repeat(3000),
            ")".repeat(3000)
        );
        let started = Instant::now();
        let result = analyze("source.js", &source, Duration::from_millis(1));
        assert!(matches!(result.status, Status::Parsed | Status::Limited));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(result.allocated_bytes() <= MAX_RETAINED_BYTES);
    }
}
