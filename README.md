# prognost

A prognosis for a code change: which functions it touches, how far that
reaches through the code that calls them, and what in that reach looks
risky — read before it ships, from static analysis alone.

Review how execution changed, not which lines changed. prognost parses
both revisions of a TypeScript codebase, aligns the functions that are
still the same thing even when they moved or shifted lines, follows the
calls from every changed function up to the entry points, and draws it as
a graph you can walk — in the terminal or as a web page.

The core, for one file:

```
prognost packages/db-pool/src/index.ts:resetPool \
  --base main --head fix/pool-reset-waiters

  matched   resetPool  [Exact]  base:40-62  head:48-73
      + call added:   drainWaiters
  matched   connectOrFail  [Exact]  base:90-118  head:101-131
      - call removed: resetPool
  matched   noteError  [Exact]  base:80-84  head:91-95
  matched   anon@66  [BodyHash]  base:66-78  head:77-89
  added     drainWaiters  head:36-46
```

An illustrative example: a connection-pool fix. A handful of scattered
`resetPoolLater` call sites — an event handler, a wrapped query, a
`release` override, none of them sharing a name — align to the same nodes
across the revision, the one direct `resetPool` call that got replaced by
a `throw` shows up as removed from `connectOrFail` specifically, and the
new draining logic shows up as an added node and an added call, not a wall
of diff text to read line by line.

## Commands

```sh
prognost graph                       # the diff as a graph in the terminal: changed functions, callers, callees
prognost graph --html page.html      # the same graph as a self-contained web page
prognost serve                       # the page over HTTP, with seen marks synced to GitHub's Viewed
prognost plan [--json]               # what changes and how far it reaches, before it ships
prognost plan --json | prognost assess -   # the plan's risks, by rules (see docs/rules.md)
prognost rules                       # the rules in force
prognost <file>[:<symbol>]           # one file: its functions aligned across the two revisions
prognost map                         # the older file-level dependency map
```

Every command diffs from the merge base of `--base` (default: the remote's
default branch) and `--head` (default: the working tree), like
`git diff base...head`. Base is read straight from git's object store;
nothing is checked out and the working tree is never touched.

`plan` and `assess` are two steps on purpose, the way `terraform plan` and
a policy check are: the plan states facts, the assessment judges them.
See [docs/plan.md](docs/plan.md).

## How it works

1. tree-sitter turns every function-like construct into a node — named
   declarations and anonymous callbacks alike.
2. The aligner matches BASE nodes to HEAD nodes, most confident signal
   first: exact name, then identical normalized body (the
   anonymous-callback case), then position under an already-matched parent.
3. Calls are resolved across files through imports, barrels, workspace
   packages and tsconfig paths, and across the HTTP boundary through Hono's
   typed client; repositories add their own string-keyed seams in
   `prognost.toml`.
4. From each changed function, callers are followed up to the entry points
   (routes, components, exports nothing calls).

## Why not an LLM

Node identity, control-flow extraction, alignment, and diff classification
are all deterministic — the same input always produces the same graph. An
LLM could eventually narrate what changed; it should never be the thing
deciding whether two nodes are the same node.

## Why not CodeSee's shape

The dead end this design keeps checking against: an automatically generated
map of everything looks impressive once and is opened once. Everything
here starts from a single function a reviewer chose, not the repository.
