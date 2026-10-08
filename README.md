<p align="center">
  <img src="assets/logo.svg" width="112" alt="prognost logo: a change at the bottom, its callers lit up above it">
</p>

<h1 align="center">prognost</h1>

<p align="center">
  <b>A prognosis for a code change.</b><br>
  Which functions a diff touches, how far that reaches through the code that calls them,<br>
  and what in that reach looks risky — before it ships, from static analysis alone.
</p>

<p align="center">
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-blue" alt="License: MIT or Apache-2.0"></a>
  <img src="https://img.shields.io/badge/status-early-orange" alt="Status: early">
  <img src="https://img.shields.io/badge/analyses-TypeScript-3178c6" alt="Analyses TypeScript">
</p>

<p align="center">
  <img src="docs/demo/demo.gif" alt="prognost plan, assess and graph on a small demo monorepo: a change to a shared connection-pool library, followed up through the database, the API route and the web page that call it">
</p>

A line diff shows what changed. It doesn't show where the change lands: the
route three packages away that now waits on a new drain loop, the worker
that started querying once per invoice, the migration that rewrites a busy
table. prognost reads both revisions of a TypeScript codebase, works out
which functions really changed, follows their callers up to the entry
points, and lets you walk that — in the terminal, in a browser, or as JSON
for a CI step or an agent.

## Quick start

```sh
cargo install --git https://github.com/azihsoyn/prognost

cd your-repo
prognost plan                               # what changes, and how far it reaches
prognost plan --json | prognost assess -    # what in that looks risky
prognost graph                              # walk it in the terminal
```

Every command diffs from the merge base of `--base` (default: the remote's
default branch) and `--head` (default: the working tree), like
`git diff base...head`. The base is read straight from git's object store;
nothing is checked out.

To try it on the code in the demo above:

```sh
docs/demo/make-demo-repo.sh /tmp/prognost-demo
cd /tmp/prognost-demo && prognost graph --base main
```

## plan: what changes, and how far it reaches

```
$ prognost plan --base main
Changed symbols: 5  (~3 changed, +2 added, -0 removed)
  ~ createPool  packages/db-pool/src/index.ts:19  ← 9 fn / 5 pkg · public
  + drainWaiters  packages/db-pool/src/index.ts:7  ← 9 fn / 6 pkg
  ~ resetPool  packages/db-pool/src/index.ts:14  ← 9 fn / 6 pkg
  + sleep  packages/db-pool/src/index.ts:5  ← 9 fn / 6 pkg
  ~ runInvoices  apps/billing/worker/src/main.ts:5

Other changed files: 1
  + apps/shop/database/migrations/0002_order_note.sql

Reach: 9 functions in 6 files / 5 packages, entry points: 4 export, 1 module, 2 route
  @shop/database ← 3   @shop/api ← 2   @shop/web ← 2   @admin/reports ← 1   @billing/worker ← 1

Plan: 2 to add, 3 to change, 0 to remove (3 files); reaching 9 functions in 5 packages.
```

A plan states facts and judges nothing. `--json` gives the facts — every
file, every function (changed and upstream), every call between them with
its line — and a summary derived from them. See [docs/plan.md](docs/plan.md).

## assess: what in it looks risky

```
$ prognost plan --base main --json | prognost assess -
  ⚠ high   migration-domain-rewrite           adds column gift_message of domain type nonempty_text, which has a CHECK:
                                              PostgreSQL rewrites the whole table under an exclusive lock
  ⚠ high   migration-not-null-without-default adds NOT NULL column note without a DEFAULT
  ⚠ high   public-api-change                  createPool changes behaviour; imported by 3 other packages
  ⚠ medium await-in-loop                      new await inside a loop in runInvoices: one round trip per iteration
  · low    await-in-loop                      new wait inside a loop in drainWaiters: polls until a condition holds
  · low    wide-reach                         resetPool is reached from 9 functions in 6 packages

Assessment: 3 high, 1 medium, 2 low.
```

`plan` and `assess` are two steps on purpose, the way `terraform plan` and a
policy check are. Rules are data: presets for the call graph, TypeScript
and PostgreSQL migrations ship built in, and a repository adds, replaces or
turns off rules in its `prognost.toml`. `--sarif` folds in other analysers'
results on the lines the diff added, and `--fail-on high` makes it a CI
gate. See [docs/rules.md](docs/rules.md).

## graph: walk it

`prognost graph` draws the changed functions grouped by package, with their
callers and callees. Move with the arrow keys or `hjkl`: `←` steps to a
caller (finding more as you go), `→` to a callee, `Enter` opens the diff of
the selected function, `z`/`Z` folds a package into files or directories,
`v` marks a file seen.

`prognost graph --html page.html` writes the same graph as a single web page
— click a function to light up every chain through it, double-click to read
its diff — and `prognost serve` serves it with seen marks kept in sync with
GitHub's *Viewed* checkboxes.

<p align="center">
  <img src="docs/demo/web.png" width="900" alt="The web page: packages as cards, changed functions highlighted, curves for the calls between them">
</p>

## How it works

1. tree-sitter turns every function-like construct in both revisions into a
   node — named declarations and anonymous callbacks alike.
2. An aligner matches base nodes to head nodes, most confident signal
   first: exact name, then identical normalized body, then position under
   an already-matched parent. A function that only moved lines is the same
   node; one whose body changed is *changed*.
3. Calls are resolved across files through imports, barrels, workspace
   packages and tsconfig paths, and across the HTTP boundary through Hono's
   typed client. A repository adds its own string-keyed seams (an event
   name, a queue) in `prognost.toml`.
4. From each changed function, callers are followed up to the entry points:
   routes, components, module code, exports nothing calls.

It covers TypeScript and JavaScript (including the scripts of Svelte
components) in pnpm-style workspaces today. The graph is inferred: dynamic
dispatch, dependency injection and callbacks can hide an edge, so reach is
a lower bound, never a proof that something else is safe.

## Why not an LLM

Node identity, alignment, call resolution and every rule are deterministic —
the same diff always gives the same plan, and the same plan the same
assessment. That is what makes the output something to diff, cache, gate a
merge on, or hand to an agent as ground truth.

## Commands

```sh
prognost graph [--html <file>]       # the diff as a graph: terminal, or a web page
prognost serve [host:port]           # the web page over HTTP, synced with GitHub's Viewed
prognost plan [--json]               # what changes and how far it reaches
prognost assess [<plan.json> | -]    # the plan's risks, by rules; --sarif, --fail-on
prognost rules                       # the rules in force here
prognost <file>[:<symbol>]           # one file: its functions aligned across the revisions
```

Output is coloured on a terminal and plain when piped. `--color
auto|always|never` (or `--no-color`) chooses explicitly; `NO_COLOR` and
`CLICOLOR_FORCE` are honoured. With colour off, the graph marks changes by
underline and the selection by reverse video.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
