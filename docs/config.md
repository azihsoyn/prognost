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

| | | |
|---|---|---|
| **Languages** | TypeScript, JavaScript (`.ts` `.tsx` `.js` `.jsx` `.mts` `.cts` `.mjs` `.cjs`) | ✅ functions, calls, imports |
| | Svelte components (`.svelte`) | ✅ the `<script>` blocks; markup is not parsed |
| | Go (`.go`) | ✅ functions, methods, function literals, calls, imports |
| | Python (`.py`) | ✅ functions, methods, lambdas, calls, imports |
| | Vue single-file components, Angular templates | ❌ the files appear in `plan`, their calls are not followed |
| | Any other language | ❌ the files appear in `plan`, their calls are not followed |
| **Modules** | TypeScript: relative imports, package names, barrels (`export * from`), tsconfig `paths` | ✅ |
| | TypeScript: CommonJS `require` | ◐ files that `require` a module count as importing it; calls through the binding are not resolved |
| | Go: packages of the repository's modules; calls within a package across its files | ✅ |
| | Go: methods (`s.pool.Query`) | ✅ in the caller's own package, else the one package it imports with a method of that name |
| | Go: calls through an interface | ◐ every type in the repository with the interface's methods (by name), up to 8, marked `inferred` |
| | Python: absolute and relative imports, `__init__.py` re-exports, `src/` layouts | ✅ |
| | Python: methods on instances (`repo.save()`) | ◐ when the code states the class: a parameter's annotation, `x = Repo(…)`, `self.x = …` or `self.x: Repo` in the class; otherwise only a method of that name in the calling file |
| **Workspaces** | pnpm (`pnpm-workspace.yaml`), npm and yarn (`package.json` `workspaces`) | ✅ |
| | Go modules (`go.mod`, several per repository) | ✅ each package is its directory |
| | Python projects (`pyproject.toml`, `setup.py`) | ✅ |
| | a single package | ✅ |
| **Routes** | Hono, Go `HandleFunc`/`Get`/`Post`… with a function literal, Python `@app.get(…)`/`@router.post(…)`/`@bp.route(…)` | ✅ named by method and path |
| **Across HTTP** | Hono's typed client (`client.api.….$post`) → its routes | ✅ |
| | `fetch`, axios, other clients | ➖ add a [seam](#seams-calls-joined-by-a-string) |
| **Across strings** | events, queues, job names, DI tokens | ➖ add a [seam](#seams-calls-joined-by-a-string) |
| **Risk rules** | call graph (public API, reach), TypeScript and Python (await in loops), Go (defer in loops), PostgreSQL migrations | ✅ built in |
| | anything else | ➖ your own `[[risk]]` rules, or another analyser's SARIF |
| **Platforms** | macOS, Linux | ✅ tested in CI |
| | Windows | ❔ untested; the editor key and commit extraction call `sh` and `tar` |

✅ supported · ◐ partly · ➖ through configuration · ❌ not supported · ❔ unknown

Everything a diff touches appears in `plan` as a changed file, whether or
not calls are followed through it, and `assess` still applies line and SQL
rules to it.

## Caches

Each commit's tree is extracted once into `$TMPDIR/prognost-cache/<sha>`
and reused on later runs; on a large repository that grows by the size of
the checkout per commit looked at. `prognost cache` shows where it is and
how much it takes, and `prognost cache clean` removes it — trees are
extracted again when next needed.
