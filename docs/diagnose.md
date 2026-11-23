# Diagnose: bounded source evidence for an issue

`diagnose` is an experimental, deterministic retrieval pipeline. It takes an error,
failed-test output, stack trace or issue description and returns ranked files with
source excerpts. It does not run a model, execute project code, repair a bug or establish
the cause of a failure.

```sh
kiro diagnose --project /path/to/project --report /path/to/failure.log
kiro diagnose --project /path/to/project --message 'MissingWidget at src/service.js:42'
kiro diagnose --report - --json < failure.log
```

Additional positional arguments add explicit search terms. `--path` restricts the search
to a project-relative file or directory; repeat it for several paths. `--max-files` selects
1–16 files (default 8). `--budget` controls readable text in UTF-8 bytes (default 12,000,
maximum 65,536); structured evidence has its own bounds. `--json` prints that structured
result. Keep report files outside the source tree if they should not be search candidates.

The pipeline performs local steps without returning to the model between them:

1. Extract bounded identifiers, phrases and source-location anchors from the report.
2. Discover files using Scout's existing file-discovery policy, then exclude symlinks,
   non-regular files and paths outside the canonical project root.
3. Search those exact file names with Scout's lexical scanner. Files are not parsed just
   to obtain this first ranking.
4. Combine trace, lexical and path-name rankings using reciprocal rank fusion, with
   `k = 60`. The score is a retrieval signal, not a probability that the file caused the bug.
5. Read bounded windows from the selected files, adding declaration headers where useful.
   Rust, JavaScript/JSX and TypeScript/TSX can use Tree-sitter; other files retain an explicit
   lexical window. Definitions are syntactic ranges, not resolved calls or references.

All results identify the search terms, ranking signals and one-based inclusive source line
ranges. Excerpt text is display text: long lines can be clipped and line endings normalized;
`text_clipped` and `evidence_truncated` identify omissions. Use `run` or a native reader when
the original full contents are needed.

## Completion and limits

The structured result uses `schema_version: 1`, `tool: "diagnose"` and an advertised output
schema. `complete` means the eligible search for the selected terms finished; it does not
mean semantic coverage, a proven diagnosis or a validated fix. Read/resource failures can
produce `partial`; invalid input or discovery failure produces `error`. CLI exit status is
0 for complete and 1 otherwise; invalid CLI syntax/input files use 2. MCP sets `isError`
for a non-complete report.

`terms_truncated`, `selection_truncated`, `metadata_truncated`, `display_truncated` and
per-candidate flags remain separate from search completion. A zero text budget produces
empty readable text while retaining bounded structured status and evidence. No-match
searches can finish successfully with no candidates.

Reports are limited to 64 KiB, explicit terms to 24 (160 UTF-8 bytes each), files to
400,000 bytes and scan input to 16 MiB. The selected file count is at most 16. Discovery,
candidate retention, metadata, excerpts and cooperative analysis time are bounded; the
result's `limits` and omission flags describe them. The scanner shares Scout's documented
exclusions, including generated and dependency directories. Source must stay stable during
the call. Static path checks do not provide an OS sandbox or eliminate filesystem races.

## MCP

The default server exposes `run`, `scout` and `diagnose`. Clients may request only a subset:

```sh
kiro mcp --project /path/to/project --tools run,scout
kiro mcp --project /path/to/project --tools run,scout,diagnose
```

Disabled tools are neither advertised nor callable. A request to `diagnose` has this shape:

```json
{
  "name": "diagnose",
  "arguments": {
    "report": "MissingWidget at src/service.js:42",
    "terms": ["validateInput"],
    "budget": 12000,
    "max_files": 8
  }
}
```

The implementation is a bounded pipeline of fixed operations, not an interpreter for
arbitrary code or plans supplied by a model. The local Qwen specialist is not connected.
Reading source, diagnosing a failure and deciding which action to take remain distinct.

## Evidence and evaluation

The design draws on [reciprocal rank fusion](https://cormack.uwaterloo.ca/cormacksigir09-rrf.pdf),
the structured-context approach evaluated in
[Agentless](https://lingming.cs.illinois.edu/publications/fse2025.pdf), and local composition
of operations studied by [CodeAct](https://arxiv.org/abs/2402.01030).
Those publications do not establish a speed or quality gain for Kiro. A useful comparison
must include native reading/search tools, scripts and batching, the existing Scout tool,
the complete session time and an assessment separate from file-location mentions.
