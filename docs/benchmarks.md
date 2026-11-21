# Benchmarks

The runners distinguish native command compatibility, MCP presentation and live agent
behaviour. They do not establish a universal speedup or a semantic correctness score.

## Reproduce the public command fixture

This fixture is embedded in [`examples/replay.rs`](../examples/replay.rs). It creates a
small temporary TypeScript project and notes file, replays 15 explicit reading/searching
commands, and removes the temporary files afterwards. It needs no private sessions, Claude
installation or model calls.

```sh
cargo run --locked --release --example replay -- --fixture

# Include the persistent MCP transport in the measurement.
cargo build --locked --release
cargo run --locked --release --example replay -- --fixture --mcp target/release/kiro
```

The command exits unsuccessfully if any sampled fixture command differs, is refused or
cannot be replayed. Times are measured locally, not copied from a published speedup table.
This is a reproducible smoke benchmark, not a representative agent workload.

### What the current replay checks

- Every branch and pipeline is checked against a small program/syntax allowlist, including
  branches that a successful earlier command would skip. Custom executable paths, globs,
  expansions, writers and unknown filters are excluded. `sed -n` is accepted only with
  numeric printing scripts; it does not make a `w` or `e` script safe.
- The native engine must also accept the whole command **before either reference shell
  run**, including warm-up. Refused commands are counted but never handed to the shell.
  Therefore the accepted subset cannot estimate coverage of all historical shell calls.
- The reference is `/bin/sh` with `/usr/bin:/bin` as PATH and a cleared environment, except
  HOME and locale settings. User shell startup hooks, shell functions and PATH wrappers are
  not part of this measurement. File caches are warmed before the timed executions.
- Native comparison requires identical stdout bytes, stderr bytes **and exit status**.
  Different ordering, missing final newlines and different errors are mismatches. Every
  mismatch is excluded from timing totals and causes an unsuccessful exit.
- MCP mode additionally compares the **formatted UTF-8 response** exactly, including its
  exit marker. MCP combines stdout and stderr into a text result; this is not independent
  proof of byte-stream separation or binary compatibility. Non-UTF-8 output cannot pass
  this transport comparison. Native byte/status comparison is still required.
- Output capture and execution time are bounded. The former RTK rewrite/execution mode was
  removed: externally rewritten commands are outside this runner's native validation.

The replay is a conservative command filter, **not an OS sandbox**. Projects, files and
executables must remain trusted and unchanged during a run. Use a disposable checkout for
recorded sessions. The fixture itself writes only its own temporary input files; the
replayed commands cannot intentionally write project files.

### Recorded commands

Recorded session input is explicit; the runner does not scan private session history by
default. A project filter restricts recorded working directories by path components.

```sh
cargo run --locked --release --example replay -- \
  --sessions /path/to/session-jsonl-directory --project /absolute/path/to/disposable-project \
  --max 600 --examples 5

cargo run --locked --release --example replay -- \
  --sessions /path/to/session-jsonl-directory --project /absolute/path/to/disposable-project \
  --mcp target/release/kiro --max 600
```

A refused command has no reference timing. The runner does not replay unknown commands in
order to measure fallback coverage. Public fixture results should be reported separately
from results on private recorded sessions.

## Live sessions: what is measured

[`examples/live.rs`](../examples/live.rs) runs a task with Claude Code's regular tools and
with the same tools plus kiro MCP. Setup order is counterbalanced between repetitions:
regular/engine on odd repetitions, engine/regular on even repetitions. It is not blinded.
For a balanced experiment use an even number of repetitions, several tasks and repositories,
and include a baseline already using batched searches and native file-reading tools.

The runner reports time, tokens, model turns, cost and tool usage. Its
`expected_paths_mentioned` metric checks that each expected substring (or a `|`-separated
alternative) occurs in the final answer. It does **not** verify reasoning, semantic
correctness, false positives or that a suggested edit would work. Keep the individual JSONL
traces and `results.json`, and review answers manually before drawing quality conclusions.

Edit/Write/NotebookEdit/Agent/Task tools are disabled, but **Bash remains available**. The
runner does not mount the project read-only and cannot guarantee that the agent will not
write through Bash. Use a disposable checkout or apply an external read-only filesystem
policy. The output directory intentionally receives configs, session traces and results.

The example spends real model calls, up to the configured half-dollar cap per session. It
is not part of the public fixture, automated tests or a normal build.

```sh
# This command spends real model calls. Use a disposable checkout.
cargo run --locked --release --example live -- \
  /path/to/disposable-project target/release/kiro /path/to/results 6 \
  "src/file-a.ts,src/file-b.ts" < task.txt
```

## Historical measurements: exploratory, not current release evidence

Before the audit, one Apple M4 Pro running macOS and Claude Code 2.1 was used on a private
TypeScript monorepo of about 1,100 tracked source files. Those runs suggested that reducing
model round trips mattered more than removing a few milliseconds of shell startup.

The previous report listed these aggregates:

| Historical metric | Regular terminal | With kiro |
| --- | ---: | ---: |
| Live mean model turns, 15 sessions per setup | 14.5 | 5.4 |
| Live mean time | 164 s | 48 s |
| Live mean tokens | 308,500 | 146,400 |
| Live mean cost | $0.26 | $0.14 |
| Answers mentioning all expected locations | 11 / 15 | 13 / 15 |

These are retained as historical observations, **not independently reproduced or a claim
about the corrected release**. The private project, full task set and individual traces
are not published here. There was one model, three search-only tasks, five sessions per
cell, and setup order was always regular then engine. Task A used two batches run a few
hours apart. The path-substring metric did not measure answer correctness.

The former replay's claimed 320 identical answers, 37x median speedup and process counts
are not used as validated results: its comparator allowed reordered output and normalized
some error newlines, did not exclude every exit-code mismatch, and passed potentially
writing commands through its safety filter. Its grep compatibility probe could also start
a process despite the reported zero-process count. The previous RTK table and coverage
percentages are therefore not a baseline for the current runner.

A future performance claim needs public input data, the exact commit and model identifiers,
per-run traces, distributions rather than just means, an equivalent optimized baseline,
and a correctness assessment separate from path mentions. No paid live experiment was
rerun as part of these corrections.
