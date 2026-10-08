# Configuration

prognost works without configuration. A repository can add a
`prognost.toml` at its root — or any file, named by `PROGNOST_CONFIG`, for a
repository that shouldn't carry one — with three kinds of tables.

## Seams: calls joined by a string

An import says who calls whom; some calls don't go through one. An event
published here and handled there, a queue, a job name, a DI token — both
sides carry the same string. A seam is a pair of regexes: one pulls the key
out of a call site, one out of a definition. Every call site whose key
matches a definition's becomes an edge from the function around the call to
the function around the definition.

```toml
[[seam]]
name = "domain events"
call = '''publish\(\s*['"]([^'"]+)['"]'''
definition = '''subscribe\(\s*['"]([^'"]+)['"]'''
```

Capture groups form the key (joined with a space when there are several).
In `/`-separated keys a `:param`, `{param}` or `[param]` segment matches any
other. TOML's `'''…'''` strings take the regex as written.

Hono's typed RPC client (`client.api.v1.orders.$post(…)` → `.post('/', …)`
under `.route('/api/v1/orders', …)`) is built in.

## Risk rules

`presets`, `disable`, `[[risk]]` and `[[set]]` choose and write the rules
`prognost assess` applies. See [rules.md](rules.md).

## Environment

| variable | effect |
|---|---|
| `PROGNOST_CONFIG` | the configuration file to read instead of `./prognost.toml` |
| `NO_COLOR` | no colour unless `--color always` |
| `CLICOLOR_FORCE` | colour even when piped, unless `--color never` |
| `VISUAL`, `EDITOR` | the editor `o` opens in the graph (default `vi`) |
| `HERDR_ENV` | set by herdr; panes open beside the caller (see [graph.md](graph.md)) |
| `PROGNOST_DEBUG` | a file to append a trace of the analysis to |

## What it analyses

- TypeScript and JavaScript (`.ts`, `.tsx`, `.js`, `.jsx`, `.mts`, `.cts`,
  `.mjs`, `.cjs`) and the `<script>` blocks of Svelte components.
- Workspaces declared by `pnpm-workspace.yaml` or `package.json`
  `workspaces`; imports through relative paths, package names, barrels
  (`export * from`) and tsconfig `paths`.
- HTTP calls made through Hono's typed client, and the seams above.

Everything else in a diff — other languages, SQL, config — still appears in
`plan` as a changed file, and SQL migrations are read by the PostgreSQL
rules, but no calls are followed through it. The call graph is inferred:
dynamic dispatch, dependency injection and callbacks handed through
untyped values can hide an edge, so reach is a lower bound.

## Caches

Each commit's tree is extracted once into `$TMPDIR/prognost-cache/<sha>`
and reused on later runs. It is safe to delete.
