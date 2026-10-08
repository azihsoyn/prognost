//! The graph as a web page — not the terminal's cells, the same
//! structure drawn the way a browser can: package cards laid out by who
//! calls whom, directory and file groups inside, function rows coloured
//! by their diff status, edges as curves that light up along a node's
//! call chains on click, per-package detail levels, pan and zoom. One
//! self-contained file, no server — `prognost … --html out.html`.

/// One function (or unresolved call / module-level reference).
pub struct ModelNode {
    pub id: String,
    pub label: String,
    /// "added" | "removed" | "changed" | "unchanged"
    pub status: &'static str,
    pub pkg: String,
    /// Package-relative directory, empty for none.
    pub dir: String,
    /// Package-relative file path, empty for none.
    pub file: String,
    pub line: u32,
    /// Last line of the function in HEAD (or BASE when removed).
    pub end: u32,
    /// Repo-relative path, the key into `Model::files`.
    pub path: String,
    pub origin: bool,
    pub external: bool,
    pub route: Option<String>,
}

/// One line of a whole-file diff: 0 context, 1 added, 2 removed; the
/// HEAD line number (0 for a removed line); the text.
pub type DiffRow = (u8, u32, String);

pub struct ModelEdge {
    pub from: String,
    pub to: String,
    pub status: &'static str,
}

pub struct Model {
    pub base: String,
    pub head: String,
    pub nodes: Vec<ModelNode>,
    pub edges: Vec<ModelEdge>,
    /// Whole-file diffs, by repo-relative path, for every file a node
    /// points into — what Enter shows in the terminal.
    pub files: Vec<(String, Vec<DiffRow>)>,
    /// Served by `prognost --serve`: the page may call `/api/…`.
    pub api: bool,
}

pub fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl Model {
    fn json(&self) -> String {
        let nodes: Vec<String> = self
            .nodes
            .iter()
            .map(|n| {
                format!(
                    "{{\"id\":{},\"label\":{},\"status\":{},\"pkg\":{},\"dir\":{},\"file\":{},\"line\":{},\"end\":{},\"path\":{},\"origin\":{},\"external\":{},\"route\":{}}}",
                    json_str(&n.id),
                    json_str(&n.label),
                    json_str(n.status),
                    json_str(&n.pkg),
                    json_str(&n.dir),
                    json_str(&n.file),
                    n.line,
                    n.end,
                    json_str(&n.path),
                    n.origin,
                    n.external,
                    n.route.as_deref().map(json_str).unwrap_or_else(|| "null".into())
                )
            })
            .collect();
        let edges: Vec<String> = self
            .edges
            .iter()
            .map(|e| {
                format!(
                    "{{\"from\":{},\"to\":{},\"status\":{}}}",
                    json_str(&e.from),
                    json_str(&e.to),
                    json_str(e.status)
                )
            })
            .collect();
        let files: Vec<String> = self
            .files
            .iter()
            .map(|(path, rows)| {
                let rows: Vec<String> = rows
                    .iter()
                    .map(|(k, l, t)| format!("[{k},{l},{}]", json_str(t)))
                    .collect();
                format!("{}:[{}]", json_str(path), rows.join(","))
            })
            .collect();
        format!(
            "{{\"base\":{},\"head\":{},\"nodes\":[{}],\"edges\":[{}],\"files\":{{{}}}}}",
            json_str(&self.base),
            json_str(&self.head),
            nodes.join(","),
            edges.join(","),
            files.join(",")
        )
    }

    pub fn render(&self) -> String {
        let head_short: String = self.head.chars().take(10).collect();
        let title = format!("prognost · {head_short}");
        PAGE.replace("/*__TITLE__*/", &title.replace('<', "&lt;"))
            .replace("/*__MODEL__*/", &self.json())
            .replace("/*__API__*/", if self.api { "true" } else { "false" })
    }
}

const PAGE: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>/*__TITLE__*/</title>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;500;600&family=IBM+Plex+Sans:wght@400;500;600&display=swap">
<style>
:root {
  --ground:#f6f4ee; --surface:#ffffff; --surface-2:#edeae0; --ink:#1c1b18; --muted:#726d60; --line:#cac4b3;
  --accent:#0f6e64; --accent-bg:#e2efec;
  --added:#1f8a4c; --added-bg:#e4f5ea; --removed:#c0392b; --removed-bg:#fbeae8; --changed:#a3720a; --changed-bg:#f7ecd6;
  --hi:#b5179e; --hi-2:#d46bc4; --edge:#b9b3a2;
  --mono:"IBM Plex Mono",ui-monospace,"SF Mono",Consolas,monospace; --sans:"IBM Plex Sans",ui-sans-serif,system-ui,sans-serif;
  color-scheme: light;
}
@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) {
  --ground:#15161b; --surface:#1d1f26; --surface-2:#23252d; --ink:#ece9e1; --muted:#9c988d; --line:#3a3c46;
  --accent:#4fd1c0; --accent-bg:#1c3532; --added:#4ade80; --added-bg:#17301f; --removed:#ff7a6b; --removed-bg:#3a1c19;
  --changed:#eab654; --changed-bg:#3a2e14; --hi:#f0abfc; --hi-2:#c084fc; --edge:#4b4e5a; color-scheme: dark;
} }
:root[data-theme="dark"] {
  --ground:#15161b; --surface:#1d1f26; --surface-2:#23252d; --ink:#ece9e1; --muted:#9c988d; --line:#3a3c46;
  --accent:#4fd1c0; --accent-bg:#1c3532; --added:#4ade80; --added-bg:#17301f; --removed:#ff7a6b; --removed-bg:#3a1c19;
  --changed:#eab654; --changed-bg:#3a2e14; --hi:#f0abfc; --hi-2:#c084fc; --edge:#4b4e5a; color-scheme: dark;
}
* { box-sizing:border-box; }
html, body { height:100%; }
body { margin:0; background:var(--ground); color:var(--ink); font-family:var(--sans); font-size:14px; display:flex; flex-direction:column; }
header { padding:14px 20px 12px; border-bottom:1px solid var(--line); display:flex; flex-wrap:wrap; gap:8px 24px; align-items:baseline; }
.eyebrow { font-family:var(--mono); font-size:11px; letter-spacing:.08em; text-transform:uppercase; color:var(--accent); }
h1 { margin:0; font-size:18px; font-weight:600; }
.revs { font-family:var(--mono); font-size:12.5px; color:var(--muted); }
.revs b { color:var(--ink); font-weight:500; }
.legend { display:flex; flex-wrap:wrap; gap:14px; font-family:var(--mono); font-size:11.5px; color:var(--muted); margin-left:auto; }
.legend span { display:inline-flex; align-items:center; gap:6px; }
.sw { width:10px; height:10px; border-radius:3px; display:inline-block; }
.sw.added{background:var(--added)} .sw.removed{background:var(--removed)} .sw.changed{background:var(--changed)} .sw.unchanged{background:var(--line)}
.sw.hi{background:var(--hi)} .sw.hi2{background:var(--hi-2)}
#stage { flex:1; position:relative; overflow:hidden; cursor:grab; }
#stage.dragging { cursor:grabbing; }
#board { position:absolute; left:0; top:0; transform-origin:0 0; padding:28px; display:flex; gap:120px; align-items:flex-start; }
#edges { position:absolute; left:0; top:0; overflow:visible; pointer-events:none; }
.pcol { display:flex; flex-direction:column; gap:26px; }
.pkg { background:var(--surface); border:1px solid var(--line); border-radius:14px; padding:10px 12px 12px; min-width:240px; box-shadow:0 1px 2px rgba(0,0,0,.05); }
.pkg.sel { border-color:var(--accent); box-shadow:0 0 0 2px var(--accent-bg); }
.pkg-h { display:flex; align-items:center; gap:10px; margin-bottom:8px; }
.pkg-h h2 { margin:0; font-family:var(--mono); font-size:13px; font-weight:600; color:var(--accent); flex:1; white-space:nowrap; }
.lod { display:inline-flex; background:var(--surface-2); border:1px solid var(--line); border-radius:999px; padding:2px; gap:1px; }
.lod button { font-family:var(--mono); font-size:10.5px; border:0; background:transparent; color:var(--muted); padding:3px 8px; border-radius:999px; cursor:pointer; }
.lod button.on { background:var(--surface); color:var(--ink); box-shadow:0 1px 2px rgba(0,0,0,.08); }
.cols { display:flex; gap:34px; align-items:flex-start; }
.lcol { display:flex; flex-direction:column; gap:6px; min-width:200px; }
.dir { font-family:var(--mono); font-size:10.5px; color:var(--muted); margin:6px 0 0; }
.dir::before { content:"▸ "; }
.file { font-family:var(--mono); font-size:11px; color:var(--accent); font-style:italic; margin:2px 0 0 4px; }
.node { position:relative; font-family:var(--mono); font-size:12px; padding:5px 10px; border-radius:8px; border:1px solid var(--line); background:var(--surface); cursor:pointer; white-space:nowrap; display:flex; align-items:center; gap:8px; }
.node .l { min-width:0; }
.node .sub { font-size:10.5px; color:var(--muted); }
.node.added { border-color:var(--added); background:var(--added-bg); }
.node.removed { border-color:var(--removed); background:var(--removed-bg); border-style:dashed; }
.node.changed { border-color:var(--changed); background:var(--changed-bg); }
.node.origin .l { font-weight:600; }
.node.external { color:var(--muted); border-style:dotted; }
.node.sel { outline:2px solid var(--hi); outline-offset:1px; }
.node.up, .node.down { box-shadow:0 0 0 2px var(--hi-2) inset; }
.badge { font-size:9.5px; font-weight:600; letter-spacing:.03em; padding:1px 6px; border-radius:999px; color:var(--surface); }
.badge.added{background:var(--added)} .badge.removed{background:var(--removed)} .badge.changed{background:var(--changed)}
.line { font-size:10.5px; color:var(--muted); }
.route { font-size:9.5px; padding:1px 6px; border-radius:999px; background:var(--accent-bg); color:var(--accent); font-weight:600; }
.agg { font-weight:600; }
#panel { position:absolute; right:14px; bottom:14px; width:360px; max-height:46%; overflow:auto; background:var(--surface); border:1px solid var(--line); border-radius:12px; padding:12px 14px; font-size:12.5px; box-shadow:0 6px 24px rgba(0,0,0,.12); display:none; }
#panel.on { display:block; }
#panel h3 { margin:0 0 4px; font-family:var(--mono); font-size:13px; font-weight:600; }
#panel .meta { font-family:var(--mono); font-size:11px; color:var(--muted); margin-bottom:8px; word-break:break-all; }
#panel ul { margin:4px 0 8px; padding-left:16px; }
#panel li { font-family:var(--mono); font-size:11.5px; cursor:pointer; }
#panel li:hover { color:var(--accent); }
#panel .k { font-size:10.5px; letter-spacing:.06em; text-transform:uppercase; color:var(--muted); margin-top:6px; }
#hint { position:absolute; left:14px; bottom:12px; font-family:var(--mono); font-size:11px; color:var(--muted); background:var(--surface); border:1px solid var(--line); border-radius:8px; padding:5px 9px; }
svg path.e { fill:none; stroke:var(--edge); stroke-width:1.4; }
svg path.e.added { stroke:var(--added); } svg path.e.removed { stroke:var(--removed); stroke-dasharray:5 4; }
svg path.e.dim { opacity:.18; }
svg path.e.hi { stroke:var(--hi); stroke-width:2.6; } svg path.e.hi2 { stroke:var(--hi-2); stroke-width:2.2; }
svg marker path { fill:var(--edge); }
.file { display:flex; align-items:center; gap:6px; }
.seen { font-size:10px; color:var(--muted); font-style:normal; }
.seen.done { color:var(--added); font-weight:600; }
.seenbtn { font-family:var(--mono); font-size:10px; border:1px solid var(--line); background:var(--surface); color:var(--muted); border-radius:5px; padding:0 5px; cursor:pointer; font-style:normal; line-height:16px; }
.seenbtn.done { color:var(--added); border-color:var(--added); }
#gh { font-family:var(--mono); font-size:11px; color:var(--muted); align-self:center; display:flex; gap:8px; align-items:center; }
#gh button { font-family:var(--mono); font-size:11px; border:1px solid var(--line); background:var(--surface-2); color:var(--ink); border-radius:6px; padding:4px 9px; cursor:pointer; }
#drawer { position:absolute; top:0; right:0; bottom:0; width:min(760px, 62vw); background:var(--surface); border-left:1px solid var(--line); box-shadow:-8px 0 24px rgba(0,0,0,.12); display:none; flex-direction:column; z-index:5; }
#drawer.on { display:flex; }
#drawer .dh { display:flex; align-items:center; gap:10px; padding:10px 14px; border-bottom:1px solid var(--line); font-family:var(--mono); font-size:12px; }
#drawer .dh b { font-weight:600; }
#drawer .dh .p { color:var(--muted); flex:1; min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
#drawer .dh button { font-family:var(--mono); font-size:11px; border:1px solid var(--line); background:var(--surface-2); color:var(--ink); border-radius:6px; padding:3px 8px; cursor:pointer; }
#code { flex:1; overflow:auto; font-family:var(--mono); font-size:12px; line-height:1.45; padding:6px 0 40vh; cursor:text; }
.row { display:flex; white-space:pre; }
.row .g { width:56px; flex:none; text-align:right; padding-right:8px; color:var(--muted); user-select:none; }
.row .m { width:14px; flex:none; color:var(--muted); user-select:none; }
.row.add { background:var(--added-bg); } .row.add .m { color:var(--added); }
.row.del { background:var(--removed-bg); } .row.del .m { color:var(--removed); } .row.del .t { color:var(--removed); }
.row.scope { box-shadow:inset 3px 0 0 var(--accent); }
.row.first { outline:1px solid var(--changed); }
</style></head><body>
<header>
  <div><div class="eyebrow">prognost</div><h1 id="title">whole diff</h1></div>
  <div class="revs" id="revs"></div>
  <div class="lod" style="align-self:center"><button id="fit-changes" class="on">focus the change</button><button id="fit-all">fit everything</button><button id="fit-1">100%</button></div>
  <div class="lod" style="align-self:center" id="depth" title="how many calls away from the change to show"></div>
  <div id="gh" hidden><span id="ghstat"></span><button id="ghpush">mark done files Viewed on GitHub</button><button id="ghpull">pull Viewed</button></div>
  <div class="legend">
    <span><i class="sw changed"></i>changed</span><span><i class="sw added"></i>added</span><span><i class="sw removed"></i>removed</span><span><i class="sw unchanged"></i>unchanged</span>
    <span><i class="sw hi"></i>selected node's calls</span><span><i class="sw hi2"></i>rest of its chains</span>
  </div>
</header>
<div id="stage">
  <div id="board"><svg id="edges"><defs>
    <marker id="m" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto"><path d="M0,0 L10,5 L0,10 z"/></marker>
    <marker id="m-added" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto"><path d="M0,0 L10,5 L0,10 z" style="fill:var(--added)"/></marker>
    <marker id="m-removed" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto"><path d="M0,0 L10,5 L0,10 z" style="fill:var(--removed)"/></marker>
    <marker id="m-hi" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto"><path d="M0,0 L10,5 L0,10 z" style="fill:var(--hi)"/></marker>
    <marker id="m-hi2" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto"><path d="M0,0 L10,5 L0,10 z" style="fill:var(--hi-2)"/></marker>
  </defs><g id="eg"></g></svg></div>
  <div id="hint">drag to pan · ⌘/ctrl + wheel to zoom · click a node to light its chains · double-click a function to open its file at the change · package buttons: functions / files / directories / package</div>
  <aside id="panel"></aside>
  <aside id="drawer"><div class="dh"><b id="dtitle"></b><span class="p" id="dpath"></span><button id="dseen" hidden>mark seen · v</button><button id="dclose">close · esc</button></div><div id="code"></div></aside>
</div>
<script>
const M = /*__MODEL__*/;
const API = /*__API__*/;
let SEEN = {};   // path -> [seen, total, done]
async function loadSeen() { if (!API) return; try { SEEN = await (await fetch('/api/seen')).json(); } catch (e) { SEEN = {}; } paintSeen(); }
async function toggleSeen(path) { if (!API) return; try { SEEN = await (await fetch('/api/seen/toggle?path=' + encodeURIComponent(path), { method: 'POST' })).json(); } catch (e) {} paintSeen(); }
function seenLabel(path) { const p = SEEN[path]; if (!p) return ''; const [s, t, d] = p; return d ? '✓ seen' : s > 0 ? `${s}/${t} seen` : ''; }
function paintSeen() {
  for (const el of board.querySelectorAll('.file[data-path]')) {
    const path = el.dataset.path; const p = SEEN[path]; const b = el.querySelector('.seenbtn');
    if (!b) continue;
    const done = !!(p && p[2]);
    if (p && p[1] === 0) { b.hidden = true; continue; }
    b.hidden = false;
    b.textContent = done ? '✓ seen' : (p && p[0] > 0) ? `${p[0]}/${p[1]}` : 'seen?';
    b.classList.toggle('done', done);
  }
  const changed = Object.values(SEEN).filter(p => p[1] > 0);
  const done = changed.filter(p => p[2]).length, total = changed.length;
  const gh = $('#gh'); if (API) { gh.hidden = false; $('#ghstat').textContent = `${done}/${total} files seen`; }
  const ds = $('#dseen'); if (ds && drawer.dataset.path) { const p = SEEN[drawer.dataset.path]; ds.textContent = (p && p[2]) ? '✓ seen · v to undo' : 'mark seen · v'; ds.classList.toggle('done', !!(p && p[2])); }
}
const $ = (s, el = document) => el.querySelector(s);
const stage = $('#stage'), board = $('#board'), eg = $('#eg'), svg = $('#edges'), panel = $('#panel');
const LOD = ['functions', 'files', 'directories', 'package'];
const lod = {};            // pkg -> 0..3
let selected = null;       // view id
let view = { nodes: [], edges: [], of: {}, members: {} };
const byId = Object.fromEntries(M.nodes.map(n => [n.id, n]));
// How far each node is from the change: hops upstream (callers of
// callers…) or downstream from the nearest changed function.
const DIST = {};
{
  const origins = M.nodes.filter(n => n.origin).map(n => n.id);
  const into = {}, from = {};
  for (const e of M.edges) { (into[e.to] = into[e.to] || []).push(e.from); (from[e.from] = from[e.from] || []).push(e.to); }
  let frontier = origins.slice(); origins.forEach(o => DIST[o] = 0);
  while (frontier.length) { const next = []; for (const v of frontier) for (const w of [...(into[v] || []), ...(from[v] || [])]) if (DIST[w] === undefined) { DIST[w] = DIST[v] + 1; next.push(w); } frontier = next; }
}
let depth = 2;
const DEPTHS = [1, 2, 3, 4, 6, Infinity];
const pkgs = [...new Set(M.nodes.map(n => n.pkg))];
// Packages with changed functions open at function level; packages that
// only call into the change start folded to files, so a deep upstream
// walk doesn't bury the change itself.
pkgs.forEach(p => lod[p] = M.nodes.some(n => n.pkg === p && n.origin) ? 0 : 1);

$('#title').textContent = `${M.nodes.filter(n => n.origin).length} changed functions across ${new Set(M.nodes.filter(n => n.origin).map(n => n.pkg + '/' + n.file)).size} files`;
$('#revs').innerHTML = `base <b>${M.base}</b> → head <b>${M.head}</b>`;

// ---- detail levels: fold functions into files / directories / the package ----
function shown(n) { return (DIST[n.id] ?? Infinity) <= depth; }
function buildView() {
  const nodes = [], of = {}, members = {};
  for (const n of M.nodes.filter(shown)) {
    const L = lod[n.pkg];
    if (L === 0) { nodes.push({ ...n, vid: n.id, agg: false }); of[n.id] = n.id; members[n.id] = [n.id]; continue; }
    const key = L === 1 ? (n.file || n.id) : L === 2 ? n.dir : '';
    const vid = `agg::${n.pkg}::${L}::${key}`;
    of[n.id] = vid; (members[vid] = members[vid] || []).push(n.id);
    if (!nodes.some(v => v.vid === vid)) nodes.push({ vid, agg: true, pkg: n.pkg, dir: L === 1 ? n.dir : '', file: '', level: L, key, origin: false, external: false, route: null, line: 0 });
  }
  for (const v of nodes.filter(v => v.agg)) {
    const ms = members[v.vid].map(id => byId[id]);
    const st = ms.map(m => m.status);
    v.status = st.every(s => s === 'added') ? 'added' : st.every(s => s === 'removed') ? 'removed' : st.some(s => s !== 'unchanged') ? 'changed' : 'unchanged';
    v.origin = ms.some(m => m.origin);
    const files = new Set(ms.map(m => m.file).filter(Boolean)).size, fns = ms.length;
    const f = (n, w) => `${n} ${w}${n === 1 ? '' : 's'}`;
    if (v.level === 1) { v.label = v.key.split('/').pop(); v.sub = f(fns, 'function'); }
    else if (v.level === 2) { v.label = (v.key || '.') + '/'; v.sub = `${f(files, 'file')} · ${f(fns, 'function')}`; }
    else { v.label = v.pkg; v.sub = `${f(files, 'file')} · ${f(fns, 'function')}`; }
    v.id = v.vid;
  }
  for (const v of nodes.filter(v => !v.agg)) v.id = v.vid;
  const rank = s => s === 'added' || s === 'removed' ? 3 : s === 'changed' ? 2 : 1;
  const edges = [];
  for (const e of M.edges) {
    const a = of[e.from], b = of[e.to]; if (!a || !b || a === b) continue;
    const ex = edges.find(x => x.from === a && x.to === b);
    if (ex) { if (rank(e.status) > rank(ex.status)) ex.status = e.status; } else edges.push({ from: a, to: b, status: e.status });
  }
  view = { nodes, edges, of, members };
}

// ---- layout: packages left→right by who calls whom; inside, columns by call depth ----
// Longest path from the sources with cycles cut: DFS order ignoring
// back edges, then one relaxation pass along it.
function longestPath(ids, edges) {
  const out = Object.fromEntries(ids.map(i => [i, []]));
  for (const e of edges) if (out[e.from] && out[e.to]) out[e.from].push(e.to);
  const color = {}, order = [];
  for (const start of ids) {
    if (color[start]) continue;
    const stack = [[start, 0]]; color[start] = 1;
    while (stack.length) {
      const top = stack[stack.length - 1]; const v = top[0];
      if (top[1] < out[v].length) { const w = out[v][top[1]++]; if (!color[w]) { color[w] = 1; stack.push([w, 0]); } }
      else { color[v] = 2; order.push(v); stack.pop(); }
    }
  }
  order.reverse();
  const pos = {}; order.forEach((v, i) => pos[v] = i);
  const r = Object.fromEntries(ids.map(i => [i, 0]));
  for (const v of order) for (const w of out[v]) if (pos[w] > pos[v] && r[w] < r[v] + 1) r[w] = r[v] + 1;
  return r;
}
function render() {
  buildView();
  const pkgOf = Object.fromEntries(view.nodes.map(n => [n.id, n.pkg]));
  const pe = view.edges.filter(e => pkgOf[e.from] !== pkgOf[e.to]).map(e => ({ from: pkgOf[e.from], to: pkgOf[e.to] }));
  const livePkgs = pkgs.filter(p => view.nodes.some(n => n.pkg === p));
  const prank = longestPath(livePkgs, pe);
  const ncols = Math.max(...Object.values(prank)) + 1;
  for (const el of [...board.children]) if (el !== svg) el.remove();
  for (let c = 0; c < ncols; c++) {
    const col = document.createElement('div'); col.className = 'pcol';
    const here = livePkgs.filter(p => prank[p] === c);
    // near their callers: order by mean index of caller packages in the previous column
    const prev = livePkgs.filter(p => prank[p] === c - 1);
    here.sort((a, b) => bary(a) - bary(b));
    function bary(p) { const srcs = pe.filter(e => e.to === p && prank[e.from] === c - 1).map(e => prev.indexOf(e.from)); return srcs.length ? srcs.reduce((x, y) => x + y, 0) / srcs.length : 1e9; }
    for (const p of here) col.appendChild(pkgCard(p));
    board.appendChild(col);
  }
  drawEdges();
  showPanel();
  paintSeen();
}
// Bring a set of cards into view: scaled to fit, top-left with a margin.
function fitTo(cards) {
  if (!cards.length) return;
  const b = board.getBoundingClientRect();
  let x0 = 1e9, y0 = 1e9, x1 = -1e9, y1 = -1e9;
  for (const c of cards) { const r = c.getBoundingClientRect(); x0 = Math.min(x0, (r.left - b.left) / scale); y0 = Math.min(y0, (r.top - b.top) / scale); x1 = Math.max(x1, (r.right - b.left) / scale); y1 = Math.max(y1, (r.bottom - b.top) / scale); }
  const sw = stage.clientWidth - 40, sh = stage.clientHeight - 40;
  scale = Math.max(.2, Math.min(1, sw / (x1 - x0), sh / (y1 - y0)));
  tx = 20 - x0 * scale; ty = 20 - y0 * scale; apply();
}
function fitChanges() { fitTo([...board.querySelectorAll('.pkg')].filter(c => c.querySelector('.node.origin'))); }
function fitAll() { fitTo([...board.querySelectorAll('.pkg')]); }
function pkgCard(p) {
  const card = document.createElement('section'); card.className = 'pkg'; card.dataset.pkg = p;
  const h = document.createElement('div'); h.className = 'pkg-h';
  const t = document.createElement('h2'); t.textContent = p; h.appendChild(t);
  const sw = document.createElement('div'); sw.className = 'lod';
  LOD.forEach((name, i) => { const b = document.createElement('button'); b.textContent = name; if (lod[p] === i) b.className = 'on'; b.onclick = e => { e.stopPropagation(); lod[p] = i; render(); }; sw.appendChild(b); });
  h.appendChild(sw); card.appendChild(h);
  const mine = view.nodes.filter(n => n.pkg === p);
  const intra = view.edges.filter(e => pkgOfView(e.from) === p && pkgOfView(e.to) === p);
  const lr = longestPath(mine.map(n => n.id), intra);
  const nc = Math.max(0, ...Object.values(lr)) + 1;
  const cols = document.createElement('div'); cols.className = 'cols';
  for (let c = 0; c < nc; c++) {
    const col = document.createElement('div'); col.className = 'lcol';
    const inCol = mine.filter(n => lr[n.id] === c);
    // directory bands, then files, in first-appearance order
    const dirs = [...new Set(inCol.map(n => n.dir))];
    for (const d of dirs) {
      const ofDir = inCol.filter(n => n.dir === d);
      if (d) { const dh = document.createElement('div'); dh.className = 'dir'; dh.textContent = d + '/'; col.appendChild(dh); }
      const files = [...new Set(ofDir.map(n => n.file))];
      for (const f of files) {
        if (f) {
          const fh = document.createElement('div'); fh.className = 'file';
          const full = ofDir.find(n => n.file === f)?.path || '';
          fh.dataset.path = full;
          const t = document.createElement('span'); t.textContent = f.split('/').pop(); fh.appendChild(t);
          if (API && full) { const b = document.createElement('button'); b.className = 'seenbtn'; b.textContent = 'seen?'; b.title = 'mark every changed line of this file seen'; b.onclick = e => { e.stopPropagation(); toggleSeen(full); }; fh.appendChild(b); }
          col.appendChild(fh);
        }
        for (const n of ofDir.filter(n => n.file === f)) col.appendChild(nodeEl(n));
      }
    }
    cols.appendChild(col);
  }
  card.appendChild(cols);
  return card;
}
function pkgOfView(id) { const n = view.nodes.find(n => n.id === id); return n ? n.pkg : ''; }
function nodeEl(n) {
  const el = document.createElement('div');
  el.className = `node ${n.status}${n.origin ? ' origin' : ''}${n.external ? ' external' : ''}${n.agg ? ' agg' : ''}`;
  el.dataset.id = n.id;
  const l = document.createElement('span'); l.className = 'l';
  l.textContent = n.route ? n.route : n.label;
  el.appendChild(l);
  if (n.sub) { const s = document.createElement('span'); s.className = 'sub'; s.textContent = n.sub; el.appendChild(s); }
  if (n.line) { const s = document.createElement('span'); s.className = 'line'; s.textContent = 'L' + n.line; el.appendChild(s); }
  if (n.status !== 'unchanged') { const b = document.createElement('span'); b.className = 'badge ' + n.status; b.textContent = n.status; el.appendChild(b); }
  el.onclick = e => { e.stopPropagation(); if (e.detail === 2) { if (n.agg) { lod[n.pkg] = Math.max(0, lod[n.pkg] - 1); render(); } else { selected = n.id; highlight(); openFile(n); } return; } selected = selected === n.id ? null : n.id; highlight(); };
  return el;
}

// ---- edges: curves between node boxes, in board coordinates ----
function nodeRect(id) {
  const el = board.querySelector(`.node[data-id="${CSS.escape(id)}"]`); if (!el) return null;
  const b = board.getBoundingClientRect(), r = el.getBoundingClientRect(), s = scale;
  return { x: (r.left - b.left) / s, y: (r.top - b.top) / s, w: r.width / s, h: r.height / s };
}
function drawEdges() {
  eg.innerHTML = '';
  svg.setAttribute('width', board.scrollWidth); svg.setAttribute('height', board.scrollHeight);
  for (const e of view.edges) {
    const a = nodeRect(e.from), b = nodeRect(e.to); if (!a || !b) continue;
    const x1 = a.x + a.w, y1 = a.y + a.h / 2, x2 = b.x - 2, y2 = b.y + b.h / 2;
    const dx = Math.max(40, Math.abs(x2 - x1) / 2);
    const d = x2 >= x1 ? `M${x1},${y1} C${x1 + dx},${y1} ${x2 - dx},${y2} ${x2},${y2}`
                       : `M${x1},${y1} C${x1 + 60},${y1} ${x2 - 60},${y2} ${x2},${y2}`;
    const p = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    p.setAttribute('d', d); p.setAttribute('class', 'e ' + e.status); p.dataset.from = e.from; p.dataset.to = e.to;
    p.setAttribute('marker-end', e.status === 'added' ? 'url(#m-added)' : e.status === 'removed' ? 'url(#m-removed)' : 'url(#m)');
    eg.appendChild(p);
  }
  highlight();
}
function chains(id) {
  const up = new Set(), down = new Set(); let f = [id];
  while (f.length) { const c = f.pop(); for (const e of view.edges) if (e.to === c && !up.has(e.from)) { up.add(e.from); f.push(e.from); } }
  f = [id];
  while (f.length) { const c = f.pop(); for (const e of view.edges) if (e.from === c && !down.has(e.to)) { down.add(e.to); f.push(e.to); } }
  return { up, down };
}
function highlight() {
  board.querySelectorAll('.node').forEach(el => el.classList.remove('sel', 'up', 'down'));
  board.querySelectorAll('.pkg').forEach(el => el.classList.remove('sel'));
  const paths = [...eg.children];
  if (!selected) { paths.forEach(p => { p.classList.remove('hi', 'hi2', 'dim'); p.setAttribute('marker-end', p.classList.contains('added') ? 'url(#m-added)' : p.classList.contains('removed') ? 'url(#m-removed)' : 'url(#m)'); }); showPanel(); return; }
  const { up, down } = chains(selected);
  for (const p of paths) {
    const f = p.dataset.from, t = p.dataset.to;
    const direct = f === selected || t === selected;
    const chained = (up.has(f) && up.has(t)) || (down.has(f) && down.has(t));
    p.classList.toggle('hi', direct); p.classList.toggle('hi2', !direct && chained); p.classList.toggle('dim', !direct && !chained);
    p.setAttribute('marker-end', direct ? 'url(#m-hi)' : chained ? 'url(#m-hi2)' : p.classList.contains('added') ? 'url(#m-added)' : p.classList.contains('removed') ? 'url(#m-removed)' : 'url(#m)');
    if (direct || chained) eg.appendChild(p);
  }
  const selEl = board.querySelector(`.node[data-id="${CSS.escape(selected)}"]`);
  if (selEl) { selEl.classList.add('sel'); selEl.closest('.pkg').classList.add('sel'); }
  up.forEach(id => { const el = board.querySelector(`.node[data-id="${CSS.escape(id)}"]`); if (el) el.classList.add('up'); });
  down.forEach(id => { const el = board.querySelector(`.node[data-id="${CSS.escape(id)}"]`); if (el) el.classList.add('down'); });
  showPanel();
}
// ---- the diff: the whole file, +/- marked, scrolled to the node's first change ----
const drawer = $('#drawer'), code = $('#code');
function openFile(n) {
  const rows = M.files[n.path]; if (!rows) return;
  code.innerHTML = '';
  let first = null;
  const frag = document.createDocumentFragment();
  rows.forEach(([k, l, t], i) => {
    const r = document.createElement('div');
    r.className = 'row' + (k === 1 ? ' add' : k === 2 ? ' del' : '');
    const inScope = n.line && l >= n.line && l <= n.end;
    if (inScope) r.classList.add('scope');
    if (first === null && k !== 0 && (inScope || (k === 2 && rows[i + 1] && rows[i + 1][1] >= n.line && rows[i + 1][1] <= n.end))) { first = r; r.classList.add('first'); }
    r.innerHTML = `<span class="g">${l || ''}</span><span class="m">${k === 1 ? '+' : k === 2 ? '−' : ''}</span><span class="t"></span>`;
    r.lastChild.textContent = t;
    frag.appendChild(r);
  });
  code.appendChild(frag);
  const target = first || [...code.children].find(r => Number(r.firstChild.textContent) >= n.line);
  $('#dtitle').textContent = n.route || n.label;
  $('#dpath').textContent = `${n.path}${n.line ? ' · L' + n.line : ''}${first ? ' · first change at L' + (first.firstChild.textContent || rows[[...code.children].indexOf(first) + 1]?.[1] || '?') : ' · no change inside'}`;
  drawer.dataset.path = n.path;
  $('#dseen').hidden = !API;
  drawer.classList.add('on');
  paintSeen();
  if (target) { target.scrollIntoView({ block: 'center' }); }
}
$('#dseen').onclick = () => { if (drawer.dataset.path) toggleSeen(drawer.dataset.path); };
window.addEventListener('keydown', e => { if (e.key === 'v' && drawer.classList.contains('on') && drawer.dataset.path) toggleSeen(drawer.dataset.path); });
$('#ghpush').onclick = async () => { $('#ghstat').textContent = 'marking…'; try { const r = await (await fetch('/api/viewed/push', { method: 'POST' })).json(); $('#ghstat').textContent = r.error ? ('GitHub: ' + r.error) : `marked ${r.marked}/${r.total} done file(s) Viewed on PR #${r.pr}`; } catch (e) { $('#ghstat').textContent = 'GitHub: request failed'; } };
$('#ghpull').onclick = async () => { $('#ghstat').textContent = 'reading…'; try { const r = await (await fetch('/api/viewed/pull', { method: 'POST' })).json(); if (r.error) { $('#ghstat').textContent = 'GitHub: ' + r.error; } else { await loadSeen(); $('#ghstat').textContent = `PR #${r.pr}: ${r.viewed} Viewed on GitHub · ${r.imported} imported`; } } catch (e) { $('#ghstat').textContent = 'GitHub: request failed'; } };
$('#dclose').onclick = () => drawer.classList.remove('on');
window.addEventListener('keydown', e => { if (e.key === 'Escape') drawer.classList.remove('on'); });
drawer.addEventListener('mousedown', e => e.stopPropagation());
drawer.addEventListener('wheel', e => e.stopPropagation(), { passive: true });

function showPanel() {
  const n = view.nodes.find(n => n.id === selected);
  if (!n) { panel.className = ''; panel.innerHTML = ''; return; }
  const callers = view.edges.filter(e => e.to === n.id).map(e => view.nodes.find(x => x.id === e.from)).filter(Boolean);
  const callees = view.edges.filter(e => e.from === n.id).map(e => view.nodes.find(x => x.id === e.to)).filter(Boolean);
  const item = m => `<li data-id="${m.id.replace(/"/g, '&quot;')}">${(m.route || m.label).replace(/</g, '&lt;')}${m.pkg !== n.pkg ? ` <span class="sub">· ${m.pkg}</span>` : ''}</li>`;
  const mem = view.members[n.id] || [];
  const hiddenIn = new Set(M.edges.filter(e => mem.includes(e.to) && !shown(byId[e.from])).map(e => e.from)).size;
  const hiddenOut = new Set(M.edges.filter(e => mem.includes(e.from) && !shown(byId[e.to])).map(e => e.to)).size;
  const canOpen = !n.agg && n.path && M.files[n.path];
  const noDiff = !n.agg && n.path && !M.files[n.path];
  panel.innerHTML = `<h3>${(n.route || n.label).replace(/</g, '&lt;')}</h3><div class="meta">${n.pkg}${n.file ? ' · ' + n.file : ''}${n.line ? ' · L' + n.line : ''} · ${n.status}${n.agg ? ` · ${n.sub}` : ''}${canOpen ? ' · <a href="#" id="openfile" style="color:var(--accent)">open the file ↗</a>' : noDiff ? ' · unchanged file, not embedded' : ''}</div>
    <div class="k">called by (${callers.length})</div><ul>${callers.map(item).join('') || '<li style="cursor:default;color:var(--muted)">nothing on the canvas</li>'}${hiddenIn ? `<li id="more-in" style="color:var(--accent)">+ ${hiddenIn} more beyond the shown depth — show</li>` : ''}</ul>
    <div class="k">calls (${callees.length})</div><ul>${callees.map(item).join('') || '<li style="cursor:default;color:var(--muted)">nothing on the canvas</li>'}${hiddenOut ? `<li id="more-out" style="color:var(--accent)">+ ${hiddenOut} more beyond the shown depth — show</li>` : ''}</ul>`;
  panel.className = 'on';
  panel.querySelectorAll('li[data-id]').forEach(li => li.onclick = () => { selected = li.dataset.id; highlight(); });
  const o = $('#openfile'); if (o) o.onclick = e => { e.preventDefault(); openFile(n); };
  for (const id of ['more-in', 'more-out']) { const el = $('#' + id); if (el) el.onclick = () => { setDepth(DEPTHS[Math.min(DEPTHS.length - 1, DEPTHS.indexOf(depth) + 1)]); }; }
}

// ---- pan & zoom ----
let scale = 1, tx = 0, ty = 0, drag = null;
function apply() { board.style.transform = `translate(${tx}px,${ty}px) scale(${scale})`; }
stage.addEventListener('mousedown', e => { if (e.target.closest('#panel')) return; drag = { x: e.clientX, y: e.clientY, tx, ty, moved: false }; stage.classList.add('dragging'); });
window.addEventListener('mousemove', e => { if (!drag) return; const dx = e.clientX - drag.x, dy = e.clientY - drag.y; if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true; tx = drag.tx + dx; ty = drag.ty + dy; apply(); });
window.addEventListener('mouseup', e => { if (drag && !drag.moved && !e.target.closest('.node') && !e.target.closest('#panel') && !e.target.closest('.lod')) { selected = null; highlight(); } drag = null; stage.classList.remove('dragging'); });
stage.addEventListener('wheel', e => {
  if (e.ctrlKey || e.metaKey) {
    e.preventDefault();
    // Zoom by how far the wheel moved, not per event: a trackpad pinch
    // fires dozens of small-delta events a second and a fixed factor
    // per event ran away; a mouse notch (~100px, clamped) still steps
    // about 10% like before.
    const d = Math.max(-25, Math.min(25, e.deltaMode === 1 ? e.deltaY * 20 : e.deltaY));
    const r = stage.getBoundingClientRect(); const mx = e.clientX - r.left, my = e.clientY - r.top; const ns = Math.min(3, Math.max(.25, scale * Math.exp(-d * 0.004))); tx = mx - (mx - tx) * ns / scale; ty = my - (my - ty) * ns / scale; scale = ns; apply(); }
  else { tx -= e.deltaX; ty -= e.deltaY; apply(); e.preventDefault(); }
}, { passive: false });
window.addEventListener('resize', drawEdges);
$('#fit-changes').onclick = fitChanges; $('#fit-all').onclick = fitAll; $('#fit-1').onclick = () => { scale = 1; apply(); };
function setDepth(d) { depth = d; renderDepth(); render(); }
function renderDepth() {
  const box = $('#depth'); box.innerHTML = '<span style="font-family:var(--mono);font-size:10.5px;color:var(--muted);padding:3px 6px">hops from the change</span>';
  for (const d of DEPTHS) { const b = document.createElement('button'); b.textContent = d === Infinity ? 'all' : d; if (d === depth) b.className = 'on'; b.onclick = () => setDepth(d); box.appendChild(b); }
}
renderDepth();
render();
fitChanges();
loadSeen();
document.fonts && document.fonts.ready.then(() => { drawEdges(); fitChanges(); });
</script></body></html>
"##;
