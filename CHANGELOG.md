# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Go: functions, methods and function literals; calls through the
  packages of the repository's `go.mod` modules, within a package across
  its files, and to methods; routes registered with a function literal.
- Python: functions, methods and lambdas; calls through absolute and
  relative imports and `__init__.py` re-exports; routes from decorators
  (`@app.get`, `@router.post`, `@bp.route`).
- Go calls through an interface reach every implementation in the
  repository (up to 8), as `inferred` calls: `"inferred": true` in the
  plan, a hollow arrowhead `▷` in the graph, a dotted curve on the web
  page.
- Python calls on instances whose class the code states (annotations,
  `x = Repo(…)`, `self.x = …`) reach the class's method.
- `[[resolver]]` in `prognost.toml`: an external command (Go's
  `callgraph`, or anything printing call site → function positions) that
  answers the calls prognost cannot resolve by reading code. Run once per
  revision; a commit's answer is cached.
- Presets `go` (`defer-in-loop`) and `python` (`python-await-in-loop`),
  on by default; `ast` rules take `language = "go"` or `"python"`.

## [0.1.0] - 2026-10-08

### Added

- `graph`: the changed functions of a diff as a call graph in the terminal,
  with callers found on demand, per-package detail levels, a diff view, an
  editor key, seen marks and GitHub *Viewed* sync; `--html` for a web page.
- `serve`: the web page over HTTP, keeping seen marks and *Viewed* in sync.
- `plan`: the changed functions, files, calls and reach of a diff, as text
  or JSON (`--json`, schema in `--schema`).
- `assess`: a plan judged by rules written as data — presets for the call
  graph, TypeScript and PostgreSQL migrations, plus a repository's own
  `prognost.toml` — and by other analysers' SARIF; `--fail-on` for CI.
- `rules`: the rules in force.
- `align`: one file's functions aligned across two revisions.
- herdr: `graph` without a terminal opens in a pane beside the caller; `o`
  edits in a pane beside the graph.
- `--color auto|always|never`, `--no-color`, `NO_COLOR`, `CLICOLOR_FORCE`.
- Releases: prebuilt binaries for macOS and Linux (arm64 and x86_64), a
  shell installer, and a Homebrew formula in `azihsoyn/tap`.
- `cache`: where extracted commit trees are kept and how much they take;
  `cache clean` removes them.

### Fixed

- A diff that changes no function (docs, config, SQL, another language) no
  longer fails `plan`: it gives a plan of its files, which `assess` still
  judges; `graph` says there is nothing to draw.

### Security

- `serve` listens only on loopback addresses unless `--expose`: its API
  acts with your `gh` login and has no authentication of its own.
