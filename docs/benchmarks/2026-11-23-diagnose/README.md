# Diagnose prototype — real AI comparison, 23 November 2026

This experiment compares native tools, the existing Scout configuration and the new
experimental diagnose pipeline. It uses 30 distinct public issues from 14 repositories,
14 Rust and 16 JavaScript/TypeScript cases, selected before model measurements.

<!-- OBSERVED_RESULTS -->

## What is measured

Each case is investigated once in each of three fresh Claude Code sessions (90 planned
sessions), with primary model `claude-sonnet-4-6`, effort medium. Native Bash, Read, Grep
and Glob are available in every configuration; batching, scripts and filtering are allowed.
Scout adds Kiro MCP run/scout. Diagnose adds the same tools plus diagnose. The same binary
serves both MCP conditions with an explicit tool allowlist; disabled tools are not advertised
and cannot be called. Tool availability is checked at initialization and uses are recorded.

The experiment measures full-process wall time, including CLI/MCP startup, model requests,
tools and final output. CLI/API-reported durations are retained separately. Missing counters
remain null. Reported cost is not an invoice; tokens include the CLI's reported cache counters.
All started sessions remain in the data, including slow, unsuccessful and malformed answers.
No session is selectively rerun. The per-session limits are 480 seconds and $3 of reported
model cost, with at most four sessions running concurrently.

Task order is deterministically shuffled. The configuration order rotates across cases,
with ten cases in each starting position. Conversations are fresh, CLI setting sources
are disabled and unrelated tools, agents and Chrome are unavailable. MCP startup is eager.
Filesystem caches are shared/warm; provider prompt caching is enabled. Background activity
on this single Mac is not fully controlled. These are paired descriptive observations,
with one repetition per case/configuration, not a statistical significance claim.

## Source data and isolation

The cases come from [SWE-bench Multilingual](https://www.swebench.com/multilingual.html),
with [the dataset card](https://huggingface.co/datasets/SWE-bench/SWE-bench_Multilingual)
reporting an MIT license. The pinned task repository and archive hash are recorded in
[selection-protocol.json](selection-protocol.json). The original dataset tests repairs;
this experiment uses its issue descriptions and pre-fix source for read-only diagnosis.
Some selected issues request changes to API behavior, so these are 30 investigation cases,
not a claim that every case is a conventional bug.

Each configuration receives the same pinned production-only source projection for a case:
Rust/JavaScript/TypeScript files and package metadata, excluding separate test, documentation,
fixture, dependency and build trees. Original source bytes, comments and inline tests remain.
No projects or dependency installers are executed. The 30 projections contain 15,287 files
and 94,007,369 source bytes in total; [the manifest](manifest.json) records every file and
source commit. The largest files may encounter the tools' normal exclusions or resource
bounds; no projection was trimmed based on prototype results.

OS policy denies project writes, reads of reference patches/other answers, and reads of
other case projections. The policy also protects reference data and other projections
from blind writes. Each session can read its own MCP configuration. Before/after fingerprints
cover relative file names, kinds, permission modes and contents. Raw traces remain private
because CLI initialization contains local machine/account context. Published answers replace
project/home paths with placeholders.

The [selection protocol](selection-protocol.json), [case IDs](selected-case-ids.json),
[prompts](tasks.json) and [qualitative rubric](review-rubric.json) were fixed before the
measured campaign. Reference patches, added tests, hints and FAIL_TO_PASS identifiers are
not sent to the model. Gold annotations are OS-denied during the runs and may be published
with the completed results for inspection. Public benchmark material may overlap with model
training data; this experiment cannot eliminate that possibility.

## What the quality metrics mean

The model returns ranked production files, source line ranges of at most 40 lines and an
explanation. Top-5 uses the first five distinct paths. File-hit rate means at least one of
those paths is a production file modified by the reference patch; file recall measures the
share of reference files recovered. Region overlap means a narrow, valid pre-fix range
intersects a reference region in a Top-5 file. Invalid, missing or out-of-file ranges receive
no region credit. Failed/unparseable sessions remain in the denominators.

Reference locations are an imperfect proxy: alternative diagnoses can be valid, and a
correct path or overlapping range does not establish a correct explanation. Pure insertions
use explicit nearby pre-fix anchor lines. The preparation checks validate 107 reference
regions, including 29 insertion contexts, against the pre-fix sources.

A separate AI-assisted source review evaluates all explanations using the predeclared rubric.
Review inputs hide configuration names; self-references in answers may reveal them. Labels
are qualitative, not human-certified accuracy scores. No external project test suite was
run and no fix was validated. These results must not be presented as SWE-bench resolution
rates, causal proof, or production repair reliability.

## Artifacts and reproduction

- [Runner](run_sessions.py), [pure helper checks](verify_helpers.py)
- [Dataset preparer](prepare_dataset.py), [preparation checks](preparation-checks.json)
- [Chart renderer](render_charts.py), [plot dependencies](requirements.txt)
- [Prototype usage, contracts and limits](../../diagnose.md)

Build the exact source commit recorded in protocol.json, then run prepare_dataset.py in a
new disposable directory. Keep its private reference data outside the model-visible source
roots. The preparer downloads pinned upstream sources; it never runs project code.

<!-- REPRODUCTION_COMMANDS -->

The earlier [30-session Scout experiment](../2026-11-23-sonnet46/README.md) uses three tasks
and a Bash-only baseline, while this experiment uses 30 external cases and native reading
and search tools. The campaigns are separate; their aggregate percentages do not isolate
a version-to-version speed or quality change.
