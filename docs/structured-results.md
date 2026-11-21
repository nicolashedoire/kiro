# Structured MCP results

Both tools return readable `content` and a machine-readable `structuredContent` object.
`tools/list` advertises each result's JSON `outputSchema`. The contract is identified by
`schema_version: 1`; it is independent of the package and negotiated MCP versions.
Existing text clients keep the readable output. `exact: true` affects only that display.

## Run

A request `{"command":"grep -n alpha a.txt"}` can return:

```json
{
  "schema_version": 1,
  "tool": "run",
  "status": "complete",
  "reason": null,
  "cwd": "/project",
  "commands": [{
    "command": "grep -n alpha a.txt",
    "status": "complete",
    "reason": null,
    "stdout": "1:alpha\n",
    "stderr": "",
    "exit_code": 0,
    "cwd_before": "/project",
    "cwd_after": "/project",
    "search": {
      "metadata_truncated": false,
      "records": [{
        "path": "a.txt",
        "line_number": 1,
        "kind": "match",
        "output_start": 0,
        "text_start": 2,
        "text_end": 7,
        "output_end": 8
      }]
    }
  }]
}
```

- `complete` means native execution was covered and returned; it does **not** mean exit
  code zero. Inspect `exit_code` and `stderr`. Grep with no selected lines returns 1.
- A refused command has `status: "refused"`, a `reason`, empty stdout/stderr and a null
  `exit_code`. Its directory is unchanged. It never falls back to a shell.
- Batch commands run in order. Top-level `partial` means some commands were refused;
  `refused` means all were refused. Both set MCP `isError: true`. Covered commands retain
  their effects on the session directory. This is not an atomic execution batch.
- Invalid arguments or an oversized response produce top-level `error` with a reason and
  no command results. No directory change from that call is committed.

Search metadata is emitted by grep during execution, not inferred from its displayed
text. Line numbers are one-based. `match` denotes a selected line (also with `-v` or
`-o`); `context` denotes a surrounding line. `path` is the source spelling relative to
`cwd_after`, or an absolute path; null denotes pipeline input, not a source file reference.
Positions are **zero-based byte offsets**, with exclusive ends, into the command's original
UTF-8 `stdout`. `text_start..text_end` excludes prefixes/newline; `output_start..output_end`
includes the full emitted line. Context separators are gaps between records. Offsets are
not character indices or file offsets, and are unaffected by grouped presentation.

`search` is null for counts, filename-only/quiet/binary output, non-grep final stages,
shell-style command lists and output transforms. Such stdout is still returned intact.
A single pipeline ending in line-oriented grep can expose metadata even without `-n` or
printed filenames. Each search retains at most 4,096 records and 256 KiB of metadata
allocations. `metadata_truncated: true` signals omitted records; stdout is never shortened
to fit the metadata budget. Grouping is disabled for truncated metadata.

## Scout

The structured result includes `schema_version`, `tool`, `status`, `reason`, `cwd`,
normalized `terms`, `counts`, `matches`, `metadata_truncated` and `display_truncated`.

| Field | Meaning |
| --- | --- |
| `status: complete` | Eligible text scan finished, including a negative search. |
| `status: partial` | Discovery/input/retention limit or file read errors left an incomplete scan. |
| `status: error` | Invalid input or discovery failed before scanning. |
| `reason` | Explanation independent of the display budget; null for a complete scan. |
| `counts` | Discovered candidates, text files read, skipped files, read errors, matching files/lines. |
| `matches` | Direct source references: `path`, one-based `line_number`, and zero-based `term_indices` into `terms`. |
| `metadata_truncated` | Some direct references were omitted by the independent metadata bound. |
| `display_truncated` | The generated text exceeded the requested byte budget and was clipped. |

Counts and references come from scanning, not parsing the summary. Counts cover the files
actually searched even when references or display excerpts are omitted. Metadata retains
at most 4,096 references and 256 KiB of allocations. A metadata limit alone does not stop
the scan or make its status partial. Paths are relative to `cwd` unless absolute.

`complete` covers the documented eligible text scope, excluding ignored/binary/oversized
files. It is not a claim of semantic completeness. Even untruncated text selects excerpts
and may clip long lines; neither truncation flag means every matching line was displayed.
With `budget: 0`, text is empty while status, reason, counts and bounded references remain.
Partial/error results set MCP `isError: true`.

## Bounds and compatibility

Run readable text is bounded to 2 MiB. Encoded readable text plus structured data are
bounded to 8 MiB, checked during batch accumulation and again before committing directory
state. JSON framing adds a small fixed envelope. Native file/input/output bounds still
apply. Non-UTF-8 stdout is refused by this text transport.

The CLI's stdout/stderr/exit behavior is unchanged. Rust callers can use
`engine::Reply.search`, `grep::Output.search` and `scout::scout_report()` directly.
`scout()` and `scout_result()` remain compatibility adapters. The old text-only grouping
helper has been replaced by metadata-driven rendering; no filesystem prefix cache is used.
