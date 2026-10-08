# Writing rules for prognost assess

Every finding `prognost assess` reports comes from a rule, and every rule is
data: a `[[risk]]` table in TOML. The built-in ones ship as presets
(`presets/*.toml`, compiled into the binary); a repository chooses
presets and adds, replaces or turns off rules in its own `prognost.toml`
(or the file `PROGNOST_CONFIG` names, for a repository that doesn't carry
one).

```toml
presets = ["graph", "typescript", "postgres"]   # default: all presets
disable = ["wide-reach"]                        # turn rules off by name

[[risk]]                                        # a new rule
name = "admin-privileges"
pattern = '''runWithAdminPrivileges\('''
unless = '''^\s*(import|export)\b'''
severity = "high"
paths = '''\.ts$'''
title = "runs a query with row-level security bypassed{in_function}"
```

A rule in the config whose `name` matches preset rules replaces all of
them. `prognost rules` lists what is in force and where each rule came
from; an invalid rule stops it with the reason.

## Presets

| preset | covers |
|---|---|
| `graph` | `public-api-change`, `wide-reach`: from prognost's call graph, any language |
| `typescript` | `await-in-loop`: TypeScript, JavaScript and the script of Svelte components |
| `postgres` | `migration-*`: PostgreSQL migrations |

## Fields every rule has

| field | meaning |
|---|---|
| `name` | the id shown in the report; rules sharing a name are tiers (see `symbol`) |
| `kind` | `line` (default), `symbol`, `sql` or `ast` |
| `title` | the finding's text; `{placeholders}` are filled in, unknown ones left empty |
| `severity` | `high`, `medium` (default) or `low` |
| `paths` | only files whose repo-relative path matches this regex |
| `fold` | fold this rule's findings of one severity in one file into one line |
| `[[risk.variant]]` | exceptions that keep the hit but change it: the first whose `pattern` matches the line, or whose `when` fact holds, sets `severity` and `title` |

## kind = "line"

A regex over lines.

- `pattern`: the regex. Named groups become placeholders (`(?P<fn>\w+)` → `{fn}`).
- `unless`: skip a hit whose matched text matches this.
- `scope`: `added` (default) checks the lines the diff added; `reach`
  checks every line of every function the change reaches upstream, once
  per function — "this change runs under X".
- Placeholder `{in_function}`: ` in <changed function>`, or empty.

## kind = "symbol"

A changed function, judged by what the call graph knows about it.

- `where`: conditions that must all hold: `field`, `!field`, or
  `field <op> value` with `==`, `!=`, `>=`, `<=`, `>`, `<`.
- Fields: `change` (`added`, `removed`, `changed`), `exported`, `route`,
  `public` (a route, or exported and imported by another package),
  `importing_packages`, `reach_functions`, `reach_files`,
  `reach_packages`, `entries`.
- Rules with the same name are tiers, tried in order; the first that
  holds wins for that function.
- Placeholders: `{name}`, `{route}`, `{change}`, `{importing_packages}`,
  `{importing_packages_list}`, `{called_from}` (`; called from a, b` or
  empty), `{reach_functions}`, `{reach_files}`, `{reach_packages}`,
  `{entries}`.

## kind = "sql"

A regex over whole SQL statements of migrations.

- Files: `.sql` under a directory whose name starts with `migration`,
  unless `paths` says otherwise. Only the forward half of a file is read
  (up to `-- Down Migration`, `-- migrate:down`, `-- +goose Down`,
  `-- +migrate Down`), and only statements the diff added are judged.
- `pattern`: matched against each statement (comments removed); every
  match is a hit, so one statement can yield several.
- `unless`: skip a match whose text matches this.
- `skip_new_tables` (default true): skip statements on a table the same
  file creates. The table is the `table` named group if the pattern has
  one, else the word after `TABLE`.
- `in_set = { capture = "type", set = "checked_domains" }`: keep a match
  only when that named group's value (lowercased) is in a set.

### Sets

Values gathered from the whole head revision, for `in_set`:

```toml
[[set]]
name = "checked_domains"
paths = '''\.sql$'''
patterns = ['''(?is)create\s+domain\s+(?P<name>\w+)\s+as\s+[^;]*?\bcheck\b''']
```

Each match's `name` group (else group 1) is collected, lowercased. A set
in the config replaces a preset set of the same name.

## kind = "ast"

A [tree-sitter query](https://tree-sitter.github.io/tree-sitter/using-parsers/queries/)
over TypeScript / JavaScript (and Svelte scripts).

- `query`: the query; the `@hit` capture (else a match's first capture)
  is the hit, reported when it starts on an added line.
- `pattern`: optionally, a regex the hit's line must match.
- `filters`: built-in narrowing that a query alone cannot express:
  - `per-loop-iteration`: keep a hit only when it runs once per
    iteration of a loop in the same function (not in a nested callback,
    a loop header, a `for await` body, or on the way out of the loop).
    Establishes the fact `loop-stops-early` when the loop can `return`
    or `break`, for a variant's `when`.
- Placeholder `{in_function}` as for `line`.

## Results from other analysers

`prognost assess --sarif <file>` (repeatable) reads a SARIF 2.1 log —
ESLint, semgrep, a SQL linter, anything that writes one — and keeps the
results on lines the diff added. They appear as `<tool>/<ruleId>`;
`error` is medium, anything else low. The analyser judges the code;
prognost narrows it to the change.
