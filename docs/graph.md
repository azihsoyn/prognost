# graph and serve

`prognost graph` draws the diff as a graph you walk: every changed
function, grouped by package and file, with the code that calls it and the
code it calls. Callers are found as you step towards them, so the graph
grows only where you look.

```
prognost graph [--base <rev>] [--head <rev>] [--lod <pkg>=<n>] [--hops N]
prognost graph --html page.html     # the same graph as a single web page
prognost graph --dump graph.txt     # the same graph as text
prognost serve [host:port]          # the web page over HTTP, with seen marks
```

## Reading it

- **Colour**: yellow, a function whose body changed; green, added; red,
  removed; grey, unchanged. A green or red line is a call the diff added or
  removed. Magenta lines are the selected function's call chains, bright
  for its own calls.
- **Without colour** (`--no-color`, `NO_COLOR`): changed, added and removed
  functions are underlined, unchanged ones dim, the selection reversed.
- **Boxes** are packages, left to right by who calls whom; inside a box,
  columns follow call depth. `▸`/`▾` headers are directories and files.

## Keys

| key | does |
|---|---|
| `←` `→` / `h` `l` | step to a caller / a callee of the selected function (looking for more when there are none on screen yet) |
| `↑` `↓` / `k` `j` | the function above / below, in this box or the nearest one past its edge |
| `Enter` | the diff of the selected function's file, scrolled to it; on a folded group, open it |
| `o` | open the file at the function in `$VISUAL` / `$EDITOR` (inside herdr, in a pane beside) |
| `z` / `Z` | less / more detail for the selected package: functions → files → directories → package |
| `g` | switch the whole graph between functions, files and packages |
| `a` | show or hide callees that didn't change |
| `c` | collapse what was expanded from the selected node |
| `v` / `V` | mark the selected file / package seen (toggle) |
| `S` / `P` | push seen files to GitHub's *Viewed* / pull *Viewed* into seen marks |
| `m` | the minimap |
| `Shift`+arrows, `H` `J` `K` `L`, wheel, drag | pan; `0` back to the left edge |
| click | select |
| `q` / `Esc` | quit (in the diff: back to the graph) |

In the diff: `j`/`k` or arrows scroll, `PageUp`/`PageDown` page, `v` marks
the file seen, `Enter`/`q`/`Esc` go back.

## The web page

`--html` writes one self-contained file (no network, no server). Packages
are cards; click a function to light every chain through it and list its
callers and callees, double-click to read its diff, and use the package
buttons to fold a package into files, directories or one node. "hops from
the change" limits how far upstream is drawn; "focus the change" and "fit
everything" frame the view. Drag to pan, ⌘/Ctrl + wheel to zoom.

## serve: seen marks and GitHub's Viewed

`prognost serve` serves the same page with a small API behind it, so what
you mark is kept:

- **Seen marks** are stored per changed hunk in
  `.git/slidiff/seen.json` — the store [slidiff](https://github.com/azihsoyn/slidiff)
  uses, so the two tools agree on what you have looked at. A file is *done*
  when every changed line in it is seen.
- **GitHub's Viewed** checkboxes are pushed and pulled through the
  [`gh`](https://cli.github.com) CLI, with your own login. The pull request
  is the open one containing the head commit, or `--pr <number>`.

The terminal graph does the same with `v`, `S` and `P`.

## Inside herdr

In a [herdr](https://herdr.dev) pane, prognost uses the workspace around it:

- `prognost graph` run without a terminal — by a coding agent, or a script —
  opens the graph in a new pane beside the caller instead of failing.
  `--pane` asks for that from a terminal too.
- `o` opens the editor in a pane beside the graph, so the graph stays up.

Panes split to the right when the caller is wide and down when it is tall,
and are labelled `prognost graph` / `prognost edit`. Outside herdr none of
this happens.
