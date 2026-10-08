# In CI

`prognost plan` and `prognost assess` are deterministic and need only git
history, so they fit a pull-request check. A GitHub Actions job that fails
on high-severity findings and keeps the plan as an artifact:

```yaml
name: prognost
on: pull_request

jobs:
  assess:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v5
        with:
          fetch-depth: 0          # the merge base must be reachable
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo install --locked --git https://github.com/azihsoyn/prognost
      - name: Plan
        run: prognost plan --base origin/${{ github.base_ref }} --head HEAD --json > plan.json
      - name: Assess
        run: prognost assess plan.json --fail-on high
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: prognost-plan
          path: plan.json
```

- `--fail-on high` exits 1 when anything high-severity is found; use
  `medium` to be stricter, or drop it to report only.
- Pipe other analysers in with `--sarif`: run ESLint with a SARIF formatter
  (or semgrep, or a SQL linter), then
  `prognost assess plan.json --sarif eslint.sarif` keeps only the results on
  lines this pull request added.
- `--json` on either command gives machine-readable output; the shapes are
  in `prognost --schema`.
