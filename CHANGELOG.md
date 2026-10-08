# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
- `cache`: where extracted commit trees are kept and how much they take;
  `cache clean` removes them.

### Fixed

- A diff that changes no function (docs, config, SQL, another language) no
  longer fails `plan`: it gives a plan of its files, which `assess` still
  judges; `graph` says there is nothing to draw.

### Security

- `serve` listens only on loopback addresses unless `--expose`: its API
  acts with your `gh` login and has no authentication of its own.
