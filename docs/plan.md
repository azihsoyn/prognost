# prognost plan / assess

Two commands, the way `terraform plan` and a policy check are two steps:

- `prognost plan` says what a change does — which functions change, and
  what reaches them — and judges nothing.
- `prognost assess` assesses a plan's risks with rules, and does not
  analyse the code again: the plan is its input.

```
prognost plan --json | prognost assess - --fail-on high
```

Both are static analysis of two revisions: the same diff always gives the
same plan, and the same plan and rules the same assessment.

## plan

```
prognost plan [--base <rev>] [--head <rev>] [--json] [--hops N]
```

The diff is taken from the merge base of `--base` (default: the remote's
default branch) and `--head` (default: the working tree).

```
Changed symbols: 2  (~2 changed, +0 added, -0 removed)
  ~ createPool  packages/db-pool/src/index.ts:120  ← 21 fn / 5 pkg · public
  ~ createClient  apps/shop/database/src/client.ts:34  ← 15 fn / 3 pkg

Reach: 21 functions in 9 files / 5 packages, entry points: 7 export, 6 function
  @shop/database ← 6   @billing/database ← 5   …

Plan: 0 to add, 2 to change, 0 to remove; reaching 21 functions in 5 packages.
```

- **Changed symbols**: each function the diff added (`+`), changed (`~`)
  or removed (`-`), with how many functions and packages reach it.
  `public` marks a route handler, or an export another package imports.
- **Other changed files**: files the diff touches without changing a
  function — types, config, SQL, tests, markup.
- **Reach**: everything upstream together, per package, with the entry
  points (routes, components, exports nothing calls, module code).

`--json` prints a `PlanReport` (schema: `plan` in `prognost --schema`) in
two layers:

```jsonc
{
  "version": 3, "base": "<merge base>", "head": "<sha> | null",
  // facts
  "files":     [{ "path", "change": "added|modified|deleted|renamed", "previous_path"?, "package" }],
  "functions": [{ "id", "name", "path", "range": { "start", "end" }, "side": "head|base",
                  "change": "added|changed|removed|unchanged", "package", "exported",
                  "route"?, "entry"?: "route|component|module|export|function|unexplored" }],
  "calls":     [{ "caller", "callee", "line", "change": "added|removed|unchanged" }],
  // derived from the facts
  "summary": {
    "changed": { "added", "changed", "removed", "files_without_function_changes" },
    "reach":   { "functions", "files", "packages": [{ "package", "functions", "files" }], "entries": { "<kind>": n } },
    "symbols": [{ "id", "public", "called_from_packages", "reach_functions", "reach_files", "reach_packages", "reach_entries" }]
  },
  "truncated": false, "limits": { "hops", "nodes" }
}
```

`functions` holds the changed functions and everything upstream of them
(`change: "unchanged"`) in one shape. Every number in `summary` can be
recomputed from `functions` and `calls`: a function's reach is what
reaches it over the calls the diff keeps (`change` other than
`removed`), not counting other changed functions.

## assess

```
prognost assess [<plan.json> | -] [--sarif <file>] [--json] [--fail-on high|medium|low]
```

Reads the plan from a file, or from stdin with `-` (the default).

Applies the rules in force to the plan: the changed functions and their
reach as the plan recorded them, and the lines the diff added, read from
the plan's two revisions. `--sarif` adds another analyser's results that
sit on added lines. `--fail-on` exits 1 when a finding is at least that
severe, for CI. `--json` prints an `Assessment` (schema: `assess`).

Five rule presets are on by default:

| preset | rules |
|---|---|
| `graph` | `public-api-change` (low / medium / high as 1 / 2 / 3+ other packages import it; route handlers medium), `wide-reach` (20+ functions or 3+ packages upstream) |
| `typescript` | `await-in-loop` (medium; low for polls, stream reads and loops that stop early) |
| `go` | `defer-in-loop` (medium) |
| `python` | `python-await-in-loop` (medium; low for polls and loops that stop early) |
| `postgres` | `migration-drop`, `-rename`, `-not-null-without-default`, `-domain-rewrite`, `-alter-type` (high); `-set-not-null`, `-index-lock`, `-rls-policy` (medium); `-rls-disabled` (high) |

A repository chooses presets and adds, replaces or turns off rules in its
`prognost.toml`; `prognost rules` shows what is in force. See
[rules.md](rules.md).

## What a plan cannot do

Code calls itself, so one line can matter in thirty places; its
dependencies are inferred, so dynamic dispatch, DI and callbacks can hide
an edge; and there is no live state to compare with, only the previous
code. A plan says how far a change reaches — not that anything outside it
is safe.
