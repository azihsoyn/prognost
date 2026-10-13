//! The graph view: start at one node, expand its callers and/or callees
//! on request, keep expanding — the picture grows outward instead of
//! being replaced each time, so "how far does this reach" stays visible
//! at a glance instead of one hop at a time. Nothing is expanded that
//! wasn't asked for; there is still no "whole graph" drawn up front.
//!
//! Three granularities share this one renderer: function (this file's
//! call graph, from `hub.rs`), file (the workspace import graph, from
//! `file_hub.rs`, built on reachhop's original coarse layer), and
//! package (package.json dependencies, from `package_hub.rs`). `g`
//! cycles function → file → package → function, re-rooting the graph at
//! whatever the finer/coarser level's equivalent of the current node is.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{
    Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget, Widget,
};
use ratatui::{DefaultTerminal, Frame};

use crate::align::{self, Alignment};
use crate::file_hub;
use crate::graph::Direction as ImportDirection;
use crate::hub::{self, Status};
use crate::lang::Lang;
use crate::package_hub;
use crate::rev::Rev;
use crate::ts_extract::TsFunction;
use crate::workspace::Workspace;

/// Columns size themselves to their longest label, within these bounds
/// — one fixed width either truncates every route and domain-method
/// name into "db.orderFulfillmentSettingsDom…" or wastes half the screen
/// on short ones.
const MIN_COL_WIDTH: u16 = 26;
const MAX_COL_WIDTH: u16 = 110;
/// World row 0 holds the column headers, pinned to the top of the
/// diagram while everything under it scrolls.
const HEADER_ROW: i32 = 1;
const KEY_HINT: &str = "↑↓←→ move · shift+↑↓←→ / wheel / drag pan · click select · z/Z package detail · v seen · S/P Viewed ↔ GitHub · m map · a unchanged · c collapse · g level · enter diff · o edit · q quit";
const PAN_STEP_X: i32 = 10;
const PAN_STEP_Y: i32 = 5;
const WHEEL_STEP_X: i32 = 6;
const WHEEL_STEP_Y: i32 = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Granularity {
    Function,
    File,
    Package,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Dir {
    Callers,
    Callees,
}

/// One hop's worth of a node: its id, display label, diff status, and
/// whether it resolved to something real enough to expand further.
type Fragment = (String, String, Status, bool);

/// One box on screen. `id` is what re-queries this node (a function
/// label, a real file path, a package directory — resolved once, at
/// expansion time, from whatever short display name the hub builder
/// handed back).
struct GNode {
    id: String,
    display: String,
    status: Status,
    /// False for a callee that couldn't be resolved to a real function —
    /// `db.transaction(...)`, say, a method on an imported object rather
    /// than a bare function call, which the same name-matching that
    /// resolves a caller across files can't follow. Still worth showing
    /// (the call is real; only its target is out of reach), just not
    /// something Enter/arrows can do anything more with.
    drillable: bool,
}

/// A route handler's place in the API: method, full path segments
/// (mount prefixes included), and the file and function it lives in.
#[derive(Clone)]
struct RouteEntry {
    method: String,
    segments: Vec<String>,
    file: PathBuf,
    label: String,
}

struct RouteTable {
    entries: Vec<RouteEntry>,
    /// Mount prefix per router file: `index.ts` → [], OrderController
    /// → ["api", "v1", "folders"].
    mount_paths: HashMap<PathBuf, Vec<String>>,
}

/// One Hono RPC call in the source: what it calls, where.
#[derive(Clone)]
struct RpcSite {
    call: crate::hono::RpcCall,
    file: PathBuf,
    line: u32,
}

/// One pass over a revision's source: every RPC call site and every
/// `.route('/prefix', Binding)` mount, so both sides of the API seam
/// come from a single read of each file.
#[derive(Clone, Default)]
struct HonoScan {
    sites: Vec<RpcSite>,
    mounts: Vec<(PathBuf, String, String)>,
    /// (seam index, definition) and (seam index, call site) for the
    /// repository's own seams.
    seam_defs: Vec<(usize, crate::seam::Def)>,
    seam_calls: Vec<(usize, crate::seam::CallSite)>,
    /// `export * from` / `export { … } from` in each file: (file, specifier).
    reexports: Vec<(PathBuf, String)>,
}

/// Where the minimap was drawn and at what scale: one of its cells is
/// `scale_x` × `scale_y` world cells.
#[derive(Clone, Copy)]
struct Minimap {
    inner: Rect,
    scale_x: i32,
    scale_y: i32,
}

const MINIMAP_W: u16 = 44;
const MINIMAP_H: u16 = 14;

/// One file's parsed functions, cached the first time a function-level
/// node from it is seen. The graph isn't confined to the origin file's
/// own call list: walking upstream past its outermost export crosses
/// into whatever file imports it, so more than one file's functions can
/// be live at once.
struct FunctionFileEntry {
    base_fns: Vec<TsFunction>,
    head_fns: Vec<TsFunction>,
    alignment: Vec<Alignment>,
}

pub struct App {
    root: PathBuf,
    base_rev: Rev,
    head_rev: Rev,
    base_ws: Workspace,
    head_ws: Workspace,

    granularity: Granularity,

    function_files: HashMap<PathBuf, FunctionFileEntry>,
    /// The files importing a file (through its barrels too), both
    /// revisions, tests left out: asked once per *function* of that file
    /// on an upstream walk, and the same for all of them.
    importers_cache: HashMap<PathBuf, Vec<String>>,
    go_index: Option<GoIndex>,
    /// The repository's external resolvers, and what each reported, by
    /// (head?, language).
    resolvers: Vec<crate::resolver::Resolver>,
    external: HashMap<(bool, Lang), std::rc::Rc<crate::resolver::Calls>>,
    /// (calling file, function id) reached only through an interface.
    inferred_calls: HashSet<(PathBuf, String)>,
    /// Edges (caller, callee) that are such calls: drawn as guesses.
    inferred_edges: HashSet<(String, String)>,

    nodes: Vec<GNode>,
    /// Every call relation on the canvas, caller → callee, with the
    /// status of the *call* (added/removed) where that's known. A node
    /// can have any number of callers and callees: three route handlers
    /// converging on one domain function is the picture, not a tree.
    edges: HashMap<(String, String), Status>,
    /// How each node first got onto the canvas — which node's expansion
    /// revealed it, in which direction. Drives collapse, visibility of
    /// unchanged callees, and where an unresolved call is opened from.
    expanded_from: HashMap<String, (String, Dir)>,
    /// The node ←/→ was last pressed on, so stepping back along an edge
    /// retraces the step rather than picking some other neighbour.
    came_from: Option<String>,
    /// Files each file imports (both revisions), for resolving a bare
    /// call to the export it names; computed once per file.
    deps_cache: HashMap<PathBuf, Vec<PathBuf>>,
    /// Import bindings and `export * as` re-exports per file, both
    /// revisions merged (HEAD wins), read once.
    bindings_cache: HashMap<PathBuf, HashMap<String, crate::bindings::Import>>,
    reexports_cache: HashMap<PathBuf, HashMap<String, String>>,
    /// Specifier resolution memo: (from directory, specifier) → file.
    spec_cache: HashMap<(PathBuf, String), Option<PathBuf>>,
    /// For a `pkg::` node whose name was traced to a file that exports
    /// it without defining a function by that name (a domain assembled
    /// by a factory, say): that file and the line the name appears on,
    /// so the node still has a place in a box and something for Enter.
    call_file: HashMap<String, (PathBuf, u32)>,
    /// Every Hono route handler in the workspace with its full mounted
    /// path, built once on first need (see [`App::route_table`]).
    routes: Option<RouteTable>,
    /// Every Hono RPC call site and router mount in the workspace, per
    /// revision — and every definition / call site of the repository's
    /// own seams (`prognost.toml`).
    hono_head: Option<HonoScan>,
    hono_base: Option<HonoScan>,
    seams: Vec<crate::seam::StringKeySeam>,
    /// The call text a `pkg::` node was made from, per node — its
    /// package-relative label drops the binding (`db.`), but finding the
    /// call site again needs the text as written.
    call_text: HashMap<String, String>,
    /// A callee's position in the order its caller actually calls
    /// things, recorded when it was added — the source of truth for
    /// procedural ordering, since a callee's own definition line (used
    /// for everything else) says nothing about when it's invoked, and an
    /// unresolved "[external]" call has no definition line at all.
    call_order: HashMap<String, usize>,
    /// Nodes whose callers were looked for and none found — drawn as a
    /// "no caller found" note in the column ← would have filled, so an
    /// empty left side reads as an answer, not as "not expanded yet".
    no_callers: HashSet<String>,
    /// Every node whose callers have been looked for, found or not.
    callers_sought: HashSet<String>,
    /// The roots of the graph, all at layer 0. One when launched from a
    /// hand-picked entrypoint; one per outermost changed function when
    /// launched from a whole diff, grouped by file in the leftmost
    /// column so the picture reads as "what this change touched".
    origins: Vec<String>,

    selected: String,
    status: String,
    diff_view: Option<DiffView>,
    /// Whether unchanged callees are drawn. Off by default: a route
    /// handler's `c.json`, `resolveAuthContext`, `Promise.all` are real
    /// calls, but nothing about the change reaches them, and listing
    /// them buries the ones it does reach. Callers are never hidden —
    /// everything upstream is, by definition, affected. Toggled with `a`.
    show_unchanged: bool,
    /// The graph is drawn on an unbounded canvas and this is the window
    /// onto it, in cells: the world column at the left edge of the
    /// diagram, and the world row (below the pinned column headers) at
    /// its top. Independent of the selection — the cursor stays where it
    /// is while the view pans — except that a *keyboard* move re-aims
    /// the camera at the selected node (`follow`) so the cursor is never
    /// off-screen after ↑/↓/←/→.
    camera: (i32, i32),
    follow: bool,
    /// The minimap in the corner: shown while the graph is bigger than
    /// the window (toggle `m`); its inner rect and the world cells one
    /// of its cells stands for, recomputed every frame for clicks.
    show_minimap: bool,
    minimap: Option<Minimap>,
    /// Detail level per package box: function (default), file,
    /// directory, or the whole package as one node. `z` coarsens the
    /// selected node's package, `Z` refines it.
    lod: HashMap<String, u8>,
    /// "I have looked at this" per file, shared with slidiff; `v`
    /// toggles the selected node's file, `S` mirrors done files to the
    /// PR's Viewed checkboxes, `P` pulls them back.
    seen: Option<crate::seen::SeenStore>,
    /// (seen, total, done) per repo path, refreshed after every toggle,
    /// so the layout can label file headers without touching git.
    seen_progress: HashMap<String, (usize, usize, bool)>,
    /// The pull request the head belongs to, looked up on first use;
    /// `--pr` pins the number.
    pr: Option<crate::github::PullRequest>,
    pub pr_number: Option<u64>,
    /// Where the diagram was drawn last frame, for mouse hit-testing.
    canvas: Rect,
    /// A left-button press in progress: where it started and whether it
    /// has moved yet — a press that never moves is a click (select), one
    /// that does is a drag (pan).
    drag: Option<(u16, u16, bool)>,
    quit: bool,
    /// `o` asked for this file at this line in an editor; the run loop
    /// opens it, leaving the screen to the editor while it runs.
    edit_request: Option<(PathBuf, u32)>,
}

/// A computed unified diff, shown full-screen in place of the graph.
/// Built once when Enter is pressed rather than reading the files again
/// on every redraw.
struct DiffView {
    /// What the diff is scoped to and where — the selected node's own
    /// label plus the file, so the view answers "diff of what" on its
    /// own without the reader having to remember what was selected.
    title: String,
    lines: Vec<DiffLine>,
    scroll: u16,
    /// " ✓" or " 3/12 seen" for the header.
    seen_note: String,
}

/// One row of the file view: the *whole* file, every line, with the
/// changed ones marked — not a unified diff's hunks. A reviewer asked
/// for the full file, with the change visible inside it, and a hunk
/// view (changes plus three lines of context) is precisely "only the
/// diff" no matter how it's scrolled.
struct DiffLine {
    text: String,
    style: Style,
    /// This line's number in HEAD — `None` for a removed line, which
    /// only exists in BASE. What `scroll_to_line` positions on.
    head_line: Option<u32>,
    /// Added or removed, as opposed to context.
    changed: bool,
}

/// Everything `App::new` needs, grouped so the constructor takes one
/// argument instead of a dozen positional ones a caller could transpose.
pub struct Init {
    pub root: PathBuf,
    pub file: PathBuf,
    pub base_rev: Rev,
    pub head_rev: Rev,
    pub base_ws: Workspace,
    pub head_ws: Workspace,
    pub base_fns: Vec<TsFunction>,
    pub head_fns: Vec<TsFunction>,
    pub alignment: Vec<Alignment>,
    pub focus: String,
}

impl App {
    pub fn new(init: Init) -> Option<Self> {
        let origin_status =
            hub::build(&init.base_fns, &init.head_fns, &init.alignment, &init.focus)?
                .focus
                .status;
        let origin_display = describe_function_label(&init.head_fns, &init.base_fns, &init.focus);
        let origin_id = make_fn_id(&init.file, &init.focus);
        let mut function_files = HashMap::new();
        function_files.insert(
            init.file.clone(),
            FunctionFileEntry {
                base_fns: init.base_fns,
                head_fns: init.head_fns,
                alignment: init.alignment,
            },
        );
        let mut app = Self {
            root: init.root,
            base_rev: init.base_rev,
            head_rev: init.head_rev,
            base_ws: init.base_ws,
            head_ws: init.head_ws,
            granularity: Granularity::Function,
            function_files,
            importers_cache: HashMap::new(),
            go_index: None,
            resolvers: Vec::new(),
            external: HashMap::new(),
            inferred_calls: HashSet::new(),
            inferred_edges: HashSet::new(),
            nodes: vec![GNode {
                id: origin_id.clone(),
                display: origin_display,
                status: origin_status,
                drillable: true,
            }],
            edges: HashMap::new(),
            expanded_from: HashMap::new(),
            came_from: None,
            deps_cache: HashMap::new(),
            bindings_cache: HashMap::new(),
            reexports_cache: HashMap::new(),
            spec_cache: HashMap::new(),
            call_file: HashMap::new(),
            routes: None,
            hono_head: None,
            hono_base: None,
            seams: Vec::new(),
            call_text: HashMap::new(),
            call_order: HashMap::new(),
            no_callers: HashSet::new(),
            callers_sought: HashSet::new(),
            origins: vec![origin_id.clone()],
            selected: origin_id.clone(),
            status:
                "↑/↓ move · ←/→ step to a caller/callee · c collapse · g zoom · enter diff · q quit"
                    .into(),
            diff_view: None,
            show_unchanged: false,
            camera: (0, 0),
            follow: true,
            show_minimap: true,
            minimap: None,
            lod: HashMap::new(),
            seen: None,
            seen_progress: HashMap::new(),
            pr: None,
            pr_number: None,
            canvas: Rect::default(),
            drag: None,
            quit: false,
            edit_request: None,
        };
        // A single node tells a reviewer nothing about reach; expand one
        // hop both ways immediately so there is something to look at
        // without a keypress, same as the concept preview showed.
        app.expand(origin_id.clone(), Dir::Callers);
        app.expand(origin_id, Dir::Callees);
        Some(app)
    }

    /// One graph for a whole change set: every function whose body
    /// differs between the two revisions, across every changed file,
    /// each its own root in the leftmost column — instead of one
    /// hand-picked entrypoint. Only the outermost changed function per
    /// nesting chain becomes a root: an anonymous handler's body hash
    /// changes whenever a callback inside it does, and that inner one is
    /// one → away, already marked changed there. Test files are left
    /// out — they're where a change is asserted, not where it flows.
    /// Nothing is pre-expanded: the roots alone already say what the
    /// change touched, and each is one keypress from its neighbours.
    pub fn from_changes(
        root: PathBuf,
        base_rev: Rev,
        head_rev: Rev,
        base_ws: Workspace,
        head_ws: Workspace,
    ) -> Result<Self> {
        let files = crate::origin::changed_files_between(&root, &base_rev, &head_rev)?;
        let mut app = Self {
            root,
            base_rev,
            head_rev,
            base_ws,
            head_ws,
            granularity: Granularity::Function,
            function_files: HashMap::new(),
            importers_cache: HashMap::new(),
            go_index: None,
            resolvers: Vec::new(),
            external: HashMap::new(),
            inferred_calls: HashSet::new(),
            inferred_edges: HashSet::new(),
            nodes: Vec::new(),
            edges: HashMap::new(),
            expanded_from: HashMap::new(),
            came_from: None,
            deps_cache: HashMap::new(),
            bindings_cache: HashMap::new(),
            reexports_cache: HashMap::new(),
            spec_cache: HashMap::new(),
            call_file: HashMap::new(),
            routes: None,
            hono_head: None,
            hono_base: None,
            seams: Vec::new(),
            call_text: HashMap::new(),
            call_order: HashMap::new(),
            no_callers: HashSet::new(),
            callers_sought: HashSet::new(),
            origins: Vec::new(),
            selected: String::new(),
            status: String::new(),
            diff_view: None,
            show_unchanged: false,
            camera: (0, 0),
            follow: true,
            show_minimap: true,
            minimap: None,
            lod: HashMap::new(),
            seen: None,
            seen_progress: HashMap::new(),
            pr: None,
            pr_number: None,
            canvas: Rect::default(),
            drag: None,
            quit: false,
            edit_request: None,
        };
        app.seams = crate::seam::load(&app.root)?;
        app.resolvers = crate::resolver::load(&app.root)?;
        let mut files_with_roots = 0;
        for file in files.iter().filter(|f| !is_test_file(f)) {
            if !app.ensure_function_file(file) {
                continue;
            }
            let roots: Vec<(String, String, Status)> = {
                let entry = app.function_files.get(file).unwrap();
                let mut roots = changed_roots(entry);
                roots.sort_by_key(|r| r.0);
                roots
                    .into_iter()
                    .map(|(_, label, status)| {
                        let display =
                            describe_function_label(&entry.head_fns, &entry.base_fns, &label);
                        (make_fn_id(file, &label), display, status)
                    })
                    .collect()
            };
            if !roots.is_empty() {
                files_with_roots += 1;
            }
            for (id, display, status) in roots {
                app.nodes.push(GNode {
                    id: id.clone(),
                    display,
                    status,
                    drillable: true,
                });
                app.origins.push(id);
            }
        }
        if app.origins.is_empty() {
            anyhow::bail!(
                "no changed functions between {} and {}",
                app.base_rev.label(),
                app.head_rev.label()
            );
        }
        app.selected = app.origins[0].clone();
        // The whole tree, not just the trunk: every root's callers and
        // callees one hop out from the start, so "who calls this / what
        // does it reach" is on screen without ←/→ — and a root with
        // nothing to its left visibly has no known caller, instead of ←
        // simply doing nothing. A changed function that another changed
        // function calls simply lands one column further right, joined
        // by its edge.
        for id in app.origins.clone() {
            let t = std::time::Instant::now();
            app.expand(id.clone(), Dir::Callers);
            let t1 = t.elapsed();
            app.expand(id.clone(), Dir::Callees);
            debug_log(&format!(
                "expand {id}: callers {:?}, callees {:?}",
                t1,
                t.elapsed() - t1
            ));
        }
        app.refresh_displays();
        // Start on the top-left changed function, so the first frame
        // shows the canvas from its left edge rather than scrolled to
        // wherever the first file's function happened to land.
        app.select_top_left();
        app.open_seen();
        app.status = format!(
            "{} changed function(s) in {} file(s), callers and callees one hop out — ←/→ to go further",
            app.origins.len(),
            files_with_roots
        );
        Ok(app)
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
        let mut result = Ok(());
        while !self.quit {
            if let Err(e) = terminal.draw(|f| {
                self.draw(f);
                if !crate::color::enabled() {
                    crate::color::strip_buffer(f.buffer_mut());
                }
            }) {
                result = Err(e.into());
                break;
            }
            match event::read() {
                Ok(ev) => self.handle_event(ev),
                Err(e) => {
                    result = Err(e.into());
                    break;
                }
            }
            if let Some((path, line)) = self.edit_request.take() {
                self.open_in_editor(terminal, &path, line);
            }
        }
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
        result
    }

    /// `o`: the selected node's file at its line in `$VISUAL`/`$EDITOR`.
    /// Inside herdr it opens in a pane beside this one and the graph
    /// stays on screen; elsewhere the editor takes this terminal until it
    /// exits, and the graph comes back.
    fn open_in_editor(
        &mut self,
        terminal: &mut DefaultTerminal,
        path: &std::path::Path,
        line: u32,
    ) {
        let command = crate::herdr::editor_line(path, line);
        if crate::herdr::inside() {
            self.status =
                match crate::herdr::open_beside(&self.root, &command, "prognost edit", true) {
                    Ok(pane) => format!("opened {} in herdr pane {pane}", path.display()),
                    Err(e) => format!("herdr: {e}"),
                };
            return;
        }
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
        ratatui::restore();
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .current_dir(&self.root)
            .status();
        *terminal = ratatui::init();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
        let _ = terminal.clear();
        self.status = match status {
            Ok(s) if s.success() => format!("back from {}", path.display()),
            Ok(s) => format!("editor exited with {s}"),
            Err(e) => format!("could not run the editor: {e}"),
        };
    }

    /// Parses and aligns a file's functions the first time anything from
    /// it is needed — walking upstream from the origin file into
    /// whatever imports it means the set of files in play isn't known
    /// up front.
    fn ensure_function_file(&mut self, file: &std::path::Path) -> bool {
        if self.function_files.contains_key(file) {
            return true;
        }
        let Some((base_fns, head_fns, alignment)) =
            parse_functions(&self.root, &self.base_rev, &self.head_rev, file)
        else {
            return false;
        };
        self.function_files.insert(
            file.to_path_buf(),
            FunctionFileEntry {
                base_fns,
                head_fns,
                alignment,
            },
        );
        true
    }

    fn hub_for(&mut self, id: &str) -> Option<hub::Hub> {
        match self.granularity {
            Granularity::Function => {
                let (file, label) = split_fn_id(id);
                self.ensure_function_file(&file);
                let entry = self.function_files.get(&file)?;
                hub::build(&entry.base_fns, &entry.head_fns, &entry.alignment, label)
            }
            Granularity::File => file_hub::build(
                &self.root,
                &self.base_rev,
                &self.head_rev,
                &self.base_ws,
                &self.head_ws,
                std::path::Path::new(id),
            ),
            Granularity::Package => {
                package_hub::build(&self.base_ws, &self.head_ws, std::path::Path::new(id))
            }
        }
    }

    /// Whether `label` in `file` is visible to another file at all —
    /// only a top-level, exported binding can be imported, so only those
    /// are worth following into the files that import `file`.
    ///
    /// In Go every top-level function and method is reachable from the
    /// package's other files; in Python every top-level `def` can be
    /// imported, underscore or not, and a class's methods are called
    /// wherever an instance of it goes.
    fn is_importable(&self, file: &std::path::Path, label: &str) -> bool {
        let visible: fn(&TsFunction) -> bool = match Lang::of(file) {
            Some(Lang::Go) => |f| f.parent.is_none(),
            Some(Lang::Python) => |f| f.parent.is_none(),
            _ => |f| f.parent.is_none() && f.exported,
        };
        self.function_files.get(file).is_some_and(|entry| {
            entry
                .head_fns
                .iter()
                .any(|f| align::label(f) == label && visible(f))
                || entry
                    .base_fns
                    .iter()
                    .enumerate()
                    .any(|(i, f)| base_label(entry, i) == label && visible(f))
        })
    }

    /// Every function-level id this node could reasonably be reached
    /// from, going one hop further in `dir`: real callers/callees from
    /// the in-file hub, plus — for `Dir::Callers` — two things the hub
    /// can't see because it only looks within one file: the function
    /// `label` is declared inside (so an anonymous callback's chain can
    /// be walked all the way out to a real top-level export), and, once
    /// that export is reached, whichever files import it.
    fn function_fragment(
        &mut self,
        file: &std::path::Path,
        label: &str,
        dir: Dir,
    ) -> Vec<Fragment> {
        let file = file.to_path_buf();
        if !self.ensure_function_file(&file) {
            return Vec::new();
        }
        let entry = self.function_files.get(&file).unwrap();
        let Some(h) = hub::build(&entry.base_fns, &entry.head_fns, &entry.alignment, label) else {
            return Vec::new();
        };
        let in_file = match dir {
            Dir::Callers => h.callers,
            Dir::Callees => h.callees,
        };
        // A caller is always a real function when the hub reports one at
        // all, so a non-drillable caller means it no longer exists to
        // walk into — drop it. A callee can name something real that
        // just isn't *resolvable* (`db.transaction`, a method on an
        // imported object rather than a bare function call this tool
        // can trace) — that one is still worth showing, greyed out,
        // rather than silently vanishing.
        let mut out: Vec<Fragment> = in_file
            .into_iter()
            .filter(|n| dir == Dir::Callees || n.drillable)
            .map(|n| {
                (
                    make_fn_id(&file, &n.label),
                    n.label.clone(),
                    n.status,
                    n.drillable,
                )
            })
            .collect();

        if dir == Dir::Callees {
            // Downstream of a function is what its *code* calls, and an
            // anonymous callback — `db.transaction(..., async (client)
            // => {...})` — is that code, not a hop: a reviewer reading
            // "API → DB" doesn't want the transaction's own closure as a
            // level between them, and "a change inside the same
            // function shows up one column deeper" read exactly that
            // way. So the calls of every anonymous function nested in
            // this one are folded into it, in source order; a *named*
            // nested function keeps its own node, since it is a thing
            // that can be called from elsewhere.
            let entry = self.function_files.get(&file).unwrap();
            let Some(head_idx) = entry.head_fns.iter().position(|f| align::label(f) == label)
            else {
                // Gone from HEAD: the hub's view (everything removed) is
                // all there is to say.
                return out;
            };
            let head_calls = inlined_calls(&entry.head_fns, head_idx);
            let base_idx = base_counterpart(entry, head_idx);
            let base_calls = base_idx
                .map(|b| inlined_calls(&entry.base_fns, b))
                .unwrap_or_default();
            let head_range = (
                entry.head_fns[head_idx].start_line,
                entry.head_fns[head_idx].end_line,
            );
            let base_range =
                base_idx.map(|b| (entry.base_fns[b].start_line, entry.base_fns[b].end_line));
            // The repository's own seams: a call site inside this
            // function whose key a definition somewhere carries joins
            // straight to the function around that definition, and the
            // raw call text on that line is not shown on its own.
            let (seam_callees, seam_lines, base_seam_lines) =
                self.seam_callees(&file, head_range, base_range);
            let entry = self.function_files.get(&file).unwrap();
            let mut folded: Vec<Fragment> = Vec::new();
            let mut seen: HashSet<&str> = HashSet::new();
            for (c, line) in &head_calls {
                if seam_lines.contains(line)
                    && !entry.head_fns.iter().any(|f| &align::label(f) == c)
                {
                    continue;
                }
                if !seen.insert(c) {
                    continue;
                }
                let status = if base_calls.iter().any(|(b, _)| b == c) {
                    Status::Unchanged
                } else {
                    Status::Added
                };
                let drillable = entry.head_fns.iter().any(|f| &align::label(f) == c);
                // This is the status of the *call*; the callee's own
                // body status is worked out when the node is made.
                debug_log(&format!(
                    "call {label} -> {c}: {status:?} (base counterpart: {:?}, base calls: {})",
                    base_counterpart(entry, head_idx),
                    base_calls.len()
                ));
                folded.push((make_fn_id(&file, c), c.clone(), status, drillable));
            }
            for (c, line) in &base_calls {
                if base_seam_lines.contains(line) {
                    continue;
                }
                if seen.insert(c) {
                    folded.push((make_fn_id(&file, c), c.clone(), Status::Removed, false));
                }
            }
            folded.extend(seam_callees);
            let mut named = Vec::new();
            named_nested(&entry.head_fns, head_idx, &mut named);
            for i in named {
                let child_label = align::label(&entry.head_fns[i]);
                if seen.contains(child_label.as_str()) {
                    continue;
                }
                let status = hub::build(
                    &entry.base_fns,
                    &entry.head_fns,
                    &entry.alignment,
                    &child_label,
                )
                .map(|h| h.focus.status)
                .unwrap_or(Status::Unchanged);
                folded.push((make_fn_id(&file, &child_label), child_label, status, true));
            }
            return folded;
        }

        let entry = self.function_files.get(&file).unwrap();
        let hit = find_function(entry, label);
        let route = hit.and_then(|(_, f)| f.route.clone());
        let range = hit.map(|(_, f)| (f.start_line, f.end_line));
        // The function this one is declared inside — the nearest *named*
        // one: a callback nested three closures deep is reached in one
        // step, not three.
        if let Some((fns, f)) = hit
            && let Some(parent_idx) = f.parent
        {
            let parent_idx = named_enclosing(fns, parent_idx);
            let parent_label = if std::ptr::eq(fns, entry.base_fns.as_slice()) {
                base_label(entry, parent_idx)
            } else {
                align::label(&fns[parent_idx])
            };
            let status = status_of_label(entry, &parent_label);
            out.push((make_fn_id(&file, &parent_label), parent_label, status, true));
        }

        if self.is_importable(&file, label) {
            out.extend(self.cross_file_callers(&file, label));
        }
        // A route handler's callers are the frontend functions that hit
        // its method and path through the RPC client.
        if let Some(route) = route {
            out.extend(self.rpc_callers(&file, &route));
        }
        // The repository's own seams: a definition inside this function
        // is called from wherever its key is used.
        if let Some(range) = range {
            out.extend(self.seam_callers(&file, range));
        }
        out
    }

    /// Definitions the seam call sites inside `[head_range]` (and the
    /// base counterpart's) point at, as callee fragments, plus the
    /// lines those call sites sit on.
    fn seam_callees(
        &mut self,
        file: &std::path::Path,
        head_range: (u32, u32),
        base_range: Option<(u32, u32)>,
    ) -> (Vec<Fragment>, HashSet<u32>, HashSet<u32>) {
        if self.seams.is_empty() {
            return (Vec::new(), HashSet::new(), HashSet::new());
        }
        use crate::seam::Seam;
        let head = self.hono_scan(true);
        let base = self.hono_scan(false);
        let in_range = |c: &crate::seam::CallSite, f: &std::path::Path, r: (u32, u32)| {
            c.file == f && c.line >= r.0 && c.line <= r.1
        };
        let head_keys: Vec<(usize, String, u32)> = head
            .seam_calls
            .iter()
            .filter(|(_, c)| in_range(c, file, head_range))
            .map(|(i, c)| (*i, c.key.clone(), c.line))
            .collect();
        let base_keys: Vec<(usize, String, u32)> = match base_range {
            Some(r) => base
                .seam_calls
                .iter()
                .filter(|(_, c)| in_range(c, file, r))
                .map(|(i, c)| (*i, c.key.clone(), c.line))
                .collect(),
            None => Vec::new(),
        };
        let seam_lines: HashSet<u32> = head_keys.iter().map(|(_, _, l)| *l).collect();
        let base_seam_lines: HashSet<u32> = base_keys.iter().map(|(_, _, l)| *l).collect();
        let mut out: Vec<Fragment> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut resolve =
            |this: &mut Self, i: usize, key: &str, status: Status, out: &mut Vec<Fragment>| {
                let defs: Vec<crate::seam::Def> = head
                    .seam_defs
                    .iter()
                    .chain(base.seam_defs.iter())
                    .filter(|(j, d)| *j == i && this.seams[i].same_key(&d.key, key))
                    .map(|(_, d)| d.clone())
                    .collect();
                for d in defs {
                    if !this.ensure_function_file(&d.file) {
                        continue;
                    }
                    let entry = &this.function_files[&d.file];
                    let label = innermost_at(&entry.head_fns, d.line)
                        .map(|k| align::label(&entry.head_fns[named_enclosing(&entry.head_fns, k)]))
                        .or_else(|| {
                            innermost_at(&entry.base_fns, d.line)
                                .map(|k| base_label(entry, named_enclosing(&entry.base_fns, k)))
                        })
                        .unwrap_or_else(|| format!("module@{}", d.line));
                    let id = make_fn_id(&d.file, &label);
                    if seen.insert(id.clone()) {
                        debug_log(&format!("seam {} : {key} -> {id}", this.seams[i].name));
                        let drillable = !label.starts_with("module@");
                        out.push((id, label, status, drillable));
                    }
                }
            };
        for (i, key, _) in &head_keys {
            let status = if base_keys
                .iter()
                .any(|(j, k, _)| j == i && self.seams[*i].same_key(k, key))
            {
                Status::Unchanged
            } else {
                Status::Added
            };
            resolve(self, *i, key, status, &mut out);
        }
        for (i, key, _) in &base_keys {
            if !head_keys
                .iter()
                .any(|(j, k, _)| j == i && self.seams[*i].same_key(k, key))
            {
                resolve(self, *i, key, Status::Removed, &mut out);
            }
        }
        (out, seam_lines, base_seam_lines)
    }

    /// Call sites, in either revision, whose key a seam definition
    /// inside `range` of `file` carries — as caller fragments.
    fn seam_callers(&mut self, file: &std::path::Path, range: (u32, u32)) -> Vec<Fragment> {
        if self.seams.is_empty() {
            return Vec::new();
        }
        use crate::seam::Seam;
        let head = self.hono_scan(true);
        let base = self.hono_scan(false);
        let keys: Vec<(usize, String)> = head
            .seam_defs
            .iter()
            .chain(base.seam_defs.iter())
            .filter(|(_, d)| d.file == file && d.line >= range.0 && d.line <= range.1)
            .map(|(i, d)| (*i, d.key.clone()))
            .collect();
        if keys.is_empty() {
            return Vec::new();
        }
        let matching = |scan: &HonoScan| -> Vec<(PathBuf, u32)> {
            scan.seam_calls
                .iter()
                .filter(|(i, c)| {
                    keys.iter()
                        .any(|(j, k)| j == i && self.seams[*i].same_key(k, &c.key))
                })
                .map(|(_, c)| (c.file.clone(), c.line))
                .collect()
        };
        let head_raw = matching(&head);
        let base_raw = matching(&base);
        self.callers_from_hits(head_raw, base_raw)
    }

    /// Files that import `file` and, in at least one revision, call
    /// `label` by its bare name — the closest a purely syntactic search
    /// can get to "who actually calls this export", without a real
    /// cross-module type resolver. A destructured-and-renamed import, or
    /// a call reached only through a re-export, won't be found this way.
    /// Every file importing `file`, directly or through a barrel that
    /// re-exports it, in either revision; tests excluded. Parsed into
    /// `function_files` on the way. Memoized per file.
    fn importers_of(&mut self, file: &std::path::Path) -> Vec<String> {
        if let Some(hit) = self.importers_cache.get(file) {
            return hit.clone();
        }
        // A file's export usually reaches its callers through a barrel
        // (`export * from './client.ts'` in the package's index.ts, then
        // `import { x } from '@pkg'`): the importers of every barrel that
        // re-exports this file count as importers of the file.
        let mut sources = vec![file.to_path_buf()];
        sources.extend(self.barrels_of(file));
        let mut importer_paths: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for src in &sources {
            for (rev, ws) in [
                (&self.base_rev, &self.base_ws),
                (&self.head_rev, &self.head_ws),
            ] {
                let g = crate::graph::reach(&self.root, rev, src, ImportDirection::Callers, ws);
                importer_paths.extend(g.nodes.into_iter().map(|n| n.path));
            }
        }
        // A Go package's files call each other's functions unqualified,
        // with no import between them.
        // And a method may be called through an interface it
        // satisfies: from the interface's package, and whatever imports
        // that.
        if Lang::of(file) == Some(Lang::Go) {
            let mut dirs = vec![
                file.parent()
                    .unwrap_or(std::path::Path::new(""))
                    .to_path_buf(),
            ];
            dirs.extend(self.go_interface_dirs(file));
            dirs.dedup();
            for (i, dir) in dirs.iter().enumerate() {
                for (rev, ws) in [
                    (&self.base_rev, &self.base_ws),
                    (&self.head_rev, &self.head_ws),
                ] {
                    let files = crate::golang::package_files(dir, &self.root, rev);
                    if i > 0
                        && let Some(first) = files.first()
                    {
                        let g = crate::graph::reach(
                            &self.root,
                            rev,
                            first,
                            ImportDirection::Callers,
                            ws,
                        );
                        importer_paths.extend(g.nodes.into_iter().map(|n| n.path));
                    }
                    importer_paths
                        .extend(files.into_iter().map(|p| p.to_string_lossy().into_owned()));
                }
            }
        }
        // Files an external resolver saw calling into this one.
        if let Some(lang) = Lang::of(file)
            && self.resolvers.iter().any(|r| r.language == lang)
        {
            for head in [true, false] {
                if let Some(calls) = self.external_calls(head, lang) {
                    importer_paths.extend(
                        calls
                            .into
                            .get(file)
                            .into_iter()
                            .flatten()
                            .map(|p| p.to_string_lossy().into_owned()),
                    );
                }
            }
        }
        importer_paths.remove(&file.to_string_lossy().into_owned());
        importer_paths.retain(|p| !is_test_file(std::path::Path::new(p)));
        for p in &importer_paths {
            self.ensure_function_file(std::path::Path::new(p));
        }
        let importer_paths: Vec<String> = importer_paths.into_iter().collect();
        self.importers_cache
            .insert(file.to_path_buf(), importer_paths.clone());
        importer_paths
    }

    /// Whether `call` (a dotted call text in `importer`) reaches
    /// `label` in `target`: resolved through the importer's bindings as
    /// a callee would be. A chain on something that isn't imported (a
    /// parameter, a local) reaches nothing across files; a chain traced
    /// only as far as a package counts when that package is the target's.
    fn call_resolves_to(
        &mut self,
        importer: &std::path::Path,
        call: &str,
        target: &std::path::Path,
        label: &str,
    ) -> bool {
        let frag = (
            make_fn_id(importer, call),
            call.to_string(),
            Status::Unchanged,
            false,
        );
        self.resolve_fragments(frag)
            .into_iter()
            .any(|(id, _, _, drillable)| match drillable {
                true => id == make_fn_id(target, label),
                false => match self.call_file.get(&id) {
                    Some((f, _)) => f == target,
                    // Traced only as far as the target's package: in
                    // TypeScript that is its barrel, which is how
                    // exports are reached. A Python or Go package is
                    // too wide for that to say anything.
                    None => {
                        Lang::of(importer) == Some(Lang::TypeScript)
                            && split_pkg_id(&id)
                                .is_some_and(|(pkg, _)| pkg == self.file_label(target))
                    }
                },
            })
    }

    fn cross_file_callers(&mut self, file: &std::path::Path, label: &str) -> Vec<Fragment> {
        let t = std::time::Instant::now();
        let importer_paths = self.importers_of(file);
        debug_log(&format!(
            "  importers of {} ({}): {:?}",
            file.display(),
            importer_paths.len(),
            t.elapsed()
        ));

        let mut module_head: Vec<(PathBuf, u32)> = Vec::new();
        let mut module_base: Vec<(PathBuf, u32)> = Vec::new();
        let mut out: Vec<Fragment> = Vec::new();
        // A method is only ever called on something (`repo.save`): a
        // bare `save()` elsewhere is another function.
        let method = self
            .function_files
            .get(file)
            .and_then(|e| find_function(e, label))
            .is_some_and(|(_, f)| f.method);
        for p in &importer_paths {
            let importer_path = PathBuf::from(p);
            let Some(entry) = self.function_files.get(&importer_path) else {
                continue;
            };
            // A dotted call only ends in the name (`client.insert(…)`
            // on a parameter, `db.formDomain.insert` on another domain):
            // it counts when it resolves, the way a callee does, to this
            // very function.
            let mut dotted: Vec<String> = entry
                .head_fns
                .iter()
                .chain(&entry.base_fns)
                .flat_map(|f| f.calls.iter())
                .filter(|c| {
                    (method || *c != label)
                        && (call_matches(c, short_name(label))
                            // `Repo(…)` may construct the class whose
                            // `__init__` this is.
                            || (label.ends_with(".__init__")
                                && Lang::of(&importer_path) == Some(Lang::Python)
                                && c.rsplit('.').next().is_some_and(|l| {
                                    l.chars().next().is_some_and(char::is_uppercase)
                                })))
                })
                .cloned()
                .collect();
            dotted.sort();
            dotted.dedup();
            let mut accepted: HashSet<String> = HashSet::new();
            if !method {
                accepted.insert(label.to_string());
            }
            for c in dotted {
                if self.call_resolves_to(&importer_path, &c, file, label) {
                    accepted.insert(c);
                }
            }
            // Any call an external resolver lands here, whatever it is
            // spelled (`f(id)` on a function passed in).
            if Lang::of(&importer_path)
                .is_some_and(|l| self.resolvers.iter().any(|r| r.language == l))
            {
                let mut texts: Vec<String> = self.function_files[&importer_path]
                    .head_fns
                    .iter()
                    .chain(&self.function_files[&importer_path].base_fns)
                    .flat_map(|f| f.calls.iter().cloned())
                    .collect();
                texts.sort();
                texts.dedup();
                let target = make_fn_id(file, label);
                for c in texts {
                    if self
                        .external_targets(&importer_path, &c)
                        .iter()
                        .any(|(id, _)| *id == target)
                    {
                        accepted.insert(c);
                    }
                }
            }
            let Some(entry) = self.function_files.get(&importer_path) else {
                continue;
            };
            // Every named (or routed) function in the importer whose
            // body — callbacks folded in — calls it, on either side;
            // a caller's identity across the revisions comes from the
            // alignment, so one that merely shifted lines is one node.
            let head_callers = enclosing_callers(&entry.head_fns, &accepted);
            let base_callers = enclosing_callers(&entry.base_fns, &accepted);
            let mut listed: HashSet<String> = HashSet::new();
            for &h in &head_callers {
                let caller_label = align::label(&entry.head_fns[h]);
                let still_calls_in_base =
                    base_counterpart(entry, h).is_some_and(|b| base_callers.contains(&b));
                let status = if still_calls_in_base {
                    Status::Unchanged
                } else {
                    Status::Added
                };
                listed.insert(caller_label.clone());
                out.push((
                    make_fn_id(&importer_path, &caller_label),
                    caller_label,
                    status,
                    true,
                ));
            }
            for &b in &base_callers {
                let caller_label = base_label(entry, b);
                if !listed.insert(caller_label.clone()) {
                    continue;
                }
                out.push((
                    make_fn_id(&importer_path, &caller_label),
                    caller_label,
                    Status::Removed,
                    true,
                ));
            }
            // Calls at the top level of the importer — `const pool =
            // createPool(…)` when the module loads — sit in no function:
            // they reach the change as that module's own code.
            // `app = FastAPI()` at a module's top level runs the
            // class's `__init__`.
            let class = label
                .strip_suffix(".__init__")
                .map(|c| c.rsplit('.').next().unwrap_or(c));
            let call = regex::Regex::new(&format!(
                r"\b{}\s*\(",
                regex::escape(class.unwrap_or(label))
            ))
            .expect("valid regex");
            // Go runs no code at a file's top level worth the name (and
            // an interface's method list would read as calls).
            let module_level =
                Lang::of(&importer_path) != Some(Lang::Go) && (!method || class.is_some());
            for (rev, fns, raw) in [
                (&self.head_rev, &entry.head_fns, &mut module_head),
                (&self.base_rev, &entry.base_fns, &mut module_base),
            ] {
                let Some(text) = rev
                    .read(&self.root, &importer_path)
                    .filter(|_| module_level)
                else {
                    continue;
                };
                for (i, l) in text.lines().enumerate() {
                    let line = i as u32 + 1;
                    let code = l.trim_start();
                    if code.starts_with("//")
                        || code.starts_with("import ")
                        || code.starts_with('*')
                    {
                        continue;
                    }
                    if call.is_match(l) && innermost_at(fns, line).is_none() {
                        raw.push((importer_path.clone(), line));
                    }
                }
            }
        }
        out.extend(self.callers_from_hits(module_head, module_base));

        // `export * as alias from './This.ts'` in an importer: the export
        // is then used as `alias.label` wherever *that* module is
        // imported — `db.orderHistoryDomain.
        // deleteByAccountIdWithLimit` in a batch two packages away, and
        // there as a bare reference in a step table, not a call. Neither
        // the import graph (one hop only) nor the call list can see it,
        // so it's followed by text across the packages that depend on
        // this one.
        let stem = file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let alias_re = regex::Regex::new(
            r#"(?m)(?:import|export)\s+\*\s+as\s+(\w+)\s+from\s+['"]([^'"]+)['"]"#,
        )
        .unwrap();
        let mut aliases: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for p in &importer_paths {
            for rev in [&self.head_rev, &self.base_rev] {
                let Some(text) = rev.read(&self.root, std::path::Path::new(p)) else {
                    continue;
                };
                for cap in alias_re.captures_iter(&text) {
                    let spec_stem = std::path::Path::new(&cap[2])
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if spec_stem == stem {
                        aliases.insert(cap[1].to_string());
                    }
                }
            }
        }
        for alias in aliases {
            out.extend(self.reference_callers(file, &format!("{alias}.{label}")));
        }
        out
    }

    /// Every place `pattern` (`alias.name`) appears in a package that
    /// depends on `file`'s package — or in that package itself — as the
    /// function enclosing the reference, or a module-level node when the
    /// reference sits outside any function (an entry in a step table).
    /// Added/removed/unchanged by comparing the same search over BASE.
    fn reference_callers(&mut self, file: &std::path::Path, pattern: &str) -> Vec<Fragment> {
        let t = std::time::Instant::now();
        let re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(pattern))).unwrap();
        let head_files = self.package_files(&self.head_rev, &self.head_ws, file);
        let base_files = self.package_files(&self.base_rev, &self.base_ws, file);
        let find = |rev: &Rev, files: &[PathBuf]| -> Vec<(PathBuf, u32)> {
            let mut v = Vec::new();
            for f in files {
                let Some(text) = rev.read(&self.root, f) else {
                    continue;
                };
                if !re.is_match(&text) {
                    continue;
                }
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        v.push((f.clone(), i as u32 + 1));
                    }
                }
            }
            v
        };
        let head_raw = find(&self.head_rev, &head_files);
        let base_raw = find(&self.base_rev, &base_files);
        debug_log(&format!(
            "  reference search {pattern} over {} + {} files: {:?}",
            head_files.len(),
            base_files.len(),
            t.elapsed()
        ));
        self.callers_from_hits(head_raw, base_raw)
    }

    /// The barrels that re-export `file` wholesale or by name, and the
    /// barrels of those, up to a few levels — typically just the
    /// package's `index.ts`.
    fn barrels_of(&mut self, file: &std::path::Path) -> Vec<PathBuf> {
        let reexports: Vec<(PathBuf, String)> = {
            let mut all = self.hono_scan(true).reexports;
            all.extend(self.hono_scan(false).reexports);
            all
        };
        let mut found: Vec<PathBuf> = Vec::new();
        let mut frontier = vec![file.to_path_buf()];
        for _ in 0..4 {
            let mut next = Vec::new();
            for target in &frontier {
                for (barrel, spec) in &reexports {
                    if found.contains(barrel) || barrel == file {
                        continue;
                    }
                    if self.resolve_spec(spec, barrel).as_deref() == Some(target.as_path()) {
                        found.push(barrel.clone());
                        next.push(barrel.clone());
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        found
    }

    /// Text hits (file, line) in each revision → caller fragments: the
    /// enclosing function, or a `module@<line>` node outside any.
    fn callers_from_hits(
        &mut self,
        head_raw: Vec<(PathBuf, u32)>,
        base_raw: Vec<(PathBuf, u32)>,
    ) -> Vec<Fragment> {
        for (p, _) in head_raw.iter().chain(base_raw.iter()) {
            self.ensure_function_file(p);
        }

        // Keyed by (file, enclosing function or "module") so a
        // reference that merely moved lines still reads as unchanged.
        let mut head_hits: HashMap<(PathBuf, String), u32> = HashMap::new();
        let mut base_hits: HashMap<(PathBuf, String), u32> = HashMap::new();
        for (p, line) in head_raw {
            let Some(entry) = self.function_files.get(&p) else {
                continue;
            };
            let key = innermost_at(&entry.head_fns, line)
                .map(|i| align::label(&entry.head_fns[named_enclosing(&entry.head_fns, i)]))
                .unwrap_or_else(|| "module".to_string());
            head_hits.entry((p, key)).or_insert(line);
        }
        for (p, line) in base_raw {
            let Some(entry) = self.function_files.get(&p) else {
                continue;
            };
            let key = innermost_at(&entry.base_fns, line)
                .map(|i| base_label(entry, named_enclosing(&entry.base_fns, i)))
                .unwrap_or_else(|| "module".to_string());
            base_hits.entry((p, key)).or_insert(line);
        }

        let mut out = Vec::new();
        for ((p, key), line) in &head_hits {
            let status = if base_hits.contains_key(&(p.clone(), key.clone())) {
                Status::Unchanged
            } else {
                Status::Added
            };
            out.push(reference_fragment(p, key, *line, status));
        }
        for ((p, key), line) in &base_hits {
            if head_hits.contains_key(&(p.clone(), key.clone())) {
                continue;
            }
            out.push(reference_fragment(p, key, *line, Status::Removed));
        }
        out
    }

    /// The source files of `file`'s own workspace package and of every
    /// package that declares it as a dependency — where a re-exported
    /// name from it could be referenced. Tests excluded.
    fn package_files(&self, rev: &Rev, ws: &Workspace, file: &std::path::Path) -> Vec<PathBuf> {
        let Some(owner) = ws.owning_package(file) else {
            return Vec::new();
        };
        let dirs: Vec<&PathBuf> = ws
            .packages
            .iter()
            .filter(|p| {
                p.dir == owner.dir || owner.name.as_deref().is_some_and(|n| p.deps.contains(n))
            })
            .map(|p| &p.dir)
            .collect();
        let mut seen: HashSet<&PathBuf> = HashSet::new();
        let per_dir: Vec<_> = dirs
            .into_iter()
            .filter(|d| seen.insert(*d))
            .map(|d| rev.files_under(&self.root, d))
            .collect();
        let mut files: Vec<PathBuf> = per_dir.iter().flat_map(|v| v.iter().cloned()).collect();
        // A package nested inside another's directory is listed twice.
        files.sort_unstable();
        files.dedup();
        files
            .into_iter()
            .filter(|f| {
                f.extension()
                    .is_some_and(|e| crate::lang::SOURCE_EXTENSIONS.iter().any(|x| *x == e))
            })
            .filter(|f| !is_test_file(f))
            .collect()
    }

    /// Every id one hop from `id` in `dir`, with a display label and
    /// status attached — function granularity resolves through
    /// [`function_fragment`] (which can cross file boundaries); file and
    /// package granularity re-run the same reach the hub used, since a
    /// hub fragment's label is a bare basename/package name, not
    /// necessarily something that re-queries correctly.
    fn fragment_for(&mut self, id: &str, dir: Dir) -> Vec<Fragment> {
        match self.granularity {
            Granularity::Function => {
                let (file, label) = split_fn_id(id);
                self.function_fragment(&file, label, dir)
            }
            Granularity::File => {
                let Some(h) = self.hub_for(id) else {
                    return Vec::new();
                };
                let fragment = match dir {
                    Dir::Callers => h.callers,
                    Dir::Callees => h.callees,
                };
                let from = PathBuf::from(id);
                let direction = match dir {
                    Dir::Callers => ImportDirection::Callers,
                    Dir::Callees => ImportDirection::Dependencies,
                };
                let mut resolved = Vec::new();
                for n in fragment.into_iter().filter(|n| n.drillable) {
                    let mut found = None;
                    for (rev, ws) in [
                        (&self.base_rev, &self.base_ws),
                        (&self.head_rev, &self.head_ws),
                    ] {
                        let g = crate::graph::reach(&self.root, rev, &from, direction, ws);
                        if let Some(hit) = g.nodes.iter().find(|hn| basename(&hn.path) == n.label) {
                            found = Some(hit.path.clone());
                            break;
                        }
                    }
                    resolved.push((
                        found.unwrap_or_else(|| n.label.clone()),
                        n.label,
                        n.status,
                        true,
                    ));
                }
                resolved
            }
            Granularity::Package => {
                let Some(h) = self.hub_for(id) else {
                    return Vec::new();
                };
                let fragment = match dir {
                    Dir::Callers => h.callers,
                    Dir::Callees => h.callees,
                };
                fragment
                    .into_iter()
                    .filter(|n| n.drillable)
                    .map(|n| {
                        let resolved = package_hub::dir_of(&self.base_ws, &self.head_ws, &n.label)
                            .map(|d| d.to_string_lossy().into_owned())
                            .unwrap_or_else(|| n.label.clone());
                        (resolved, n.label, n.status, true)
                    })
                    .collect()
            }
        }
    }

    fn expand(&mut self, id: String, dir: Dir) {
        if dir == Dir::Callers {
            self.callers_sought.insert(id.clone());
        }
        let fragment = self.fragment_for(&id, dir);
        if fragment.is_empty() {
            if dir == Dir::Callers {
                self.no_callers.insert(id.clone());
            }
            self.status = match dir {
                Dir::Callers => format!("no callers found for {}", basename(&id)),
                Dir::Callees => format!("nothing resolved that {} calls/imports", basename(&id)),
            };
            return;
        }
        let mut added = 0;
        let mut linked = 0;
        let mut hidden = 0;
        let mut local = 0;
        for (call_index, frag) in fragment.into_iter().enumerate() {
            let resolved = self.resolve_fragments(frag);
            if resolved.is_empty() {
                local += 1;
                continue;
            }
            for (new_id, label, call_status, drillable) in resolved {
                if new_id == id {
                    continue;
                }
                let edge = match dir {
                    Dir::Callers => (new_id.clone(), id.clone()),
                    Dir::Callees => (id.clone(), new_id.clone()),
                };
                // The line carries the call's status (a call added or
                // removed); the node carries its own body's. A function that
                // didn't change is grey even when a brand-new call reaches
                // it — the green line is what says "newly reached".
                if self
                    .inferred_calls
                    .contains(&(split_fn_id(&edge.0).0, edge.1.clone()))
                {
                    self.inferred_edges.insert(edge.clone());
                }
                self.edges.entry(edge).or_insert(call_status);
                let status = self.own_status(&new_id, &label, drillable, call_status);
                if let Some(existing) = self.nodes.iter_mut().find(|e| e.id == new_id) {
                    // Reached again from somewhere else: the new edge is the
                    // news. An unresolved call shared by several callers
                    // counts as changed if *any* of them added or removed it.
                    if !existing.drillable
                        && existing.status == Status::Unchanged
                        && status != Status::Unchanged
                    {
                        existing.status = status;
                    }
                    linked += 1;
                    continue;
                }
                let display = self.node_display(&new_id, &label, drillable);
                self.nodes.push(GNode {
                    id: new_id.clone(),
                    display,
                    status,
                    drillable,
                });
                if dir == Dir::Callees {
                    self.call_order.entry(new_id.clone()).or_insert(call_index);
                    if status == Status::Unchanged && !self.show_unchanged {
                        hidden += 1;
                    }
                }
                self.expanded_from
                    .entry(new_id)
                    .or_insert_with(|| (id.clone(), dir));
                added += 1;
            }
        }
        let mut parts = vec![format!("+{added} node(s)")];
        if linked > 0 {
            parts.push(format!("{linked} link(s) to nodes already shown"));
        }
        if hidden > 0 {
            parts.push(format!("{hidden} unchanged hidden — a to show"));
        }
        if local > 0 {
            parts.push(format!("{local} call(s) on local values not shown"));
        }
        self.status = parts.join(", ");
        self.refresh_displays();
    }

    /// Re-derives every function node's label. A root's label is made
    /// before anything else is known; once the route table exists (a
    /// later expansion built it), a handler's label gains its mounted
    /// path.
    fn refresh_displays(&mut self) {
        if self.granularity != Granularity::Function {
            return;
        }
        let ids: Vec<String> = self
            .nodes
            .iter()
            .filter(|n| n.drillable && !is_ext_id(&n.id))
            .map(|n| n.id.clone())
            .collect();
        for id in ids {
            let display = self.function_display(&id);
            if let Some(n) = self.nodes.iter_mut().find(|n| n.id == id) {
                n.display = display;
            }
        }
    }

    /// A node's own diff status: for a real function, whether its body
    /// was added, removed, changed or left alone between the revisions;
    /// for anything without a body of its own (an unresolved call, a
    /// module-level reference), the status of the call that reached it.
    fn own_status(
        &mut self,
        id: &str,
        label: &str,
        drillable: bool,
        call_status: Status,
    ) -> Status {
        if self.granularity != Granularity::Function || !drillable || label.starts_with("module@") {
            return call_status;
        }
        let (file, label) = split_fn_id(id);
        if !self.ensure_function_file(&file) {
            return call_status;
        }
        let entry = &self.function_files[&file];
        let in_head = entry.head_fns.iter().any(|f| align::label(f) == label);
        let in_base = (0..entry.base_fns.len()).any(|i| base_label(entry, i) == label);
        match (in_head, in_base) {
            (true, _) => body_status(entry, label),
            (false, true) => Status::Removed,
            (false, false) => call_status,
        }
    }

    /// Gives a fragment its canonical node id — or drops it. A callee
    /// the in-file hub couldn't resolve is traced from its first
    /// identifier through the file's import bindings: a name imported
    /// from a workspace file becomes that real function; a namespace
    /// import (`db.someDomain.find`) is walked through the barrel's
    /// `export * as` re-exports to the file that defines it; anything
    /// imported but not traceable that far becomes one `pkg::` node per
    /// package, shared by every caller, so the box it sits in is always
    /// a package. A call on something that isn't imported at all — a
    /// local map, a parameter, a global — is an operation inside the
    /// caller, not a hop to anywhere, and is not a node.
    fn resolve_fragment(&mut self, frag: Fragment) -> Option<Fragment> {
        let (id, label, status, drillable) = frag;
        if self.granularity != Granularity::Function || drillable || label.starts_with("module@") {
            return Some((id, label, status, drillable));
        }
        // A frontend calling the backend through Hono's RPC client: the
        // call spells the route, so it joins straight to the handler.
        if let Some(call) = crate::hono::rpc_call(&label) {
            let hit = self
                .route_table()
                .entries
                .iter()
                .find(|e| {
                    e.method == call.method && crate::hono::same_route(&e.segments, &call.segments)
                })
                .map(|e| (e.file.clone(), e.label.clone()));
            let shown = format!("{} /{}", call.method, call.segments.join("/"));
            debug_log(&format!("rpc call {label} -> {hit:?}"));
            if let Some((f, handler)) = hit {
                self.call_text
                    .entry(make_fn_id(&f, &handler))
                    .or_insert(label);
                return Some((make_fn_id(&f, &handler), handler, status, true));
            }
            let new_id = format!("pkg::HTTP API::{shown}");
            self.call_text.entry(new_id.clone()).or_insert(label);
            return Some((new_id, shown, status, false));
        }
        let (file, _) = split_fn_id(&id);
        let head = label.split('(').next().unwrap_or(&label);
        let segments: Vec<&str> = head.split('.').collect();
        let root = *segments.first()?;
        if Lang::of(&file) == Some(Lang::Go) {
            return self.resolve_go(&file, &label, &segments, status);
        }
        // `Repo(db)`: Python runs the class's `__init__`.
        if Lang::of(&file) == Some(Lang::Python)
            && head.len() == label.len()
            && let Some((f, init)) = self.python_constructor(&file, &segments)
        {
            return Some((make_fn_id(&f, &init), init, status, true));
        }
        // `target.findChangedByValidStarts(…)` where `target` is a local
        // (a parameter, a strategy object) and the file itself defines
        // a method of that name: that method is the callee — all of
        // them, when several objects define it, since the receiver is
        // whichever one was handed in.
        if segments.len() >= 2
            && let Some(last) = segments.last()
            && let Some(found) = self.function_files.get(&file).and_then(|entry| {
                let all = || entry.head_fns.iter().chain(&entry.base_fns);
                // `Repo.save` (a Python call whose class is known) names
                // the method exactly.
                all()
                    .find(|f| f.method && align::label(f) == head)
                    .or_else(|| all().find(|f| f.method && short_name(&align::label(f)) == *last))
                    .map(align::label)
            })
        {
            return Some((make_fn_id(&file, &found), found, status, true));
        }
        let import = self.bindings_of(&file).get(root)?.clone();

        let entry_file = self.resolve_spec(&import.specifier, &file);
        let pkg = match &entry_file {
            Some(f) => self.file_label(f),
            None => crate::lang::package_name(&file, &import.specifier),
        };
        let plain_chain = head.len() == label.len();
        let mut reached: Option<PathBuf> = None;
        // Python: `pool.query` after `from shop.db import pool`, or
        // `shop.db.pool.query` after `import shop.db.pool` — the chain
        // names modules down to the function.
        if Lang::of(&file) == Some(Lang::Python) && plain_chain && segments.len() >= 2 {
            let mut module = match &import.binding {
                crate::bindings::Binding::Named(n) => crate::python::join(&import.specifier, n),
                _ => import.specifier.clone(),
            };
            for seg in &segments[1..segments.len() - 1] {
                module = crate::python::join(&module, seg);
            }
            let last = segments[segments.len() - 1];
            if let Some(f) = self.resolve_spec(&module, &file) {
                if self.ensure_function_file(&f) && self.is_importable(&f, last) {
                    return Some((make_fn_id(&f, last), last.to_string(), status, true));
                }
                if let Some(d) = self.resolve_import(&f, last) {
                    return Some((make_fn_id(&d, last), last.to_string(), status, true));
                }
                if self.exports_textually(&f, last) {
                    reached = Some(f);
                }
            }
            // `Repo.save` (or `mod.Repo.save`) with the class imported:
            // the method, in the file that defines the class.
            let mut parts: Vec<&str> = match &import.binding {
                crate::bindings::Binding::Named(n) => vec![n.as_str()],
                _ => Vec::new(),
            };
            parts.extend(&segments[1..segments.len() - 1]);
            if reached.is_none()
                && let Some((class, path)) = parts.split_last()
            {
                let module = path
                    .iter()
                    .fold(import.specifier.clone(), |m, p| crate::python::join(&m, p));
                if let Some(f) = self.resolve_spec(&module, &file)
                    && let Some((d, method)) = self.python_method(&f, class, last)
                {
                    return Some((make_fn_id(&d, &method), method, status, true));
                }
            }
        }
        let target: Option<(PathBuf, String)> = match (&import.binding, entry_file) {
            (crate::bindings::Binding::Named(imported), Some(f))
                if plain_chain && segments.len() == 1 =>
            {
                if self.ensure_function_file(&f) && self.is_importable(&f, imported) {
                    Some((f, imported.clone()))
                } else {
                    self.resolve_import(&f, imported)
                        .map(|d| (d, imported.clone()))
                }
            }
            (crate::bindings::Binding::Namespace, Some(mut f))
                if plain_chain && segments.len() >= 2 =>
            {
                let mut ok = true;
                for seg in &segments[1..segments.len() - 1] {
                    match self.reexports_of(&f).get(*seg).cloned() {
                        Some(spec) => match self.resolve_spec(&spec, &f) {
                            Some(next) => f = next,
                            None => {
                                ok = false;
                                break;
                            }
                        },
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                let last = segments[segments.len() - 1];
                let parsed = ok && self.ensure_function_file(&f);
                let importable = parsed && self.is_importable(&f, last);
                debug_log(&format!(
                    "ns walk {label}: ok={ok} file={} parsed={parsed} importable={importable}",
                    f.display()
                ));
                if ok && !importable && self.exports_textually(&f, last) {
                    reached = Some(f.clone());
                }
                importable.then(|| (f, last.to_string()))
            }
            _ => None,
        };
        if let Some((f, name)) = target {
            return Some((make_fn_id(&f, &name), name, status, true));
        }
        let rest = match import.binding {
            crate::bindings::Binding::Namespace => label
                .strip_prefix(root)
                .and_then(|r| r.strip_prefix('.'))
                .unwrap_or(&label)
                .to_string(),
            _ => label.clone(),
        };
        // The id keeps the whole namespaced path, so two domains that
        // both export a `findEnabledByAccountIdIn` stay two
        // nodes. Traced to a file, the file header names the domain, so
        // the label itself only needs the exported name.
        let new_id = format!("pkg::{pkg}::{rest}");
        let shown = match &reached {
            Some(_) => segments[segments.len() - 1].to_string(),
            None => rest,
        };
        if let Some(f) = reached {
            let name = segments[segments.len() - 1];
            let line = self.line_of_name(&f, name);
            self.call_file.entry(new_id.clone()).or_insert((f, line));
        }
        self.call_text.entry(new_id.clone()).or_insert(label);
        Some((new_id, shown, status, false))
    }

    /// [`resolve_fragment`], and for a Go method call nothing concrete
    /// answers, every method it may land on through an interface: the
    /// implementations of the interfaces the calling file can see that
    /// declare the method. Those calls are inferred, and recorded so.
    fn resolve_fragments(&mut self, frag: Fragment) -> Vec<Fragment> {
        let (id, label, status, drillable) = frag.clone();
        if !drillable && self.granularity == Granularity::Function {
            let found = self.external_targets(&split_fn_id(&id).0, &label);
            if !found.is_empty() {
                return found
                    .into_iter()
                    .map(|(target, name)| (target, name, status, true))
                    .collect();
            }
        }
        if let Some(hit) = self.resolve_fragment(frag) {
            return vec![hit];
        }
        let (file, _) = split_fn_id(&id);
        if drillable || label.contains('(') || Lang::of(&file) != Some(Lang::Go) {
            return Vec::new();
        }
        let segments: Vec<&str> = label.split('.').collect();
        let (Some(root), Some(last)) = (segments.first(), segments.last()) else {
            return Vec::new();
        };
        if segments.len() < 2 || self.bindings_of(&file).contains_key(*root) {
            return Vec::new();
        }
        let last = last.to_string();
        self.go_interface_targets(&file, &last)
            .into_iter()
            .map(|(f, method)| {
                let target = make_fn_id(&f, &method);
                self.inferred_calls.insert((file.clone(), target.clone()));
                (target, method, status, true)
            })
            .collect()
    }

    /// What the external resolver for `lang` reported in one revision —
    /// run on first need, once.
    fn external_calls(
        &mut self,
        head: bool,
        lang: Lang,
    ) -> Option<std::rc::Rc<crate::resolver::Calls>> {
        if let Some(hit) = self.external.get(&(head, lang)) {
            return Some(hit.clone());
        }
        let resolver = self.resolvers.iter().find(|r| r.language == lang)?.clone();
        let rev = if head { &self.head_rev } else { &self.base_rev };
        let tree = rev.dir(&self.root)?;
        let t = std::time::Instant::now();
        let calls = std::rc::Rc::new(crate::resolver::run(
            &resolver,
            &tree,
            &self.root,
            if head { "head" } else { "base" },
            rev.commit_sha(),
        ));
        debug_log(&format!(
            "resolver {lang:?} ({}): {} call sites: {:?}",
            if head { "head" } else { "base" },
            calls.by_site.len(),
            t.elapsed()
        ));
        self.external.insert((head, lang), calls.clone());
        Some(calls)
    }

    /// The functions an external resolver says the call `label` in
    /// `file` lands on, in either revision: (id, label). When the line
    /// holds several calls, the targets named like the call are kept.
    fn external_targets(&mut self, file: &std::path::Path, label: &str) -> Vec<(String, String)> {
        let Some(lang) = Lang::of(file) else {
            return Vec::new();
        };
        if !self.resolvers.iter().any(|r| r.language == lang) || !self.ensure_function_file(file) {
            return Vec::new();
        }
        let mut positions: Vec<(bool, PathBuf, u32)> = Vec::new();
        for head in [true, false] {
            let entry = &self.function_files[file];
            let fns = if head {
                &entry.head_fns
            } else {
                &entry.base_fns
            };
            let lines: BTreeSet<u32> = fns
                .iter()
                .flat_map(|f| f.calls.iter().zip(&f.call_lines))
                .filter(|(c, _)| *c == label)
                .map(|(_, l)| *l)
                .collect();
            if lines.is_empty() {
                continue;
            }
            let Some(calls) = self.external_calls(head, lang) else {
                continue;
            };
            for line in lines {
                for (f, l) in calls
                    .by_site
                    .get(&(file.to_path_buf(), line))
                    .into_iter()
                    .flatten()
                {
                    positions.push((head, f.clone(), *l));
                }
            }
        }
        let mut out: Vec<(String, String)> = Vec::new();
        for (head, f, line) in positions {
            if !self.ensure_function_file(&f) {
                continue;
            }
            let entry = &self.function_files[&f];
            let fns = if head {
                &entry.head_fns
            } else {
                &entry.base_fns
            };
            // The innermost function around the line.
            let Some((i, _)) = fns
                .iter()
                .enumerate()
                .filter(|(_, g)| g.start_line <= line && line <= g.end_line)
                .min_by_key(|(_, g)| g.end_line - g.start_line)
            else {
                continue;
            };
            let name = if head {
                align::label(&fns[i])
            } else {
                base_label(entry, i)
            };
            let id = make_fn_id(&f, &name);
            if !out.iter().any(|(x, _)| *x == id) {
                out.push((id, name));
            }
        }
        let called = label
            .split('(')
            .next()
            .unwrap_or(label)
            .rsplit('.')
            .next()
            .unwrap_or(label);
        if out.len() > 1 && out.iter().any(|(_, n)| short_name(n) == called) {
            out.retain(|(_, n)| short_name(n) == called);
        }
        out
    }

    /// Interfaces and method sets of every Go file in either revision.
    fn go_index(&mut self) -> &GoIndex {
        if self.go_index.is_none() {
            let mut index = GoIndex::default();
            for rev in [&self.head_rev, &self.base_rev] {
                for f in rev.list_files(&self.root) {
                    if f.extension().is_none_or(|e| e != "go") || is_test_file(&f) {
                        continue;
                    }
                    let Some(src) = rev.read(&self.root, &f) else {
                        continue;
                    };
                    let dir = f.parent().unwrap_or(std::path::Path::new("")).to_path_buf();
                    let t = crate::golang::types(&src);
                    for (name, methods) in t.interfaces {
                        let key = (dir.clone(), name);
                        if !index.interfaces.iter().any(|(k, _)| *k == key) {
                            index.interfaces.push((key, methods));
                        }
                    }
                    for (ty, m) in t.methods {
                        index
                            .methods
                            .entry((dir.clone(), ty))
                            .or_default()
                            .entry(m)
                            .or_insert_with(|| f.clone());
                    }
                }
            }
            self.go_index = Some(index);
        }
        self.go_index.as_ref().unwrap()
    }

    /// The files of the methods named `method` that a call from `file`
    /// may reach through an interface declared in its own package or
    /// one it imports. None when more than [`MAX_IMPLEMENTATIONS`]
    /// types qualify: a method that common says nothing.
    fn go_interface_targets(
        &mut self,
        file: &std::path::Path,
        method: &str,
    ) -> Vec<(PathBuf, String)> {
        let mut visible: Vec<PathBuf> = vec![
            file.parent()
                .unwrap_or(std::path::Path::new(""))
                .to_path_buf(),
        ];
        let specs: Vec<String> = self
            .bindings_of(file)
            .values()
            .map(|i| i.specifier.clone())
            .collect();
        for spec in specs {
            if let Some(dir) = crate::golang::resolve(&spec, &self.head_ws)
                .or_else(|| crate::golang::resolve(&spec, &self.base_ws))
            {
                visible.push(dir);
            }
        }
        let index = self.go_index();
        let mut out: Vec<(PathBuf, String)> = Vec::new();
        for ((dir, _), methods) in &index.interfaces {
            if !visible.contains(dir) || !methods.iter().any(|m| m == method) {
                continue;
            }
            for ((_, ty), set) in &index.methods {
                if methods.iter().all(|m| set.contains_key(m)) {
                    out.push((set[method].clone(), format!("{ty}.{method}")));
                }
            }
        }
        out.sort();
        out.dedup();
        if out.len() > MAX_IMPLEMENTATIONS {
            return Vec::new();
        }
        out
    }

    /// The directories of the interfaces some type in `file`
    /// implements: where calls that may land on its methods are made.
    fn go_interface_dirs(&mut self, file: &std::path::Path) -> Vec<PathBuf> {
        let index = self.go_index();
        let mut out: Vec<PathBuf> = index
            .interfaces
            .iter()
            .filter(|(_, methods)| {
                index.methods.values().any(|set| {
                    methods.iter().all(|m| set.contains_key(m)) && set.values().any(|f| f == file)
                })
            })
            .map(|((dir, _), _)| dir.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// [`resolve_fragment`] for a call made in a Go file. `pkg.F` is
    /// the function `F` in the package `pkg` imports, found among its
    /// files; a bare `f` is a function elsewhere in the caller's own
    /// package; `x.M` on anything not imported is a method `M` of the
    /// caller's package. A package outside the repository becomes one
    /// `pkg::` node per function, by its import path.
    fn resolve_go(
        &mut self,
        file: &std::path::Path,
        label: &str,
        segments: &[&str],
        status: Status,
    ) -> Option<Fragment> {
        let plain = !label.contains('(');
        let last = *segments.last()?;
        let own_dir = file
            .parent()
            .unwrap_or(std::path::Path::new(""))
            .to_path_buf();
        let import = (segments.len() >= 2)
            .then(|| self.bindings_of(file).get(segments[0]).cloned())
            .flatten();
        let Some(import) = import else {
            let method = segments.len() >= 2;
            if !plain {
                return None;
            }
            if let Some((f, name)) = self.go_package_function(&own_dir, last, method) {
                return Some((make_fn_id(&f, &name), name, status, true));
            }
            if !method {
                return None;
            }
            // A method on a value whose type the file got from a
            // package it imports (`s.pool.Query` with `pool *db.Pool`):
            // taken when exactly one imported package has a method of
            // that name.
            let mut specs: Vec<String> = self
                .bindings_of(file)
                .values()
                .map(|i| i.specifier.clone())
                .collect();
            specs.sort();
            specs.dedup();
            let mut hits = Vec::new();
            for spec in specs {
                let dir = crate::golang::resolve(&spec, &self.head_ws)
                    .or_else(|| crate::golang::resolve(&spec, &self.base_ws));
                if let Some(dir) = dir
                    && let Some(hit) = self.go_package_function(&dir, last, true)
                {
                    hits.push(hit);
                }
            }
            return match hits.as_slice() {
                [(f, name)] => Some((make_fn_id(f, name), name.clone(), status, true)),
                _ => None,
            };
        };
        let dir = crate::golang::resolve(&import.specifier, &self.head_ws)
            .or_else(|| crate::golang::resolve(&import.specifier, &self.base_ws));
        if let Some(dir) = &dir
            && plain
            && segments.len() == 2
            && let Some((f, name)) = self.go_package_function(dir, last, false)
        {
            return Some((make_fn_id(&f, &name), name, status, true));
        }
        let rest = label
            .strip_prefix(segments[0])
            .and_then(|r| r.strip_prefix('.'))
            .unwrap_or(label)
            .to_string();
        let new_id = format!("pkg::{}::{rest}", import.specifier);
        self.call_text
            .entry(new_id.clone())
            .or_insert(label.to_string());
        Some((new_id, rest, status, false))
    }

    /// The file of the Go package in `dir` (either revision) that
    /// defines a top-level function — or, with `method`, a method of
    /// any type — named `name`, and its label (`Store.Save`).
    fn go_package_function(
        &mut self,
        dir: &std::path::Path,
        name: &str,
        method: bool,
    ) -> Option<(PathBuf, String)> {
        let mut files: Vec<PathBuf> = [&self.head_rev, &self.base_rev]
            .into_iter()
            .flat_map(|rev| crate::golang::package_files(dir, &self.root, rev))
            .collect();
        files.sort();
        files.dedup();
        files.into_iter().find_map(|f| {
            if !self.exports_textually(&f, name) || !self.ensure_function_file(&f) {
                return None;
            }
            let entry = &self.function_files[&f];
            let label = entry
                .head_fns
                .iter()
                .chain(&entry.base_fns)
                .filter(|g| g.parent.is_none() && g.method == method)
                .filter_map(|g| g.name.clone())
                .find(|n| match method {
                    true => short_name(n) == name,
                    false => n == name,
                })?;
            Some((f, label))
        })
    }

    /// Every source file of one revision (tests excluded).
    fn source_files(&self, rev: &Rev) -> Vec<PathBuf> {
        rev.list_files(&self.root)
            .into_iter()
            .filter(|f| {
                f.extension()
                    .is_some_and(|e| crate::lang::SOURCE_EXTENSIONS.iter().any(|x| *x == e))
            })
            .filter(|f| !is_test_file(f))
            .collect()
    }

    /// RPC call sites and router mounts of one revision, scanned once
    /// over every source file — cheap text matching, no parsing.
    fn hono_scan(&mut self, head: bool) -> HonoScan {
        let cached = if head {
            &self.hono_head
        } else {
            &self.hono_base
        };
        if let Some(v) = cached {
            return v.clone();
        }
        let t = std::time::Instant::now();
        let rev = if head { &self.head_rev } else { &self.base_rev };
        let mut scan = HonoScan::default();
        for f in self.source_files(rev) {
            let Some(src) = rev.read(&self.root, &f) else {
                continue;
            };
            let ts = Lang::of(&f) == Some(Lang::TypeScript);
            // A package's `__init__.py` is Python's barrel: what it
            // imports, it re-exports.
            if f.file_name().is_some_and(|n| n == "__init__.py") {
                for spec in crate::python::import_specs(&src) {
                    scan.reexports.push((f.clone(), spec));
                }
            }
            for (call, line) in ts
                .then(|| crate::hono::rpc_calls_in(&src))
                .into_iter()
                .flatten()
            {
                scan.sites.push(RpcSite {
                    call,
                    file: f.clone(),
                    line,
                });
            }
            if ts && src.contains(".route(") && f.extension().is_none_or(|e| e != "svelte") {
                for (prefix, binding) in crate::hono::mounts_in(&src) {
                    scan.mounts.push((f.clone(), prefix, binding));
                }
            }
            if ts && src.contains("export") && src.contains(" from ") {
                for spec in crate::bindings::reexport_specs(&src) {
                    scan.reexports.push((f.clone(), spec));
                }
            }
            for (i, seam) in self.seams.iter().enumerate() {
                use crate::seam::Seam;
                let (defs, calls) = seam.scan(&f, &src);
                scan.seam_defs.extend(defs.into_iter().map(|d| (i, d)));
                scan.seam_calls.extend(calls.into_iter().map(|c| (i, c)));
            }
        }
        debug_log(&format!(
            "hono scan ({}): {} rpc sites, {} mounts, {} seam defs, {} seam calls: {:?}",
            if head { "head" } else { "base" },
            scan.sites.len(),
            scan.mounts.len(),
            scan.seam_defs.len(),
            scan.seam_calls.len(),
            t.elapsed()
        ));
        if head {
            self.hono_head = Some(scan.clone());
        } else {
            self.hono_base = Some(scan.clone());
        }
        scan
    }

    /// Every route handler with its full path: `.route('/prefix', X)`
    /// statements are followed from the files nobody mounts (the apps)
    /// down to the routers, prefixes concatenated, and each router's
    /// handlers get the prefix in front of their own path.
    fn route_table(&mut self) -> &RouteTable {
        if self.routes.is_none() {
            let t = std::time::Instant::now();
            let raw = self.hono_scan(true).mounts;
            let mut mounts: Vec<(PathBuf, String, PathBuf)> = Vec::new();
            for (f, prefix, binding) in raw {
                let Some(imp) = self.bindings_of(&f).get(&binding).cloned() else {
                    continue;
                };
                if let Some(child) = self.resolve_spec(&imp.specifier, &f) {
                    mounts.push((f, prefix, child));
                }
            }
            let children: HashSet<&PathBuf> = mounts.iter().map(|m| &m.2).collect();
            let mut stack: Vec<(PathBuf, Vec<String>)> = mounts
                .iter()
                .map(|m| &m.0)
                .filter(|p| !children.contains(p))
                .map(|p| (p.clone(), Vec::new()))
                .collect();
            let mut mount_paths: HashMap<PathBuf, Vec<String>> = HashMap::new();
            while let Some((f, path)) = stack.pop() {
                if mount_paths.contains_key(&f) {
                    continue;
                }
                for (parent, prefix, child) in &mounts {
                    if *parent == f {
                        let mut p = path.clone();
                        p.extend(crate::hono::path_segments(prefix));
                        stack.push((child.clone(), p));
                    }
                }
                mount_paths.insert(f, path);
            }
            let mut entries = Vec::new();
            let mut mounted: Vec<(PathBuf, Vec<String>)> = mount_paths
                .iter()
                .map(|(f, p)| (f.clone(), p.clone()))
                .collect();
            mounted.sort();
            for (f, base) in mounted {
                if !self.ensure_function_file(&f) {
                    continue;
                }
                let entry = &self.function_files[&f];
                for func in &entry.head_fns {
                    let Some(route) = &func.route else { continue };
                    let Some((method, path)) = crate::hono::split_route_label(route) else {
                        continue;
                    };
                    let mut segments = base.clone();
                    segments.extend(crate::hono::path_segments(path));
                    entries.push(RouteEntry {
                        method: method.to_string(),
                        segments,
                        file: f.clone(),
                        label: align::label(func),
                    });
                }
            }
            debug_log(&format!(
                "route table: {} mounts, {} routers, {} handlers: {:?}",
                mounts.len(),
                mount_paths.len(),
                entries.len(),
                t.elapsed()
            ));
            self.routes = Some(RouteTable {
                entries,
                mount_paths,
            });
        }
        self.routes.as_ref().unwrap()
    }

    /// The frontend functions that call a route handler through the RPC
    /// client, by method and full path.
    fn rpc_callers(&mut self, file: &std::path::Path, route: &str) -> Vec<Fragment> {
        let Some((method, path)) = crate::hono::split_route_label(route) else {
            return Vec::new();
        };
        let mut segments = self
            .route_table()
            .mount_paths
            .get(file)
            .cloned()
            .unwrap_or_default();
        segments.extend(crate::hono::path_segments(path));
        let matching = |sites: Vec<RpcSite>| -> Vec<(PathBuf, u32)> {
            sites
                .into_iter()
                .filter(|s| {
                    s.call.method == method && crate::hono::same_route(&s.call.segments, &segments)
                })
                .map(|s| (s.file, s.line))
                .collect()
        };
        let head_raw = matching(self.hono_scan(true).sites);
        let base_raw = matching(self.hono_scan(false).sites);
        debug_log(&format!(
            "rpc callers of {route} at {}: {} head / {} base sites",
            file.display(),
            head_raw.len(),
            base_raw.len()
        ));
        self.callers_from_hits(head_raw, base_raw)
    }

    fn bindings_of(&mut self, file: &std::path::Path) -> &HashMap<String, crate::bindings::Import> {
        if !self.bindings_cache.contains_key(file) {
            let mut map = HashMap::new();
            for rev in [&self.base_rev, &self.head_rev] {
                if let Some(src) = rev.read(&self.root, file) {
                    map.extend(crate::lang::bindings(file, &src));
                }
            }
            self.bindings_cache.insert(file.to_path_buf(), map);
        }
        &self.bindings_cache[file]
    }

    fn reexports_of(&mut self, file: &std::path::Path) -> &HashMap<String, String> {
        if !self.reexports_cache.contains_key(file) {
            let mut map = HashMap::new();
            for rev in [&self.base_rev, &self.head_rev] {
                if let Some(src) = rev.read(&self.root, file)
                    && Lang::of(file) == Some(Lang::TypeScript)
                {
                    map.extend(crate::bindings::namespace_reexports(&src));
                }
            }
            self.reexports_cache.insert(file.to_path_buf(), map);
        }
        &self.reexports_cache[file]
    }

    /// One specifier, as written in `from_file`, to the workspace file it
    /// names — in HEAD, else BASE. `None` for anything outside the
    /// workspace (an npm package).
    fn resolve_spec(&mut self, spec: &str, from_file: &std::path::Path) -> Option<PathBuf> {
        let from_dir = from_file
            .parent()
            .unwrap_or(std::path::Path::new(""))
            .to_path_buf();
        let key = (from_dir.clone(), spec.to_string());
        if let Some(hit) = self.spec_cache.get(&key) {
            return hit.clone();
        }
        let dir = self.root.join(&from_dir);
        let tsconfig = match Lang::of(from_file) {
            Some(Lang::Go | Lang::Python) => Default::default(),
            _ => crate::tsconfig::load_nearest(&self.root, &dir),
        };
        let found = [
            (&self.head_rev, &self.head_ws),
            (&self.base_rev, &self.base_ws),
        ]
        .into_iter()
        .find_map(|(rev, ws)| {
            crate::lang::resolve_files(spec, from_file, &self.root, rev, ws, &tsconfig)
                .into_iter()
                .next()
        });
        debug_log(&format!(
            "resolve_spec {spec} from {} -> {:?}",
            from_file.display(),
            found
        ));
        self.spec_cache.insert(key, found.clone());
        found
    }

    /// The first line `name` appears on as a word, HEAD first.
    fn line_of_name(&self, file: &std::path::Path, name: &str) -> u32 {
        let pattern = regex::Regex::new(&format!(r"\b{}\b", regex::escape(name))).expect("regex");
        [&self.head_rev, &self.base_rev]
            .into_iter()
            .filter_map(|rev| rev.read(&self.root, file))
            .find_map(|src| {
                src.lines()
                    .position(|l| pattern.is_match(l))
                    .map(|i| i as u32 + 1)
            })
            .unwrap_or(1)
    }

    /// The file a node lives in, for grouping inside a box: a function's
    /// own file, or the file an unresolved export was traced to.
    fn node_file(&self, id: &str) -> Option<PathBuf> {
        if is_ext_id(id) {
            return self.call_file.get(id).map(|(f, _)| f.clone());
        }
        Some(split_fn_id(id).0)
    }

    /// Cheap text check that a file exports `name` at all, before paying
    /// for a parse of it: a barrel's sixty imports would otherwise each
    /// be parsed twice to find the one that defines the name.
    fn exports_textually(&self, file: &std::path::Path, name: &str) -> bool {
        [&self.head_rev, &self.base_rev]
            .into_iter()
            .filter_map(|rev| rev.read(&self.root, file))
            .any(|src| crate::lang::defines_textually(file, &src, name))
    }

    /// The file among `file`'s imports that exports a top-level `label`.
    fn resolve_import(&mut self, file: &std::path::Path, label: &str) -> Option<PathBuf> {
        let deps = self.deps_of(file);
        deps.into_iter().find(|d| {
            self.exports_textually(d, label)
                && self.ensure_function_file(d)
                && self.is_importable(d, label)
        })
    }

    /// The file defining Python class `class` — `file` itself, or one it
    /// imports (a package's `__init__.py` re-exporting it) — when that
    /// class has a method `method`; with the method's label
    /// (`Repo.save`).
    fn python_method(
        &mut self,
        file: &std::path::Path,
        class: &str,
        method: &str,
    ) -> Option<(PathBuf, String)> {
        let pattern =
            regex::Regex::new(&format!(r"(?m)^class\s+{}\b", regex::escape(class))).ok()?;
        let defines = |this: &Self, f: &std::path::Path| {
            [&this.head_rev, &this.base_rev]
                .into_iter()
                .filter_map(|rev| rev.read(&this.root, f))
                .any(|src| pattern.is_match(&src))
        };
        let mut candidates = vec![file.to_path_buf()];
        if !defines(self, file) {
            candidates = self.deps_of(file);
        }
        let found = candidates.into_iter().find(|f| defines(self, f))?;
        let label = format!("{class}.{method}");
        let has = self.ensure_function_file(&found)
            && self.function_files[&found]
                .head_fns
                .iter()
                .chain(&self.function_files[&found].base_fns)
                .any(|f| f.method && f.name.as_deref() == Some(label.as_str()));
        has.then_some((found, label))
    }

    /// The file whose class `segments` names (`Repo`, `models.Repo`),
    /// defined in `file` or imported into it, when that class has an
    /// `__init__`.
    fn python_constructor(
        &mut self,
        file: &std::path::Path,
        segments: &[&str],
    ) -> Option<(PathBuf, String)> {
        let class = *segments.last()?;
        if !class.chars().next().is_some_and(char::is_uppercase) {
            return None;
        }
        if segments.len() == 1
            && let Some(hit) = self.python_method(file, class, "__init__")
            && hit.0 == file
        {
            return Some(hit);
        }
        let import = self.bindings_of(file).get(segments[0])?.clone();
        let mut parts: Vec<&str> = match &import.binding {
            crate::bindings::Binding::Named(n) => vec![n.as_str()],
            _ => Vec::new(),
        };
        parts.extend(&segments[1..]);
        let (class, path) = parts.split_last()?;
        let module = path
            .iter()
            .fold(import.specifier.clone(), |m, p| crate::python::join(&m, p));
        let f = self.resolve_spec(&module, file)?;
        self.python_method(&f, class, "__init__")
    }

    /// The files `file` imports, in either revision; tests excluded.
    fn deps_of(&mut self, file: &std::path::Path) -> Vec<PathBuf> {
        if !self.deps_cache.contains_key(file) {
            let mut deps: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for (rev, ws) in [
                (&self.base_rev, &self.base_ws),
                (&self.head_rev, &self.head_ws),
            ] {
                let g =
                    crate::graph::reach(&self.root, rev, file, ImportDirection::Dependencies, ws);
                deps.extend(g.nodes.into_iter().map(|n| n.path));
            }
            let deps: Vec<PathBuf> = deps
                .into_iter()
                .map(PathBuf::from)
                .filter(|p| p != file && !is_test_file(p))
                .collect();
            self.deps_cache.insert(file.to_path_buf(), deps);
        }
        self.deps_cache[file].clone()
    }

    fn collapse(&mut self, id: &str) {
        let mut to_remove = Vec::new();
        let mut frontier = vec![id.to_string()];
        while let Some(cur) = frontier.pop() {
            for (child, (parent, _)) in &self.expanded_from {
                if parent == &cur && !to_remove.contains(child) {
                    to_remove.push(child.clone());
                    frontier.push(child.clone());
                }
            }
        }
        if to_remove.is_empty() {
            self.status = "nothing to collapse from here".into();
            return;
        }
        for r in &to_remove {
            self.expanded_from.remove(r);
        }
        self.edges
            .retain(|(a, b), _| !to_remove.contains(a) && !to_remove.contains(b));
        self.nodes.retain(|n| !to_remove.contains(&n.id));
        if to_remove.contains(&self.selected) {
            self.selected = id.to_string();
        }
        self.came_from = None;
        self.status = format!("collapsed {} node(s)", to_remove.len());
    }

    /// A cross-file caller's group label: the owning workspace package's
    /// name when it has one — a bare filename ("client.ts") is exactly
    /// the ambiguous label a monorepo with several files of that name
    /// breaks — falling back to the file's own directory, then the
    /// filename alone as a last resort.
    fn file_label(&self, file: &std::path::Path) -> String {
        self.head_ws
            .owning_package(file)
            .or_else(|| self.base_ws.owning_package(file))
            .and_then(|p| p.name.clone())
            .or_else(|| {
                file.parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .filter(|p| !p.is_empty())
            })
            .unwrap_or_else(|| basename(&file.to_string_lossy()).to_string())
    }

    fn sort_key(&self, id: &str) -> (i32, String) {
        if self.granularity == Granularity::Function {
            let (file, label) = split_fn_id(id);
            if let Some(entry) = self.function_files.get(&file)
                && let Some((_, f)) = find_function(entry, label)
            {
                return (f.start_line as i32, String::new());
            }
        }
        (0, id.to_string())
    }

    /// Whether the current view draws this node. Unchanged callees are
    /// left out unless `show_unchanged` — and so is anything that was
    /// only reached through one of them. Roots and callers always show.
    fn is_visible(&self, id: &str) -> bool {
        let mut cur = id;
        loop {
            match self.expanded_from.get(cur) {
                None => return true,
                Some((parent, dir)) => {
                    // An unchanged callee stays only when a *changed*
                    // function newly calls it (or stopped calling it):
                    // that is a change in what the code reaches. A brand
                    // new function calls everything "newly", which says
                    // nothing about the callee.
                    if *dir == Dir::Callees
                        && !self.show_unchanged
                        && self
                            .nodes
                            .iter()
                            .any(|n| n.id == cur && n.status == Status::Unchanged)
                        && !self.edges.iter().any(|((a, b), s)| {
                            b == cur
                                && *s != Status::Unchanged
                                && self
                                    .nodes
                                    .iter()
                                    .any(|n| n.id == *a && n.status == Status::Changed)
                        })
                    {
                        return false;
                    }
                    cur = parent;
                }
            }
        }
    }

    fn toggle_unchanged(&mut self) {
        self.show_unchanged = !self.show_unchanged;
        if !self.is_visible(&self.selected) {
            // The selection itself was just hidden — fall back to a root.
            self.selected = self.origins.first().cloned().unwrap_or_default();
        }
        self.status = if self.show_unchanged {
            "showing unchanged callees too — a to hide".into()
        } else {
            "unchanged callees hidden — a to show".into()
        };
    }

    /// Up/down moves among the nodes in the selected node's own column
    /// and stops at either end — no wrap-around, no crossing into the
    /// next column. ←/→ are how you change columns.
    fn move_by_row(&mut self, delta: i32) {
        let geo = self.geometry();
        let Some(&col) = geo.rank.get(&self.selected) else {
            return;
        };
        let ordered = geo.column_nodes(col);
        let Some(at) = ordered.iter().position(|(id, _)| *id == self.selected) else {
            return;
        };
        let next = at as i32 + delta;
        if (0..ordered.len() as i32).contains(&next) {
            self.selected = ordered[next as usize].0.clone();
            return;
        }
        // Past the end of this box's column: the nearest node above or
        // below on screen, in whichever box — boxes stack in a canvas,
        // and ↑/↓ shouldn't stop at a border.
        let Some(&(x, y, w, _)) = geo.rects.get(&self.selected) else {
            return;
        };
        let centre = x + w / 2;
        let best = geo
            .rects
            .iter()
            .filter(|(id, _)| **id != self.selected)
            .filter(|(_, (_, oy, _, _))| if delta > 0 { *oy > y } else { *oy < y })
            .min_by_key(|(id, (ox, oy, ow, _))| {
                let dy = (oy - y).abs();
                let dx = (ox + ow / 2 - centre).abs();
                (dy * 4 + dx, (*id).clone())
            })
            .map(|(id, _)| id.clone());
        if let Some(id) = best {
            self.selected = id;
        }
    }

    /// Left/right follows an edge: to a caller of the selected node, or
    /// to something it calls — the one nearest on screen, or the node
    /// the last ←/→ came from, so pressing the opposite arrow retraces
    /// the step. Nothing on that side yet means "look for some", and
    /// step onto whatever that finds.
    fn move_deeper(&mut self, dir: Dir) {
        let sel = self.selected.clone();
        let mut next = self.neighbor(&sel, dir);
        if next.is_none() && !is_agg_id(&sel) {
            let already_expanded = self
                .expanded_from
                .values()
                .any(|(parent, d)| *parent == sel && *d == dir);
            if !already_expanded {
                self.expand(sel.clone(), dir);
                next = self.neighbor(&sel, dir);
            }
        }
        if let Some(n) = next {
            self.came_from = Some(sel);
            self.selected = n;
        }
    }

    fn neighbor(&self, id: &str, dir: Dir) -> Option<String> {
        let geo = self.geometry();
        let my_row = *geo.rows.get(id)?;
        let mut candidates: Vec<(i32, String)> = geo
            .edges
            .iter()
            .filter_map(|(a, b)| {
                let other = match dir {
                    Dir::Callers if b == id => a,
                    Dir::Callees if a == id => b,
                    _ => return None,
                };
                let row = geo.rows.get(other)?;
                Some(((row - my_row).abs(), other.clone()))
            })
            .collect();
        if let Some(back) = &self.came_from
            && candidates.iter().any(|(_, c)| c == back)
        {
            return Some(back.clone());
        }
        candidates.sort();
        candidates.into_iter().next().map(|(_, c)| c)
    }

    /// Which caller an unresolved call is looked at from: the node ←/→
    /// last came from when that is one of its callers, else the first
    /// caller on record.
    fn ext_caller(&self, id: &str) -> Option<String> {
        if let Some(back) = &self.came_from
            && self.edges.contains_key(&(back.clone(), id.to_string()))
        {
            return Some(back.clone());
        }
        let mut callers: Vec<&String> = self
            .edges
            .keys()
            .filter(|(_, b)| b == id)
            .map(|(a, _)| a)
            .collect();
        callers.sort();
        callers.first().map(|s| s.to_string())
    }

    fn handle_event(&mut self, ev: Event) {
        let k = match ev {
            Event::Key(k) => k,
            Event::Mouse(m) => return self.handle_mouse(m),
            _ => return,
        };
        if k.kind != KeyEventKind::Press {
            return;
        }
        if self.diff_view.is_some() {
            self.handle_diff_event(k.code);
            return;
        }
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        match k.code {
            // Panning: the view moves, the selection stays put.
            KeyCode::Left if shift => self.pan(-PAN_STEP_X, 0),
            KeyCode::Right if shift => self.pan(PAN_STEP_X, 0),
            KeyCode::Up if shift => self.pan(0, -PAN_STEP_Y),
            KeyCode::Down if shift => self.pan(0, PAN_STEP_Y),
            KeyCode::Char('H') => self.pan(-PAN_STEP_X, 0),
            KeyCode::Char('L') => self.pan(PAN_STEP_X, 0),
            KeyCode::Char('K') => self.pan(0, -PAN_STEP_Y),
            KeyCode::Char('J') => self.pan(0, PAN_STEP_Y),
            KeyCode::Char('0') => self.pan(i32::MIN / 2, 0),
            KeyCode::Char('m') => {
                self.show_minimap = !self.show_minimap;
                self.follow = false;
            }
            KeyCode::Char('z') => self.change_lod(1),
            KeyCode::Char('Z') => self.change_lod(-1),
            KeyCode::Char('v') => {
                let paths = self.files_of_view(&self.selected.clone());
                self.toggle_seen(paths);
            }
            KeyCode::Char('V') => {
                let pkg = self.view_pkg(&self.selected);
                let mut paths: Vec<String> = self
                    .nodes
                    .iter()
                    .filter(|n| self.cluster_label(&n.id) == pkg)
                    .filter_map(|n| self.node_file(&n.id))
                    .map(|f| f.to_string_lossy().into_owned())
                    .collect();
                paths.sort();
                paths.dedup();
                self.toggle_seen(paths);
            }
            KeyCode::Char('S') => {
                self.status = match self.push_viewed() {
                    Ok((m, t, n)) => format!("marked {m}/{t} done file(s) Viewed on PR #{n}"),
                    Err(e) => format!("could not mark Viewed: {e}"),
                };
            }
            KeyCode::Char('P') => {
                self.status = match self.pull_viewed() {
                    Ok((i, v, n)) => {
                        format!("PR #{n}: {v} Viewed on GitHub · {i} imported as seen")
                    }
                    Err(e) => format!("could not read Viewed: {e}"),
                };
            }
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_by_row(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by_row(-1),
            KeyCode::Char('h') | KeyCode::Left => self.move_deeper(Dir::Callers),
            KeyCode::Char('l') | KeyCode::Right => self.move_deeper(Dir::Callees),
            KeyCode::Char('a') => self.toggle_unchanged(),
            KeyCode::Char('c') => self.collapse(&self.selected.clone()),
            KeyCode::Char('g') => self.zoom(),
            KeyCode::Enter if is_agg_id(&self.selected) => self.change_lod(-1),
            KeyCode::Enter => self.open_diff(),
            KeyCode::Char('o') if !is_agg_id(&self.selected) => {
                let path = self.selected_path();
                let line = (self.granularity == Granularity::Function)
                    .then(|| self.function_scope(&path))
                    .flatten()
                    .map(|(_, start, _)| start)
                    .unwrap_or(1);
                if !path.as_os_str().is_empty() {
                    self.edit_request = Some((path, line));
                }
            }
            _ => {}
        }
        // Anything that isn't a pan may have moved or revealed the
        // selection; bring the camera back to it before the next frame.
        let is_pan = (shift
            && matches!(
                k.code,
                KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
            ))
            || matches!(
                k.code,
                KeyCode::Char('H' | 'J' | 'K' | 'L' | '0' | 'm' | 'v' | 'V' | 'S' | 'P')
            );
        if !is_pan {
            self.follow = true;
        }
    }

    /// Opens the seen store for this base/head pair (a working-tree head
    /// diffs against base alone) and reads every node file's progress.
    fn open_seen(&mut self) {
        let base = self.base_rev.commit_sha().unwrap_or("HEAD").to_string();
        let head = self.head_rev.commit_sha().map(str::to_string);
        self.seen = Some(crate::seen::SeenStore::open(
            &self.root,
            &base,
            head.as_deref(),
        ));
        self.refresh_seen();
    }

    /// Recomputes the progress cache for every file a node points into.
    pub fn refresh_seen(&mut self) {
        let files: HashSet<PathBuf> = self
            .nodes
            .iter()
            .filter_map(|n| self.node_file(&n.id))
            .collect();
        let Some(seen) = self.seen.as_mut() else {
            return;
        };
        let mut progress = HashMap::new();
        for f in files {
            let p = f.to_string_lossy().into_owned();
            let (s, t) = seen.progress(&p);
            let done = seen.is_done(&p);
            progress.insert(p, (s, t, done));
        }
        self.seen_progress = progress;
    }

    /// The repo paths behind a view node: its own file, or every file
    /// under an aggregate.
    fn files_of_view(&self, id: &str) -> Vec<String> {
        let geo = self.geometry();
        let members = geo
            .members_of
            .get(id)
            .cloned()
            .unwrap_or_else(|| vec![id.to_string()]);
        let mut out: Vec<String> = members
            .iter()
            .filter_map(|m| self.node_file(m))
            .map(|f| f.to_string_lossy().into_owned())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// `v`: every changed line of the selected node's file(s) seen — or,
    /// when they all already are, none.
    fn toggle_seen(&mut self, paths: Vec<String>) {
        let Some(seen) = self.seen.as_mut() else {
            self.status = "no git repository to keep seen marks in".into();
            return;
        };
        if paths.is_empty() {
            self.status = "nothing here has a file to mark".into();
            return;
        }
        let all_done = paths.iter().all(|p| {
            let (s, t) = seen.progress(p);
            t > 0 && s == t
        });
        for p in &paths {
            seen.set_file(p, !all_done);
        }
        self.refresh_seen();
        self.status = format!(
            "{} {} file{} — v again to undo · S to mirror done files to the PR's Viewed",
            if all_done { "unseen" } else { "seen" },
            paths.len(),
            if paths.len() == 1 { "" } else { "s" }
        );
    }

    fn pull_request(&mut self) -> anyhow::Result<crate::github::PullRequest> {
        if let Some(pr) = &self.pr {
            return Ok(pr.clone());
        }
        let pr = crate::github::find_pr(&self.root, self.head_rev.commit_sha(), self.pr_number)?;
        self.pr = Some(pr.clone());
        Ok(pr)
    }

    /// Files with changed nodes that are fully seen and unflagged.
    fn done_files(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .nodes
            .iter()
            .filter(|n| n.status != Status::Unchanged || self.origins.contains(&n.id))
            .filter_map(|n| self.node_file(&n.id))
            .map(|f| f.to_string_lossy().into_owned())
            .filter(|p| self.seen_progress.get(p).is_some_and(|&(_, _, done)| done))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// `S`: tick Viewed on the PR for every done file. Returns
    /// (marked, total, PR number) or the error text.
    pub fn push_viewed(&mut self) -> Result<(usize, usize, u64), String> {
        let pr = self.pull_request().map_err(|e| e.to_string())?;
        let done = self.done_files();
        let mut marked = 0;
        for p in &done {
            if crate::github::mark_viewed(&self.root, &pr, p).is_ok() {
                marked += 1;
            }
        }
        Ok((marked, done.len(), pr.number))
    }

    /// `P`: every file Viewed on the PR becomes fully seen here.
    pub fn pull_viewed(&mut self) -> Result<(usize, usize, u64), String> {
        let pr = self.pull_request().map_err(|e| e.to_string())?;
        let viewed = crate::github::viewed_files(&self.root, &pr).map_err(|e| e.to_string())?;
        let mine: HashSet<String> = self.seen_progress.keys().cloned().collect();
        let mut imported = 0;
        if let Some(seen) = self.seen.as_mut() {
            for p in viewed.iter().filter(|p| mine.contains(*p)) {
                let (s, t) = seen.progress(p);
                if t > 0 && s < t {
                    seen.set_file(p, true);
                    imported += 1;
                }
            }
        }
        self.refresh_seen();
        Ok((imported, viewed.len(), pr.number))
    }

    /// The seen state as the web page's API reports it.
    pub fn seen_json(&self) -> String {
        let mut items: Vec<(&String, &(usize, usize, bool))> = self.seen_progress.iter().collect();
        items.sort();
        let body: Vec<String> = items
            .iter()
            .map(|(p, (s, t, d))| format!("{}:[{s},{t},{d}]", crate::html::json_str(p)))
            .collect();
        format!("{{{}}}", body.join(","))
    }

    /// Toggle by path, for the web page's API.
    pub fn toggle_seen_path(&mut self, path: &str) {
        self.toggle_seen(vec![path.to_string()]);
    }

    /// Selects the changed function drawn nearest the top-left corner.
    fn select_top_left(&mut self) {
        let geo = self.geometry();
        if let Some(first) = self
            .origins
            .iter()
            .filter_map(|id| geo.view_of.get(id))
            .filter_map(|v| geo.rects.get(v).map(|r| (r.1, r.0, v.clone())))
            .min()
        {
            self.selected = first.2;
        }
    }

    /// Sets a package's detail level up front (`--lod name=level`).
    pub fn set_lod(&mut self, pkg: &str, level: u8) {
        self.lod.insert(pkg.to_string(), level.min(LOD_PACKAGE));
        self.select_top_left();
    }

    /// Coarsens (+1) or refines (-1) the detail level of the selected
    /// node's package, keeping the selection on whatever now stands
    /// for it.
    fn change_lod(&mut self, delta: i32) {
        let pkg = self.view_pkg(&self.selected);
        let level =
            (self.lod_of(&pkg) as i32 + delta).clamp(LOD_FUNCTION as i32, LOD_PACKAGE as i32) as u8;
        if level == self.lod_of(&pkg) {
            self.status = format!(
                "{pkg} is already at its {} level",
                if delta > 0 { "coarsest" } else { "finest" }
            );
            return;
        }
        let old = self.selected.clone();
        let old_members = self
            .geometry()
            .members_of
            .get(&old)
            .cloned()
            .unwrap_or_default();
        self.lod.insert(pkg.clone(), level);
        let geo = self.geometry();
        if !geo.rows.contains_key(&old) {
            let first = old_members.first().cloned().unwrap_or_default();
            self.selected = geo.view_of.get(&first).cloned().unwrap_or(old);
        }
        self.came_from = None;
        let name = match level {
            LOD_FUNCTION => "functions",
            LOD_FILE => "files",
            LOD_DIRECTORY => "directories",
            _ => "one node",
        };
        self.status = format!("{pkg}: shown as {name} — z coarser · Z finer · enter opens a group");
    }

    fn pan(&mut self, dx: i32, dy: i32) {
        self.camera.0 = self.camera.0.saturating_add(dx);
        self.camera.1 = self.camera.1.saturating_add(dy);
        self.follow = false;
    }

    /// Wheel and drag pan the canvas; a click selects the node under the
    /// pointer. Shift+wheel (or a horizontal wheel) scrolls sideways,
    /// which is what makes a wide graph usable without walking the
    /// cursor across it column by column.
    fn handle_mouse(&mut self, m: MouseEvent) {
        if let Some(view) = self.diff_view.as_mut() {
            match m.kind {
                MouseEventKind::ScrollDown => view.scroll = view.scroll.saturating_add(3),
                MouseEventKind::ScrollUp => view.scroll = view.scroll.saturating_sub(3),
                _ => {}
            }
            return;
        }
        let shift = m.modifiers.contains(KeyModifiers::SHIFT);
        match m.kind {
            MouseEventKind::ScrollUp if shift => self.pan(-WHEEL_STEP_X, 0),
            MouseEventKind::ScrollDown if shift => self.pan(WHEEL_STEP_X, 0),
            MouseEventKind::ScrollUp => self.pan(0, -WHEEL_STEP_Y),
            MouseEventKind::ScrollDown => self.pan(0, WHEEL_STEP_Y),
            MouseEventKind::ScrollLeft => self.pan(-WHEEL_STEP_X, 0),
            MouseEventKind::ScrollRight => self.pan(WHEEL_STEP_X, 0),
            MouseEventKind::Down(MouseButton::Left) if self.jump_via_minimap(m.column, m.row) => {
                self.drag = None;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.drag = Some((m.column, m.row, false));
            }
            MouseEventKind::Drag(MouseButton::Left) if self.drag.is_none() => {
                self.jump_via_minimap(m.column, m.row);
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((sx, sy, _)) = self.drag {
                    // Dragging the canvas moves it with the pointer, so
                    // the camera moves the other way.
                    self.pan(sx as i32 - m.column as i32, sy as i32 - m.row as i32);
                    self.drag = Some((m.column, m.row, true));
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some((_, _, moved)) = self.drag.take()
                    && !moved
                    && let Some(id) = self.node_at_screen(m.column, m.row)
                {
                    self.selected = id;
                    self.follow = false;
                }
            }
            _ => {}
        }
    }

    /// A press on the minimap centres the view on that spot. True when
    /// the point was on the minimap at all.
    fn jump_via_minimap(&mut self, column: u16, row: u16) -> bool {
        let Some(mm) = self.minimap else { return false };
        let r = mm.inner;
        if column < r.x || column >= r.x + r.width || row < r.y || row >= r.y + r.height {
            return false;
        }
        let wx = (column - r.x) as i32 * mm.scale_x + mm.scale_x / 2;
        let wy = (row - r.y) as i32 * mm.scale_y + mm.scale_y / 2;
        let area = self.canvas;
        self.camera = (
            wx - area.width as i32 / 2,
            wy - HEADER_ROW - (area.height as i32 - HEADER_ROW) / 2,
        );
        self.follow = false;
        true
    }

    fn node_at_screen(&self, column: u16, row: u16) -> Option<String> {
        let area = self.canvas;
        if column < area.x
            || column >= area.x + area.width
            || row < area.y + HEADER_ROW as u16
            || row >= area.y + area.height
        {
            return None;
        }
        let geo = self.geometry();
        let wx = (column - area.x) as i32 + self.camera.0;
        let wy = (row - area.y) as i32 + self.camera.1;
        geo.node_at(wx, wy)
    }

    fn handle_diff_event(&mut self, code: KeyCode) {
        let Some(view) = self.diff_view.as_mut() else {
            return;
        };
        match code {
            KeyCode::Char('v') => {
                let path = self.selected_path().to_string_lossy().into_owned();
                self.toggle_seen(vec![path.clone()]);
                if let Some(view) = self.diff_view.as_mut() {
                    view.seen_note = seen_note(self.seen_progress.get(&path));
                }
            }
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => self.diff_view = None,
            KeyCode::Char('j') | KeyCode::Down => view.scroll = view.scroll.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => view.scroll = view.scroll.saturating_sub(1),
            KeyCode::PageDown => view.scroll = view.scroll.saturating_add(20),
            KeyCode::PageUp => view.scroll = view.scroll.saturating_sub(20),
            _ => {}
        }
    }

    /// Cycles function → file → package → function, re-rooting the graph
    /// (everything expanded so far is dropped) at wherever the new
    /// level's equivalent of the current node is: the file a function
    /// lives in, the package a file lives in, or — zooming back in — the
    /// package's declared entry point, re-extracted for functions.
    fn zoom(&mut self) {
        let current = self.selected.clone();
        if is_ext_id(&current) || is_agg_id(&current) {
            self.status =
                "an unresolved call or a group has no file of its own to zoom into".into();
            return;
        }
        match self.granularity {
            Granularity::Function => {
                // The selected node's own file, not the graph's origin
                // file — once callers can cross into an importing file,
                // zooming out should zoom out of wherever you actually
                // are, not always back to where you started.
                let (file, _) = split_fn_id(&current);
                self.granularity = Granularity::File;
                self.reroot(file.to_string_lossy().into_owned());
            }
            Granularity::File => {
                let path = PathBuf::from(&current);
                let dir = self
                    .head_ws
                    .owning_package(&path)
                    .or_else(|| self.base_ws.owning_package(&path))
                    .map(|p| p.dir.clone());
                let Some(dir) = dir else {
                    self.status = format!("{} is not in any workspace package", path.display());
                    return;
                };
                self.granularity = Granularity::Package;
                self.reroot(dir.to_string_lossy().into_owned());
            }
            Granularity::Package => {
                let dir = PathBuf::from(&current);
                let entry = self
                    .head_ws
                    .packages
                    .iter()
                    .chain(&self.base_ws.packages)
                    .find(|p| p.dir == dir)
                    .and_then(|p| {
                        p.entries.iter().find(|e| {
                            self.head_rev.exists(&self.root, e)
                                || self.base_rev.exists(&self.root, e)
                        })
                    })
                    .cloned();
                let Some(entry) = entry else {
                    self.status = format!("no entry file found for {}", dir.display());
                    return;
                };
                let Some((base_fns, head_fns, alignment, focus)) =
                    load_functions(&self.root, &self.base_rev, &self.head_rev, &entry)
                else {
                    self.status = format!("no functions found in {}", entry.display());
                    return;
                };
                self.function_files.insert(
                    entry.clone(),
                    FunctionFileEntry {
                        base_fns,
                        head_fns,
                        alignment,
                    },
                );
                self.granularity = Granularity::Function;
                self.reroot(make_fn_id(&entry, &focus));
            }
        }
    }

    /// A hub fragment's `label` is a bare identity — a function name with
    /// no line number, a file's basename with no directory, a package
    /// name that reads fine on its own. Only the first two leave a
    /// reviewer unable to say *where* in the codebase the node is, so
    /// those two get enriched; the package name is left as the hub gave
    /// it.
    fn node_display(&self, id: &str, label: &str, drillable: bool) -> String {
        match self.granularity {
            Granularity::Function if label.starts_with("module@") => self.function_display(id),
            Granularity::Function if is_ext_id(id) => {
                if self.call_file.contains_key(id) {
                    format!("{label} [exported here, body not found]")
                } else {
                    format!("{label} [external]")
                }
            }
            Granularity::Function if !drillable => {
                // Nothing in any parsed file matches this call text, so
                // there's no line number to show — just the call itself,
                // marked as a dead end rather than silently vanishing.
                format!("{label} [external]")
            }
            Granularity::Function => self.function_display(id),
            Granularity::File => shorten_path(id, MAX_COL_WIDTH as usize),
            Granularity::Package => label.to_string(),
        }
    }

    fn function_display(&self, id: &str) -> String {
        let (file, label) = split_fn_id(id);
        if let Some(n) = label.strip_prefix("module@") {
            // A reference outside any function — say, an entry in a
            // step table — named by its file, since "module level"
            // alone says nothing about which module.
            return format!(
                "{} · module level · L{n}",
                basename(&file.to_string_lossy())
            );
        }
        let Some(entry) = self.function_files.get(&file) else {
            return label.to_string();
        };
        let shown = describe_function_label(&entry.head_fns, &entry.base_fns, label);
        // A route handler shows its full mounted path — `DELETE
        // /api/v1/folders/:folderId`, not the router-relative
        // `/:folderId` — once the mount table is known.
        if let Some((_, f)) = find_function(entry, label)
            && let Some(route) = &f.route
            && let Some((method, path)) = crate::hono::split_route_label(route)
            && let Some(prefix) = self.routes.as_ref().and_then(|t| t.mount_paths.get(&file))
            && !prefix.is_empty()
        {
            let full = format!("{method} /{}{path}", prefix.join("/"));
            return shown.replacen(route, &full, 1);
        }
        shown
    }

    fn reroot(&mut self, id: String) {
        let hub = self.hub_for(&id);
        let status = hub
            .as_ref()
            .map(|h| h.focus.status)
            .unwrap_or(Status::Unchanged);
        let display = match self.granularity {
            Granularity::Function => self.function_display(&id),
            Granularity::File => shorten_path(&id, MAX_COL_WIDTH as usize),
            Granularity::Package => hub
                .as_ref()
                .map(|h| h.focus.label.clone())
                .unwrap_or_else(|| basename(&id).to_string()),
        };
        self.nodes = vec![GNode {
            id: id.clone(),
            display,
            status,
            drillable: true,
        }];
        self.edges.clear();
        self.came_from = None;
        self.expanded_from.clear();
        self.call_order.clear();
        self.no_callers.clear();
        self.origins = vec![id.clone()];
        self.selected = id.clone();
        self.expand(id.clone(), Dir::Callers);
        self.expand(id, Dir::Callees);
    }

    /// The file the *selected* node lives in, not necessarily the
    /// graph's origin file — once function-level callers can cross into
    /// an importing file, Enter must open whichever file the selection
    /// actually points at.
    fn selected_path(&self) -> PathBuf {
        match self.granularity {
            Granularity::Function => {
                if let Some((f, _)) = self.call_file.get(&self.selected) {
                    return f.clone();
                }
                let id = if is_ext_id(&self.selected) {
                    self.ext_caller(&self.selected).unwrap_or_default()
                } else {
                    self.selected.clone()
                };
                split_fn_id(&id).0
            }
            Granularity::File => PathBuf::from(&self.selected),
            Granularity::Package => PathBuf::from(&self.selected).join("package.json"),
        }
    }

    /// Builds and opens a full-screen unified diff of the *whole file*,
    /// in place of an external viewer — no dependency on anything beyond
    /// the terminal itself. Deliberately not cropped to the selected
    /// function: a reviewer asked for exactly this after an earlier,
    /// narrower version of this view turned out to lose the file's own
    /// context — the header instead states which node the diff was
    /// opened from, so "where in the file am I" still has an answer
    /// without shrinking what's shown.
    fn open_diff(&mut self) {
        let path = self.selected_path();
        let base_src = self.base_rev.read(&self.root, &path).unwrap_or_default();
        let head_src = self.head_rev.read(&self.root, &path).unwrap_or_default();

        let scope = (self.granularity == Granularity::Function)
            .then(|| self.function_scope(&path))
            .flatten();
        let lines = diff_lines(&base_src, &head_src);
        let (scroll, landed) = match &scope {
            Some((_, start, end)) => scroll_to_line(&lines, *start, *end),
            None => (0, None),
        };
        let title = match (&scope, landed) {
            (Some((label, ..)), Some(n)) => {
                format!("{label}  ·  {}  ·  first change at L{n}", path.display())
            }
            (Some((label, ..)), None) => {
                format!("{label}  ·  {}  ·  no change inside", path.display())
            }
            (None, _) => path.display().to_string(),
        };
        let seen_note = seen_note(self.seen_progress.get(&path.to_string_lossy().into_owned()));
        self.diff_view = Some(DiffView {
            title,
            lines,
            scroll,
            seen_note,
        });
    }

    /// The selected function's own display label and its line range in
    /// HEAD — the label for the diff view's header ("diff of what,
    /// specifically"), the range to look for its change in — without
    /// touching what text actually gets diffed.
    fn function_scope(&self, file: &std::path::Path) -> Option<(String, u32, u32)> {
        let (_, label) = split_fn_id(&self.selected);
        // A module-level reference (a step-table entry, say): the line
        // it sits on is its whole scope.
        if let Some(n) = label
            .strip_prefix("module@")
            .and_then(|n| n.parse::<u32>().ok())
        {
            return Some((
                format!(
                    "{} · module level · L{n}",
                    basename(&file.to_string_lossy())
                ),
                n,
                n,
            ));
        }
        if let Some((f, line)) = self.call_file.get(&self.selected) {
            let (_, rest) = split_pkg_id(&self.selected).unwrap_or(("", ""));
            return Some((
                format!("{rest} · exported from {}", basename(&f.to_string_lossy())),
                *line,
                *line,
            ));
        }
        let entry = self.function_files.get(file)?;
        if !is_ext_id(&self.selected)
            && let Some(head_f) = entry.head_fns.iter().find(|f| align::label(f) == label)
        {
            return Some((
                describe_ts_function(&entry.head_fns, head_f),
                head_f.start_line,
                head_f.end_line,
            ));
        }

        // Not a function of its own — an unresolved call such as
        // `db.transaction [external]`. Its place in the file is the line
        // it's called on, inside the function it was expanded from.
        let parent = self.ext_caller(&self.selected)?;
        let label = self
            .call_text
            .get(&self.selected)
            .map(String::as_str)
            .unwrap_or(label);
        let (_, parent_label) = split_fn_id(&parent);
        let parent_idx = entry
            .head_fns
            .iter()
            .position(|f| align::label(f) == parent_label)?;
        let parent_f = &entry.head_fns[parent_idx];
        let called_from = format!(
            "{label} [external] — called from {}",
            describe_ts_function(&entry.head_fns, parent_f)
        );
        // The call may sit inside one of the parent's folded-in
        // anonymous callbacks; `inlined_calls` covers those too.
        match inlined_calls(&entry.head_fns, parent_idx)
            .into_iter()
            .find(|(c, _)| c == label)
        {
            Some((_, line)) => Some((called_from, line, line)),
            // Only called in BASE (a removed call): its line is gone
            // from HEAD, so land on the caller's first change instead.
            None => Some((called_from, parent_f.start_line, parent_f.end_line)),
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        if let Some(view) = &self.diff_view {
            draw_diff(f, area, view);
            return;
        }
        const TOP_ROWS: u16 = 2;
        const IMPACT_ROWS: u16 = 2;
        // The key hint wraps onto a second row in a narrow terminal
        // rather than losing its tail.
        let key_rows: u16 = if KEY_HINT.chars().count() as u16 > area.width {
            2
        } else {
            1
        };
        let footer_rows: u16 = 2 + key_rows;

        let top = Rect {
            height: TOP_ROWS,
            ..area
        };
        let impact = Rect {
            y: top.y + TOP_ROWS,
            height: IMPACT_ROWS,
            ..area
        };
        let footer_y = area.y + area.height.saturating_sub(footer_rows);
        let status = Rect {
            height: 1,
            y: footer_y,
            ..area
        };
        let keys = Rect {
            height: key_rows,
            y: footer_y + 1,
            ..area
        };
        let legend = Rect {
            height: 1,
            y: footer_y + 1 + key_rows,
            ..area
        };
        let diagram = Rect {
            y: impact.y + IMPACT_ROWS,
            height: area
                .height
                .saturating_sub(TOP_ROWS + IMPACT_ROWS + footer_rows),
            ..area
        };

        let level = match self.granularity {
            Granularity::Function => "function",
            Granularity::File => "file",
            Granularity::Package => "package",
        };
        let location = match (self.granularity, self.origins.as_slice()) {
            (_, []) => String::new(),
            (Granularity::Function, [only]) => {
                let (file, _) = split_fn_id(only);
                format!("{}  ·  {}", self.function_display(only), file.display())
            }
            (Granularity::File | Granularity::Package, [only]) => only.clone(),
            (_, many) => {
                let files: HashSet<PathBuf> = many.iter().map(|id| split_fn_id(id).0).collect();
                format!(
                    "whole diff — {} changed function(s) across {} file(s)",
                    many.len(),
                    files.len()
                )
            }
        };
        f.render_widget(
            Paragraph::new(vec![
                ratatui::text::Line::from(format!("[{level}] {location}")),
                ratatui::text::Line::from(format!(
                    "base {}  →  head {}",
                    self.base_rev.label(),
                    self.head_rev.label()
                )),
            ]),
            top,
        );
        f.render_widget(
            Paragraph::new(self.impact_summary())
                .wrap(ratatui::widgets::Wrap { trim: true })
                .style(Style::default().add_modifier(Modifier::BOLD)),
            impact,
        );
        f.render_widget(Paragraph::new(self.status.as_str()), status);
        f.render_widget(
            Paragraph::new(KEY_HINT)
                .wrap(ratatui::widgets::Wrap { trim: true })
                .style(Style::default().fg(Color::DarkGray)),
            keys,
        );
        f.render_widget(
            Paragraph::new(if crate::color::enabled() {
                "colors: yellow = body changed · green = added · red = removed · gray = unchanged · green/red line = call added/removed · magenta line = the selected node's call chains (bright = its own calls)"
            } else {
                "no colour: underlined = changed, added or removed · dim = unchanged · reversed = selected · Enter on a node shows what changed"
            })
                .style(Style::default().fg(Color::DarkGray)),
            legend,
        );
        let geo = self.geometry();
        self.canvas = diagram;
        self.aim_camera(&geo, diagram);
        self.minimap = minimap_layout(&geo, diagram, self.show_minimap);
        f.render_widget(
            GraphWidget {
                app: self,
                geo: &geo,
            },
            diagram,
        );
    }

    /// Keeps the camera inside the world, and — after a keyboard move —
    /// just far enough over that the selected node is on screen. Pans
    /// by mouse or Shift+arrows never trigger the follow, so the view
    /// can be taken anywhere and stays there.
    fn aim_camera(&mut self, geo: &Geometry, area: Rect) {
        let (view_w, view_h) = viewport(geo, area);
        if self.follow {
            self.follow = false;
            if let Some((x, y, w)) = geo.node_rect(&self.selected) {
                let y = y - HEADER_ROW;
                if x + w > self.camera.0 + view_w {
                    self.camera.0 = x + w - view_w;
                }
                if x < self.camera.0 {
                    self.camera.0 = x;
                }
                if y >= self.camera.1 + view_h {
                    self.camera.1 = y - view_h + 1;
                }
                if y < self.camera.1 {
                    self.camera.1 = y;
                }
            }
        }
        self.camera.0 = self.camera.0.clamp(0, (geo.world_w - view_w).max(0));
        self.camera.1 = self
            .camera
            .1
            .clamp(0, (geo.world_h - HEADER_ROW - view_h).max(0));
    }

    /// Names, not just counts: the whole point of the tool is "where does
    /// this reach", so the answer belongs in plain sight instead of
    /// requiring the reviewer to decode the diagram first.
    fn impact_summary(&self) -> String {
        let mut removed = Vec::new();
        let mut added = Vec::new();
        let mut changed = Vec::new();
        for n in &self.nodes {
            if self.origins.contains(&n.id) {
                continue;
            }
            let name = match split_pkg_id(&n.id) {
                Some((pkg, rest)) => format!("{rest} [{pkg}]"),
                None => n.display.clone(),
            };
            match n.status {
                Status::Removed => removed.push(name),
                Status::Added => added.push(name),
                Status::Changed => changed.push(name),
                Status::Unchanged => {}
            }
        }
        let group = |label: &str, names: &[String]| -> Option<String> {
            if names.is_empty() {
                return None;
            }
            let shown: Vec<&str> = names.iter().take(4).map(String::as_str).collect();
            let more = names.len() - shown.len();
            let mut s = format!("{label}: {}", shown.join(", "));
            if more > 0 {
                s.push_str(&format!(" (+{more} more)"));
            }
            Some(s)
        };
        let parts: Vec<String> = [
            group("removed", &removed),
            group("added", &added),
            group("changed", &changed),
        ]
        .into_iter()
        .flatten()
        .collect();
        if parts.is_empty() {
            "no changes reached yet — press ←/→ on the selected node to step outward".into()
        } else {
            format!("impact so far — {}", parts.join("   "))
        }
    }
}

/// Where everything sits on the canvas, in world cells, before any
/// scrolling. Every package is a box of its own, placed freely: boxes
/// are laid out left to right by who calls whom (a package sits one
/// column right of the packages that call it) and stacked near their
/// callers, so frontend, backend and database end up side by side
/// instead of in far-apart bands. Inside a box the package's functions
/// are grouped by directory and file and arranged left to right by
/// call depth *within the package*. Calls between packages leave a box
/// through its right border and enter the next through its left one —
/// via a channel row at the box's bottom or top when the function
/// isn't in the box's last or first column — so no line ever runs
/// through another node's text. World row 0 is a caption.
struct Geometry {
    /// The nodes as the current detail levels show them: real
    /// functions, or a file / directory / whole package standing in
    /// for its functions where a package is set coarser.
    nodes: Vec<ViewNode>,
    /// The edges between those, deduplicated.
    edges: Vec<(String, String)>,
    /// Real node → the view node standing in for it.
    view_of: HashMap<String, String>,
    /// View node → the real nodes it stands in for (itself when real).
    members_of: HashMap<String, Vec<String>>,
    /// A column id per visible node — which box, which depth column in
    /// it — so ↑/↓ move within one stack.
    rank: HashMap<String, usize>,
    /// World row per visible node.
    rows: HashMap<String, i32>,
    /// (x, y, width) per visible node.
    rects: HashMap<String, (i32, i32, i32, i32)>,
    /// File headers: (x, row, label, width).
    headers: Vec<(i32, i32, String, i32)>,
    /// Directory headers: (x, row, label).
    dir_headers: Vec<(i32, i32, String)>,
    boxes: Vec<PkgBox>,
    legs: Vec<Leg>,
    world_w: i32,
    world_h: i32,
}

/// A node as drawn: a real function, or an aggregate at the file,
/// directory or package level.
#[derive(Clone)]
struct ViewNode {
    id: String,
    display: String,
    status: Status,
    pkg: String,
    /// Directory band inside the box, when the node belongs to one.
    dir: Option<String>,
    /// File header inside the band, when the node is a function.
    file: Option<PathBuf>,
    /// Ordering within a stack: (call order, line, name).
    order: (usize, i32, String),
    /// Text width, suffix included.
    len: i32,
    /// A second, dimmer line under the label (an aggregate's counts).
    sub: Option<String>,
    origin: bool,
    dim: bool,
}

/// Detail levels a package's box can be shown at.
const LOD_FUNCTION: u8 = 0;
const LOD_FILE: u8 = 1;
const LOD_DIRECTORY: u8 = 2;
const LOD_PACKAGE: u8 = 3;

fn is_agg_id(id: &str) -> bool {
    id.starts_with("agg::")
}

/// One package's box: border cells inclusive.
struct PkgBox {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    title: String,
}

/// One edge as an orthogonal polyline, plus where its arrowhead goes.
struct Leg {
    points: Vec<(i32, i32)>,
    status: Status,
    from: String,
    to: String,
    arrow: Option<(i32, i32)>,
}

/// Cells kept clear inside a box: left border, a blank, the arrowhead
/// cell, a blank, then the first column's text.
const BOX_PAD_LEFT: i32 = 3;
/// A gap's fixed cells besides its trunks: stub, arrowhead, blank.
const GAP_FIXED: i32 = 3;

impl Geometry {
    /// A node's cell: (x, y, width).
    fn node_rect(&self, id: &str) -> Option<(i32, i32, i32)> {
        self.rects.get(id).map(|&(x, y, w, _)| (x, y, w))
    }

    fn node_at(&self, wx: i32, wy: i32) -> Option<String> {
        self.rects
            .iter()
            .find(|(_, r)| wy >= r.1 && wy < r.1 + r.3 && wx >= r.0 && wx < r.0 + r.2)
            .map(|(id, _)| id.clone())
    }

    /// The nodes of one stack, top to bottom.
    fn column_nodes(&self, col: usize) -> Vec<(String, i32)> {
        let mut out: Vec<(String, i32)> = self
            .rows
            .iter()
            .filter(|(id, _)| self.rank.get(*id) == Some(&col))
            .map(|(id, r)| (id.clone(), *r))
            .collect();
        out.sort_by_key(|(_, r)| *r);
        out
    }
}

/// What `--impact` reports.
pub struct Impact {
    /// (package, files, functions) with changed functions.
    pub changed: Vec<(String, usize, usize)>,
    /// (package, functions) that call into changed code, transitively,
    /// without changes of their own.
    pub affected: Vec<(String, usize)>,
    pub crossings: Vec<ImpactCrossing>,
}

/// Calls from one package into another, as seen from the changed code.
pub struct ImpactCrossing {
    pub from: String,
    pub to: String,
    pub calls: usize,
    pub changed_calls: usize,
    /// Route handlers among the callees (`DELETE /api/v1/…`).
    pub routes: Vec<String>,
}

impl Impact {
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str("changed:\n");
        for (pkg, files, fns) in &self.changed {
            out.push_str(&format!(
                "  {pkg:<32} {files} file{} · {fns} function{}\n",
                if *files == 1 { "" } else { "s" },
                if *fns == 1 { "" } else { "s" }
            ));
        }
        if self.affected.is_empty() {
            out.push_str("affected upstream: none found (nothing outside the changed packages calls the changed code)\n");
        } else {
            out.push_str("affected upstream (calls into changed code, no changes of its own):\n");
            for (pkg, fns) in &self.affected {
                out.push_str(&format!(
                    "  {pkg:<32} {fns} function{}\n",
                    if *fns == 1 { "" } else { "s" }
                ));
            }
        }
        if !self.crossings.is_empty() {
            out.push_str("calls across packages (from the changed code):\n");
            for c in &self.crossings {
                out.push_str(&format!(
                    "  {} → {}: {} call{}{}\n",
                    c.from,
                    c.to,
                    c.calls,
                    if c.calls == 1 { "" } else { "s" },
                    if c.changed_calls > 0 {
                        format!(" ({} added/removed)", c.changed_calls)
                    } else {
                        String::new()
                    }
                ));
                for r in &c.routes {
                    out.push_str(&format!("      {r}\n"));
                }
            }
        }
        out
    }

    pub fn to_json(&self) -> String {
        let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let changed: Vec<String> = self
            .changed
            .iter()
            .map(|(p, f, n)| format!("{{\"package\":{},\"files\":{f},\"functions\":{n}}}", q(p)))
            .collect();
        let affected: Vec<String> = self
            .affected
            .iter()
            .map(|(p, n)| format!("{{\"package\":{},\"functions\":{n}}}", q(p)))
            .collect();
        let crossings: Vec<String> = self
            .crossings
            .iter()
            .map(|c| {
                let routes: Vec<String> = c.routes.iter().map(|r| q(r)).collect();
                format!(
                    "{{\"from\":{},\"to\":{},\"calls\":{},\"changed_calls\":{},\"routes\":[{}]}}",
                    q(&c.from),
                    q(&c.to),
                    c.calls,
                    c.changed_calls,
                    routes.join(",")
                )
            })
            .collect();
        format!(
            "{{\"changed\":[{}],\"affected\":[{}],\"crossings\":[{}]}}\n",
            changed.join(","),
            affected.join(","),
            crossings.join(",")
        )
    }
}

/// Longest-path depth from the sources, with cycles cut: a depth-first
/// walk orders the nodes topologically ignoring back edges, then one
/// relaxation pass along that order. Plain relaxation on a graph with a
/// cycle (mutual recursion, a strategy table calling back) would keep
/// bumping ranks until its iteration cap — four hundred columns for one
/// package, once.
fn longest_path_ranks(ids: &[String], edges: &[(String, String)]) -> HashMap<String, usize> {
    let index: HashMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); ids.len()];
    for (a, b) in edges {
        if let (Some(&i), Some(&j)) = (index.get(a.as_str()), index.get(b.as_str())) {
            out[i].push(j);
        }
    }
    // Iterative DFS: 0 = unseen, 1 = on the stack, 2 = done.
    let mut color = vec![0u8; ids.len()];
    let mut order: Vec<usize> = Vec::new();
    for start in 0..ids.len() {
        if color[start] != 0 {
            continue;
        }
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        color[start] = 1;
        while let Some(&mut (v, ref mut next)) = stack.last_mut() {
            if *next < out[v].len() {
                let w = out[v][*next];
                *next += 1;
                if color[w] == 0 {
                    color[w] = 1;
                    stack.push((w, 0));
                }
            } else {
                color[v] = 2;
                order.push(v);
                stack.pop();
            }
        }
    }
    order.reverse();
    let mut pos = vec![0usize; ids.len()];
    for (k, &v) in order.iter().enumerate() {
        pos[v] = k;
    }
    let mut rank = vec![0usize; ids.len()];
    for &v in &order {
        for &w in &out[v] {
            if pos[w] > pos[v] && rank[w] < rank[v] + 1 {
                rank[w] = rank[v] + 1;
            }
        }
    }
    ids.iter()
        .enumerate()
        .map(|(i, id)| (id.clone(), rank[i]))
        .collect()
}

/// A relative polyline with its status, endpoints and arrowhead cell.
type LocalLeg = (Vec<(i32, i32)>, Status, String, String, (i32, i32));
/// An exit path and the row it leaves the box on.
type Exit = (Vec<(i32, i32)>, i32);
/// An entry path, its arrowhead cell, and the row it enters the box on.
type Entry = (Vec<(i32, i32)>, (i32, i32), i32);
/// An intra-box edge with its legs: (gap, trunk key, from row, to row).
type Chain<'a> = (&'a (String, String, Status), Vec<(usize, String, i32, i32)>);

/// One package box laid out on its own, in coordinates relative to its
/// top-left border cell, before the boxes are placed on the canvas.
struct LocalBox {
    name: String,
    pkg_col: usize,
    members: Vec<String>,
    lrank: HashMap<String, usize>,
    col_x: Vec<i32>,
    col_w: Vec<i32>,
    rows: HashMap<String, i32>,
    headers: Vec<(usize, i32, String)>,
    /// Directory band headers: (column, row, label) — repeated in every
    /// column the band has nodes in, so a node far to the right still
    /// has its directory right above it.
    dir_headers: Vec<(usize, i32, String)>,
    /// Intra-box legs, relative.
    legs: Vec<LocalLeg>,
    /// Per outgoing inter-package edge: the polyline from the caller's
    /// text to the right border, and the row it leaves on.
    exits: HashMap<(String, String), Exit>,
    /// Per incoming inter-package edge: the polyline from the left
    /// border to the callee, its arrowhead, and the row it enters on.
    entries: HashMap<(String, String), Entry>,
    width: i32,
    height: i32,
}

impl App {
    /// What box a node belongs in: the owning package (function and file
    /// level), or the package an unresolved call was traced to.
    fn cluster_label(&self, id: &str) -> String {
        match self.granularity {
            Granularity::Function => {
                if let Some((pkg, _)) = split_pkg_id(id) {
                    return pkg.to_string();
                }
                let (file, _) = split_fn_id(id);
                self.file_label(&file)
            }
            Granularity::File => self.file_label(std::path::Path::new(id)),
            Granularity::Package => "packages".to_string(),
        }
    }

    /// A file's path inside its package — the box already names the
    /// package, so `src/routes/orders/OrderController.ts` says the rest
    /// without repeating `apps/shop/backend/api`.
    fn path_in_package(&self, file: &std::path::Path) -> String {
        let pkg_dir = self
            .head_ws
            .owning_package(file)
            .or_else(|| self.base_ws.owning_package(file))
            .map(|p| p.dir.clone());
        match pkg_dir.as_deref().and_then(|d| file.strip_prefix(d).ok()) {
            Some(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
            _ => file.to_string_lossy().into_owned(),
        }
    }

    /// The directory of a file inside its package: `src/routes/folders`.
    fn dir_in_package(&self, file: &std::path::Path) -> String {
        let rel = self.path_in_package(file);
        match rel.rsplit_once('/') {
            Some((dir, _)) => dir.to_string(),
            None => String::new(),
        }
    }

    /// How wide a node's text is.
    fn label_len(&self, id: &str) -> i32 {
        self.nodes
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.display.chars().count() as i32)
            .unwrap_or(0)
    }

    fn lod_of(&self, pkg: &str) -> u8 {
        self.lod.get(pkg).copied().unwrap_or(LOD_FUNCTION)
    }

    /// The package a view id belongs to.
    fn view_pkg(&self, id: &str) -> String {
        match id.strip_prefix("agg::").and_then(|r| r.split_once("::")) {
            Some((pkg, _)) => pkg.to_string(),
            None => self.cluster_label(id),
        }
    }

    /// The visible graph at the current detail levels: every real node
    /// whose package is at function level as itself; the rest folded
    /// into one node per file, directory or package. Edges follow, one
    /// per pair, with the strongest status among the calls they stand
    /// for (added or removed wins over changed wins over unchanged).
    #[allow(clippy::type_complexity)]
    fn view(
        &self,
    ) -> (
        Vec<ViewNode>,
        Vec<(String, String, Status)>,
        HashMap<String, String>,
        HashMap<String, Vec<String>>,
    ) {
        let visible: Vec<&GNode> = self
            .nodes
            .iter()
            .filter(|n| self.is_visible(&n.id))
            .collect();
        let mut view: Vec<ViewNode> = Vec::new();
        let mut view_of: HashMap<String, String> = HashMap::new();
        let mut members_of: HashMap<String, Vec<String>> = HashMap::new();
        for n in &visible {
            let pkg = self.cluster_label(&n.id);
            let level = self.lod_of(&pkg);
            let file = self.node_file(&n.id);
            let dir = file.as_ref().map(|f| self.dir_in_package(f));
            let order = (
                self.call_order.get(&n.id).copied().unwrap_or(usize::MAX),
                self.sort_key(&n.id).0,
                self.sort_key(&n.id).1,
            );
            if level == LOD_FUNCTION {
                view.push(ViewNode {
                    id: n.id.clone(),
                    display: n.display.clone(),
                    status: n.status,
                    pkg: pkg.clone(),
                    dir,
                    file,
                    order,
                    len: self.label_len(&n.id),
                    sub: None,
                    origin: self.origins.contains(&n.id),
                    dim: !n.drillable,
                });
                view_of.insert(n.id.clone(), n.id.clone());
                members_of.insert(n.id.clone(), vec![n.id.clone()]);
                continue;
            }
            let key = match level {
                LOD_FILE => file
                    .as_ref()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| n.id.clone()),
                LOD_DIRECTORY => dir.clone().unwrap_or_default(),
                _ => String::new(),
            };
            let id = format!("agg::{pkg}::{level}::{key}");
            view_of.insert(n.id.clone(), id.clone());
            members_of.entry(id.clone()).or_default().push(n.id.clone());
            if view.iter().any(|v| v.id == id) {
                continue;
            }
            // Aggregates keep the order their first member appeared in
            // (roots first, in file order) — line numbers across files
            // would say nothing.
            let appearance = visible.iter().position(|v| v.id == n.id).unwrap_or(0);
            view.push(ViewNode {
                id,
                display: String::new(),
                status: Status::Unchanged,
                pkg: pkg.clone(),
                dir: if level == LOD_FILE { dir } else { None },
                file: None,
                order: (appearance, 0, String::new()),
                len: 0,
                sub: None,
                origin: false,
                dim: false,
            });
        }
        // Aggregates: label, status and origin from their members.
        for v in view.iter_mut().filter(|v| is_agg_id(&v.id)) {
            let members = &members_of[&v.id];
            let statuses: Vec<Status> = members
                .iter()
                .filter_map(|m| self.nodes.iter().find(|n| n.id == *m))
                .map(|n| n.status)
                .collect();
            v.status = if !statuses.is_empty() && statuses.iter().all(|s| *s == Status::Added) {
                Status::Added
            } else if !statuses.is_empty() && statuses.iter().all(|s| *s == Status::Removed) {
                Status::Removed
            } else if statuses.iter().any(|s| *s != Status::Unchanged) {
                Status::Changed
            } else {
                Status::Unchanged
            };
            v.origin = members.iter().any(|m| self.origins.contains(m));
            let files: HashSet<PathBuf> =
                members.iter().filter_map(|m| self.node_file(m)).collect();
            let fns = members.len();
            let (_, rest) =
                v.id.strip_prefix("agg::")
                    .unwrap()
                    .split_once("::")
                    .unwrap();
            let (level, key) = rest.split_once("::").unwrap();
            v.display = match level {
                "1" => format!(
                    "{} · {fns} function{}",
                    basename(key),
                    if fns == 1 { "" } else { "s" }
                ),
                "2" => {
                    // The directory on its own line, the counts under
                    // it — one long line per directory read as clutter.
                    v.sub = Some(format!(
                        "{} file{} · {fns} function{}",
                        files.len(),
                        if files.len() == 1 { "" } else { "s" },
                        if fns == 1 { "" } else { "s" }
                    ));
                    format!("{}/", if key.is_empty() { "." } else { key })
                }
                _ => format!(
                    "{} file{} · {fns} function{}",
                    files.len(),
                    if files.len() == 1 { "" } else { "s" },
                    if fns == 1 { "" } else { "s" }
                ),
            };
            v.len = v
                .display
                .chars()
                .count()
                .max(v.sub.as_ref().map_or(0, |t| t.chars().count())) as i32;
        }
        // Directory groups share their common parent: the deepest
        // *strict* ancestor that at least two of a package's groups sit
        // under becomes a band header, and each group shows only the
        // rest — `src/lib/components/domain/order-item/` over
        // `detail/`, `split/`, `tree/` instead of the full path six
        // times. A group is never its own band, so it always has a name
        // of its own under the header (no `./`); a directory that holds
        // files of its own as well as changed subdirectories lists in
        // its parent's band while its subdirectories get a band of
        // their own.
        let dir_keys: Vec<(String, String)> = view
            .iter()
            .filter(|v| v.id.starts_with("agg::") && v.id.contains("::2::"))
            .map(|v| {
                (
                    v.pkg.clone(),
                    v.id.rsplit("::").next().unwrap_or("").to_string(),
                )
            })
            .collect();
        for v in view.iter_mut() {
            if !(v.id.starts_with("agg::") && v.id.contains("::2::")) {
                continue;
            }
            let key = v.id.rsplit("::").next().unwrap_or("").to_string();
            let segs: Vec<&str> = key.split('/').filter(|s| !s.is_empty()).collect();
            let siblings: Vec<&String> = dir_keys
                .iter()
                .filter(|(p, _)| *p == v.pkg)
                .map(|(_, k)| k)
                .collect();
            let mut shared: Option<String> = None;
            for n in (1..segs.len()).rev() {
                let prefix = segs[..n].join("/");
                let count = siblings
                    .iter()
                    .filter(|k| **k == &prefix || k.starts_with(&format!("{prefix}/")))
                    .count();
                if count >= 2 {
                    shared = Some(prefix);
                    break;
                }
            }
            if let Some(prefix) = shared {
                let rest = key
                    .strip_prefix(&prefix)
                    .map(|r| r.trim_start_matches('/'))
                    .unwrap_or("");
                v.display = format!("{rest}/");
                v.dir = Some(prefix);
                v.len = v
                    .display
                    .chars()
                    .count()
                    .max(v.sub.as_ref().map_or(0, |t| t.chars().count()))
                    as i32;
            }
        }
        // Edges, mapped and deduplicated.
        let mut edges: Vec<(String, String, Status)> = Vec::new();
        let mut sorted_real: Vec<(&(String, String), &Status)> = self.edges.iter().collect();
        sorted_real.sort_by(|x, y| x.0.cmp(y.0));
        for ((a, b), status) in sorted_real {
            let (Some(va), Some(vb)) = (view_of.get(a), view_of.get(b)) else {
                continue;
            };
            if va == vb {
                continue;
            }
            match edges.iter_mut().find(|(x, y, _)| x == va && y == vb) {
                Some((_, _, s)) => {
                    let rank = |st: Status| match st {
                        Status::Added | Status::Removed => 3,
                        Status::Changed => 2,
                        Status::Unchanged => 1,
                    };
                    if rank(*status) > rank(*s) {
                        *s = *status;
                    }
                }
                None => edges.push((va.clone(), vb.clone(), *status)),
            }
        }
        (view, edges, view_of, members_of)
    }

    /// Lays out one package's box: directory bands, file groups, depth
    /// columns by the calls *inside* the package, and the paths every
    /// inter-package edge takes to reach its border.
    #[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
    fn layout_box(
        &self,
        name: &str,
        pkg_col: usize,
        nodes: Vec<&ViewNode>,
        edges: &[(String, String, Status)],
        pkg_of: &HashMap<String, String>,
    ) -> LocalBox {
        let members: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
        let by_id: HashMap<&str, &ViewNode> = nodes.iter().map(|n| (n.id.as_str(), *n)).collect();
        let member_set: HashSet<&str> = members.iter().map(String::as_str).collect();
        let intra: Vec<&(String, String, Status)> = edges
            .iter()
            .filter(|(a, b, _)| member_set.contains(a.as_str()) && member_set.contains(b.as_str()))
            .collect();
        let outgoing: Vec<&(String, String, Status)> = edges
            .iter()
            .filter(|(a, b, _)| member_set.contains(a.as_str()) && !member_set.contains(b.as_str()))
            .collect();
        let incoming: Vec<&(String, String, Status)> = edges
            .iter()
            .filter(|(a, b, _)| !member_set.contains(a.as_str()) && member_set.contains(b.as_str()))
            .collect();

        // Depth within the package.
        let lrank: HashMap<String, usize> = longest_path_ranks(
            &members,
            &intra
                .iter()
                .map(|(a, b, _)| (a.clone(), b.clone()))
                .collect::<Vec<_>>(),
        );
        let ncols = lrank.values().max().map_or(0, |m| m + 1).max(1);
        // Calls from one function to one other package share a channel
        // row and a trunk (an exit bundle); calls into one function from
        // one package likewise (an entry bundle). The lines split apart
        // only in the gap between the packages.
        let exit_key = |a: &str, b: &str| format!("exit:{a}>{}", pkg_of[b]);
        let entry_key = |a: &str, b: &str| format!("entry:{}>{b}", pkg_of[a]);
        let n_in = incoming
            .iter()
            .filter(|(_, b, _)| lrank[b] != 0)
            .map(|(a, b, _)| entry_key(a, b))
            .collect::<HashSet<_>>()
            .len() as i32;
        let n_out = outgoing
            .iter()
            .filter(|(a, _, _)| lrank[a] != ncols - 1)
            .map(|(a, b, _)| exit_key(a, b))
            .collect::<HashSet<_>>()
            .len() as i32;
        let content_top = 1 + n_in;

        // Rows: directory bands, each with one stack per column.
        let appearance = |id: &str| members.iter().position(|n| n == id).unwrap_or(0);
        let mut bands: Vec<(Option<String>, Vec<String>)> = Vec::new();
        for m in &members {
            let dir = by_id[m.as_str()].dir.clone();
            match bands.iter_mut().find(|(d, _)| *d == dir) {
                Some((_, ids)) => ids.push(m.clone()),
                None => bands.push((dir, vec![m.clone()])),
            }
        }
        bands.sort_by_cached_key(|(_, ids)| {
            ids.iter()
                .map(|id| (lrank[id], appearance(id)))
                .min()
                .unwrap_or((0, 0))
        });
        let show_file_headers = self.granularity == Granularity::Function;
        let mut rows: HashMap<String, i32> = HashMap::new();
        let mut headers: Vec<(usize, i32, String)> = Vec::new();
        let mut dir_headers: Vec<(usize, i32, String)> = Vec::new();
        let mut cursor: Vec<i32> = vec![content_top; ncols];
        let pred_row =
            |a: &str, rows: &HashMap<String, i32>| -> Option<i32> { rows.get(a).copied() };
        for (dir, ids) in &bands {
            let mut sub_top = cursor.iter().copied().max().unwrap_or(content_top);
            if let Some(dir) = dir
                && !dir.is_empty()
            {
                for c in 0..ncols {
                    if ids.iter().any(|id| lrank[id] == c) {
                        dir_headers.push((c, sub_top, dir.clone()));
                    }
                }
                sub_top += 1;
            }
            for y in cursor.iter_mut() {
                *y = sub_top;
            }
            for c in 0..ncols {
                let mut stack: Vec<&String> = ids.iter().filter(|id| lrank[*id] == c).collect();
                if stack.is_empty() {
                    continue;
                }
                stack.sort_by_cached_key(|id| {
                    let v = by_id[id.as_str()];
                    (v.file.clone().unwrap_or_default(), v.order.clone())
                });
                let mut groups: Vec<(Option<PathBuf>, Vec<&String>, f64, usize)> = Vec::new();
                for m in &stack {
                    let file = by_id[m.as_str()].file.clone();
                    let preds: Vec<i32> = intra
                        .iter()
                        .filter(|(_, b, _)| b == *m)
                        .filter_map(|(a, _, _)| pred_row(a, &rows))
                        .collect();
                    let bary = if preds.is_empty() {
                        f64::MAX
                    } else {
                        preds.iter().sum::<i32>() as f64 / preds.len() as f64
                    };
                    match groups.iter_mut().find(|(f, ..)| *f == file) {
                        Some((_, g, b, _)) => {
                            g.push(m);
                            *b = b.min(bary);
                        }
                        None => groups.push((file, vec![m], bary, groups.len())),
                    }
                }
                groups.sort_by(|x, y| x.2.total_cmp(&y.2).then(x.3.cmp(&y.3)));
                // Rows the previous column sends lines out from: a node
                // level with one that does not call it would share the
                // line, so those rows are skipped.
                let blocked: HashSet<i32> = if c == 0 {
                    HashSet::new()
                } else {
                    rows.iter()
                        .filter(|(id, _)| lrank[id.as_str()] == c - 1)
                        .map(|(_, r)| *r)
                        .collect()
                };
                for (file, g, ..) in groups {
                    if show_file_headers && let Some(file) = &file {
                        let mut label = basename(&file.to_string_lossy()).to_string();
                        label.push_str(&seen_note(
                            self.seen_progress.get(&file.to_string_lossy().into_owned()),
                        ));
                        headers.push((c, cursor[c], label));
                        cursor[c] += 1;
                    }
                    for m in g {
                        let mut y = cursor[c];
                        while blocked.contains(&y)
                            && !intra
                                .iter()
                                .any(|(a, b, _)| b == m && pred_row(a, &rows) == Some(y))
                        {
                            y += 1;
                        }
                        rows.insert(m.clone(), y);
                        cursor[c] = y + 1 + i32::from(by_id[m.as_str()].sub.is_some());
                    }
                }
            }
        }
        // Pass-through rows for intra edges that skip a column, below
        // the stacks of the columns they cross.
        let mut dummy_rows: HashMap<(String, String, usize), i32> = HashMap::new();
        for (a, b, _) in &intra {
            let (ra, rb) = (lrank[a], lrank[b]);
            if rb <= ra + 1 {
                continue;
            }
            for c in (ra + 1)..rb {
                let y = cursor[c];
                dummy_rows.insert((a.clone(), b.clone(), c), y);
                cursor[c] = y + 1;
            }
        }
        let content_bottom = cursor.iter().copied().max().unwrap_or(content_top);
        let out_top = content_bottom;
        let height = out_top + n_out + 1;

        // Gap users → trunk slots. Intra sources by span, then exits,
        // then entries.
        let mut gap_users: Vec<Vec<(String, i32)>> = vec![Vec::new(); ncols.saturating_sub(1)];
        let mut key_span: HashMap<(usize, String), i32> = HashMap::new();
        let mut intra_chains: Vec<Chain> = Vec::new();
        for e in &intra {
            let (a, b, _) = e;
            let (ra, rb) = (lrank[a], lrank[b]);
            if rb <= ra {
                continue;
            }
            let mut hops = Vec::new();
            let mut y0 = rows[a];
            for c in ra..rb {
                let y1 = if c + 1 == rb {
                    rows[b]
                } else {
                    dummy_rows[&(a.clone(), b.clone(), c + 1)]
                };
                let key = format!("row:{y0}");
                let span = (y1 - y0).abs();
                let k = (c, key.clone());
                key_span
                    .entry(k)
                    .and_modify(|s| *s = (*s).max(span))
                    .or_insert(span);
                hops.push((c, key, y0, y1));
                y0 = y1;
            }
            intra_chains.push((e, hops));
        }
        let mut exits_via: Vec<(&(String, String, Status), usize)> = Vec::new();
        for e in &outgoing {
            let la = lrank[&e.0];
            if la != ncols - 1 {
                exits_via.push((e, la));
            }
        }
        let mut entries_via: Vec<(&(String, String, Status), usize)> = Vec::new();
        for e in &incoming {
            let lb = lrank[&e.1];
            if lb != 0 {
                entries_via.push((e, lb - 1));
            }
        }
        for g in 0..gap_users.len() {
            let mut intra_keys: Vec<(String, i32)> = key_span
                .iter()
                .filter(|((gg, _), _)| *gg == g)
                .map(|((_, k), s)| (k.clone(), *s))
                .collect();
            intra_keys.sort_by(|x, y| x.1.cmp(&y.1).then(x.0.cmp(&y.0)));
            for (k, _) in intra_keys {
                gap_users[g].push((k, 0));
            }
            for (e, gg) in &exits_via {
                let key = exit_key(&e.0, &e.1);
                if *gg == g && !gap_users[g].iter().any(|(k, _)| *k == key) {
                    gap_users[g].push((key, 0));
                }
            }
            for (e, gg) in &entries_via {
                let key = entry_key(&e.0, &e.1);
                if *gg == g && !gap_users[g].iter().any(|(k, _)| *k == key) {
                    gap_users[g].push((key, 0));
                }
            }
        }
        // Column x positions.
        let mut col_w: Vec<i32> = vec![MIN_COL_WIDTH as i32 / 2; ncols];
        for m in &members {
            let c = lrank[m];
            col_w[c] = col_w[c].max(by_id[m.as_str()].len);
        }
        for (c, _, label) in &headers {
            col_w[*c] = col_w[*c].max(label.chars().count() as i32 + 2);
        }
        for w in &mut col_w {
            *w = (*w).min(MAX_COL_WIDTH as i32);
        }
        let mut col_x: Vec<i32> = Vec::new();
        let mut x = BOX_PAD_LEFT;
        for c in 0..ncols {
            col_x.push(x);
            x += col_w[c];
            if c + 1 < ncols {
                x += GAP_FIXED + gap_users[c].len() as i32;
            }
        }
        let title_w = name.chars().count() as i32 + 4;
        let dir_w = dir_headers
            .iter()
            .filter(|(c, ..)| *c == 0)
            .map(|(_, _, d)| d.chars().count() as i32 + 4)
            .max()
            .unwrap_or(0);
        let width = (x + 2).max(title_w + 2).max(dir_w + BOX_PAD_LEFT + 1);
        let slot_x = |g: usize, key: &str| -> i32 {
            let slot = gap_users[g].iter().position(|(k, _)| k == key).unwrap_or(0) as i32;
            col_x[g] + col_w[g] + 1 + slot
        };

        // Intra legs.
        let mut legs = Vec::new();
        for (e, hops) in &intra_chains {
            let (a, b, status) = e;
            let mut points = vec![(col_x[lrank[a]] + by_id[a.as_str()].len + 1, rows[a])];
            for (c, key, y0, y1) in hops {
                let tx = slot_x(*c, key);
                points.push((tx, *y0));
                points.push((tx, *y1));
            }
            let land = (col_x[lrank[b]] - 2, rows[b]);
            points.push(land);
            legs.push((points, *status, a.clone(), b.clone(), land));
        }
        // Exits, one channel row per bundle.
        let mut exits = HashMap::new();
        let mut exit_bundles: HashMap<String, Exit> = HashMap::new();
        let mut out_row = out_top;
        for e in &outgoing {
            let (a, b, _) = e;
            let la = lrank[a];
            let start = (col_x[la] + by_id[a.as_str()].len + 1, rows[a]);
            let exit = if la == ncols - 1 {
                (vec![start, (width - 1, rows[a])], rows[a])
            } else {
                let key = exit_key(a, b);
                exit_bundles
                    .entry(key.clone())
                    .or_insert_with(|| {
                        let tx = slot_x(la, &key);
                        let y = out_row;
                        out_row += 1;
                        (vec![start, (tx, rows[a]), (tx, y), (width - 1, y)], y)
                    })
                    .clone()
            };
            exits.insert((a.clone(), b.clone()), exit);
        }
        // Entries, one channel row per bundle.
        let mut entries = HashMap::new();
        let mut entry_bundles: HashMap<String, Entry> = HashMap::new();
        let mut in_row = 1;
        for e in &incoming {
            let (a, b, _) = e;
            let lb = lrank[b];
            let entry = if lb == 0 {
                let land = (col_x[0] - 2, rows[b]);
                (vec![(0, rows[b]), land], land, rows[b])
            } else {
                let key = entry_key(a, b);
                entry_bundles
                    .entry(key.clone())
                    .or_insert_with(|| {
                        let tx = slot_x(lb - 1, &key);
                        let y = in_row;
                        in_row += 1;
                        let land = (col_x[lb] - 2, rows[b]);
                        (vec![(0, y), (tx, y), (tx, rows[b]), land], land, y)
                    })
                    .clone()
            };
            entries.insert((a.clone(), b.clone()), entry);
        }

        LocalBox {
            name: name.to_string(),
            pkg_col,
            members,
            lrank,
            col_x,
            col_w,
            rows,
            headers,
            dir_headers,
            legs,
            exits,
            entries,
            width,
            height,
        }
    }

    #[allow(clippy::needless_range_loop)]
    fn geometry(&self) -> Geometry {
        // 1. What's on screen at the current detail levels.
        let (view, edges, view_of, members_of) = self.view();

        // 2. Packages, ranked by who calls whom.
        let pkg_of: HashMap<String, String> =
            view.iter().map(|n| (n.id.clone(), n.pkg.clone())).collect();
        let mut pkg_names: Vec<String> = Vec::new();
        for n in &view {
            let p = &pkg_of[&n.id];
            if !pkg_names.contains(p) {
                pkg_names.push(p.clone());
            }
        }
        let pkg_index: HashMap<&str, usize> = pkg_names
            .iter()
            .enumerate()
            .map(|(i, p)| (p.as_str(), i))
            .collect();
        let pkg_edges: Vec<(String, String)> = edges
            .iter()
            .map(|(a, b, _)| (pkg_of[a].clone(), pkg_of[b].clone()))
            .filter(|(a, b)| a != b)
            .collect();
        let pkg_rank_map = longest_path_ranks(&pkg_names, &pkg_edges);
        let pkg_rank: Vec<usize> = pkg_names.iter().map(|p| pkg_rank_map[p]).collect();
        let npc = pkg_rank.iter().max().map_or(0, |m| m + 1);

        // 3. Each package's box on its own.
        let boxes: Vec<LocalBox> = pkg_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let members: Vec<&ViewNode> = view.iter().filter(|n| n.pkg == *name).collect();
                self.layout_box(name, pkg_rank[i], members, &edges, &pkg_of)
            })
            .collect();

        // 4. Package columns: widths, and the trunks each gap needs —
        //    one per inter-package edge crossing it.
        let inter: Vec<(String, String, Status, usize, usize)> = edges
            .iter()
            .filter_map(|(a, b, s)| {
                let (pa, pb) = (
                    pkg_rank[pkg_index[pkg_of[a].as_str()]],
                    pkg_rank[pkg_index[pkg_of[b].as_str()]],
                );
                (pb > pa).then(|| (a.clone(), b.clone(), *s, pa, pb))
            })
            .collect();
        let mut pcol_w = vec![0i32; npc];
        for b in &boxes {
            pcol_w[b.pkg_col] = pcol_w[b.pkg_col].max(b.width);
        }
        let gap_edges: Vec<Vec<usize>> = (0..npc)
            .map(|g| {
                inter
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, _, _, pa, pb))| *pa <= g && g < *pb)
                    .map(|(i, _)| i)
                    .collect()
            })
            .collect();
        let mut px = vec![0i32; npc];
        let mut x = 0;
        for r in 0..npc {
            px[r] = x;
            x += pcol_w[r] + GAP_FIXED + gap_edges[r].len() as i32;
        }
        let world_w = (x + 1).max(1);

        // 5. Rows: boxes stacked in their package column near the boxes
        //    that call them; pass-through rows between boxes for edges
        //    that skip the column.
        let mut box_y: Vec<i32> = vec![0; boxes.len()];
        let mut placed: Vec<bool> = vec![false; boxes.len()];
        let mut pass_rows: HashMap<(usize, usize), i32> = HashMap::new();
        for r in 0..npc {
            let mut items: Vec<(f64, usize, usize, bool)> = Vec::new(); // (bary, tiebreak, index, is_box)
            for (i, b) in boxes.iter().enumerate() {
                if b.pkg_col != r {
                    continue;
                }
                let mut src_rows: Vec<f64> = Vec::new();
                for (k, (a, bb, _, pa, _)) in inter.iter().enumerate() {
                    if !b.members.contains(bb) {
                        continue;
                    }
                    let y = if *pa == r - 1 && r > 0 {
                        let sb = boxes.iter().position(|x| x.members.contains(a)).unwrap();
                        if placed[sb] {
                            Some(box_y[sb] as f64 + boxes[sb].height as f64 / 2.0)
                        } else {
                            None
                        }
                    } else if r > 0 {
                        pass_rows.get(&(k, r - 1)).map(|y| *y as f64)
                    } else {
                        None
                    };
                    if let Some(y) = y {
                        src_rows.push(y);
                    }
                }
                let bary = if src_rows.is_empty() {
                    f64::MAX
                } else {
                    src_rows.iter().sum::<f64>() / src_rows.len() as f64
                };
                items.push((bary, i, i, true));
            }
            for (k, (_, _, _, pa, pb)) in inter.iter().enumerate() {
                if *pa < r && r < *pb {
                    let bary = pass_rows
                        .get(&(k, r - 1))
                        .map(|y| *y as f64)
                        .unwrap_or(f64::MAX);
                    items.push((bary, boxes.len() + k, k, false));
                }
            }
            items.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            let mut next_free = HEADER_ROW;
            for (bary, _, idx, is_box) in items {
                if is_box {
                    let ideal = if bary == f64::MAX {
                        next_free
                    } else {
                        (bary.round() as i32 - boxes[idx].height / 2).max(next_free)
                    };
                    box_y[idx] = ideal;
                    placed[idx] = true;
                    next_free = ideal + boxes[idx].height + 1;
                } else {
                    let y = if bary == f64::MAX {
                        next_free
                    } else {
                        (bary.round() as i32).max(next_free)
                    };
                    pass_rows.insert((idx, r), y);
                    next_free = y + 1;
                }
            }
        }
        // Pass-through rows share a column's trunks with the edge's own
        // slot, so they must also not sit on a placed box's rows: they
        // were allocated between boxes above, which guarantees that.

        // 6. Absolute geometry.
        let mut rank = HashMap::new();
        let mut rows = HashMap::new();
        let mut rects = HashMap::new();
        let mut headers = Vec::new();
        let mut dir_headers = Vec::new();
        let mut pkg_boxes = Vec::new();
        let mut legs: Vec<Leg> = Vec::new();
        let box_x = |b: &LocalBox| px[b.pkg_col];
        for (i, b) in boxes.iter().enumerate() {
            let (bx, by) = (box_x(b), box_y[i]);
            pkg_boxes.push(PkgBox {
                x0: bx,
                y0: by,
                x1: bx + b.width - 1,
                y1: by + b.height - 1,
                title: b.name.clone(),
            });
            for m in &b.members {
                let c = b.lrank[m];
                rank.insert(m.clone(), i * 64 + c);
                let y = by + b.rows[m];
                rows.insert(m.clone(), y);
                let h = 1 + i32::from(view.iter().any(|v| v.id == *m && v.sub.is_some()));
                rects.insert(m.clone(), (bx + b.col_x[c], y, b.col_w[c], h));
            }
            for (c, y, label) in &b.headers {
                headers.push((bx + b.col_x[*c], by + y, label.clone(), b.col_w[*c]));
            }
            for (c, y, dir) in &b.dir_headers {
                // The first column's header may run long; a later
                // column's is cut from the left to its column's width.
                // The marker sits in the margin two cells left of the
                // text, so a header lines up with the nodes under it
                // instead of looking indented past them.
                let label = if *c == 0 {
                    format!("▸ {dir}/")
                } else {
                    let room = (b.col_w[*c] as usize).saturating_sub(1);
                    let full = format!("{dir}/");
                    let n = full.chars().count();
                    if n > room {
                        format!("▸ …{}", full.chars().skip(n - room).collect::<String>())
                    } else {
                        format!("▸ {full}")
                    }
                };
                dir_headers.push((bx + b.col_x[*c] - 2, by + y, label));
            }
            for (points, status, from, to, land) in &b.legs {
                legs.push(Leg {
                    points: points.iter().map(|(x, y)| (bx + x, by + y)).collect(),
                    status: *status,
                    from: from.clone(),
                    to: to.clone(),
                    arrow: Some((bx + land.0, by + land.1)),
                });
            }
        }
        // Inter-package legs: exit polyline, trunks and pass-through
        // rows across the columns in between, entry polyline.
        for (k, (a, b, status, pa, pb)) in inter.iter().enumerate() {
            let sb = boxes.iter().position(|x| x.members.contains(a)).unwrap();
            let tb = boxes.iter().position(|x| x.members.contains(b)).unwrap();
            let (sbox, tbox) = (&boxes[sb], &boxes[tb]);
            let (sx, sy) = (box_x(sbox), box_y[sb]);
            let (tx0, ty0) = (box_x(tbox), box_y[tb]);
            let Some((exit_pts, exit_row)) = sbox.exits.get(&(a.clone(), b.clone())) else {
                continue;
            };
            let Some((entry_pts, land, entry_row)) = tbox.entries.get(&(a.clone(), b.clone()))
            else {
                continue;
            };
            let mut points: Vec<(i32, i32)> =
                exit_pts.iter().map(|(x, y)| (sx + x, sy + y)).collect();
            let mut y = sy + exit_row;
            for g in *pa..*pb {
                let slot = gap_edges[g].iter().position(|&e| e == k).unwrap_or(0) as i32;
                let trunk = px[g] + pcol_w[g] + 1 + slot;
                let next_y = if g + 1 == *pb {
                    ty0 + entry_row
                } else {
                    pass_rows[&(k, g + 1)]
                };
                points.push((trunk, y));
                points.push((trunk, next_y));
                y = next_y;
            }
            points.extend(entry_pts.iter().map(|(x, yy)| (tx0 + x, ty0 + yy)));
            legs.push(Leg {
                points,
                status: *status,
                from: a.clone(),
                to: b.clone(),
                arrow: Some((tx0 + land.0, ty0 + land.1)),
            });
        }
        let world_h = pkg_boxes.iter().map(|b| b.y1).max().unwrap_or(HEADER_ROW) + 2;

        Geometry {
            edges: edges
                .iter()
                .map(|(a, b, _)| (a.clone(), b.clone()))
                .collect(),
            nodes: view,
            view_of,
            members_of,
            rank,
            rows,
            rects,
            headers,
            dir_headers,
            boxes: pkg_boxes,
            legs,
            world_w: world_w.max(40),
            world_h,
        }
    }
}

/// How many world cells the diagram area shows on each axis, after
/// giving up its last row/column to a scrollbar wherever the world
/// overflows that axis.
fn viewport(geo: &Geometry, area: Rect) -> (i32, i32) {
    let full_w = area.width as i32;
    let full_h = (area.height as i32 - HEADER_ROW).max(1);
    let needs_h = geo.world_w > full_w;
    let h = if needs_h { (full_h - 1).max(1) } else { full_h };
    let needs_v = geo.world_h - HEADER_ROW > h;
    let w = if needs_v { (full_w - 1).max(1) } else { full_w };
    (w, h)
}

/// The minimap's place and scale, when it is wanted: only while the
/// graph overflows the window on some axis, in the bottom-right corner,
/// inside the scrollbars.
fn minimap_layout(geo: &Geometry, area: Rect, wanted: bool) -> Option<Minimap> {
    if !wanted {
        return None;
    }
    let (view_w, view_h) = viewport(geo, area);
    if geo.world_w <= view_w && geo.world_h - HEADER_ROW <= view_h {
        return None;
    }
    let w = MINIMAP_W.min(area.width / 2);
    let h = MINIMAP_H.min(area.height / 2);
    if w < 6 || h < 4 {
        return None;
    }
    let outer = Rect {
        x: area.x + area.width - w - 1,
        y: area.y + area.height - h - 1,
        width: w,
        height: h,
    };
    let inner = Rect {
        x: outer.x + 1,
        y: outer.y + 1,
        width: outer.width - 2,
        height: outer.height - 2,
    };
    let scale_x = ((geo.world_w + inner.width as i32 - 1) / inner.width as i32).max(1);
    let scale_y = ((geo.world_h + inner.height as i32 - 1) / inner.height as i32).max(1);
    debug_log(&format!(
        "minimap: world {}x{} inner {}x{} scale {}x{}",
        geo.world_w, geo.world_h, inner.width, inner.height, scale_x, scale_y
    ));
    Some(Minimap {
        inner,
        scale_x,
        scale_y,
    })
}

/// Draws the minimap: every package box as a shaded block, the selected
/// node as a bright cell, and the window's own footprint as a lighter
/// ground — so "where am I in this" has an answer without panning.
fn draw_minimap(app: &App, geo: &Geometry, mm: Minimap, area: Rect, buf: &mut Buffer) {
    use ratatui::widgets::{Block, Clear};
    let outer = Rect {
        x: mm.inner.x - 1,
        y: mm.inner.y - 1,
        width: mm.inner.width + 2,
        height: mm.inner.height + 2,
    };
    Clear.render(outer, buf);
    Block::bordered()
        .title(" map · m ")
        .border_style(Style::default().fg(Color::DarkGray))
        .render(outer, buf);
    let inner = mm.inner;
    let to_mini = |wx: i32, wy: i32| -> Option<(u16, u16)> {
        let mx = wx / mm.scale_x;
        let my = wy / mm.scale_y;
        (mx >= 0 && my >= 0 && mx < inner.width as i32 && my < inner.height as i32)
            .then(|| (inner.x + mx as u16, inner.y + my as u16))
    };
    // The window's footprint first, as a ground the rest sits on.
    let (view_w, view_h) = viewport(geo, area);
    let (cam_x, cam_y) = app.camera;
    for wy in (cam_y + HEADER_ROW)..(cam_y + HEADER_ROW + view_h) {
        for wx in cam_x..(cam_x + view_w) {
            if let Some((x, y)) = to_mini(wx, wy) {
                buf[(x, y)].set_bg(Color::Indexed(238));
            }
        }
    }
    for b in &geo.boxes {
        for wy in b.y0..=b.y1 {
            for wx in b.x0..=b.x1 {
                if let Some((x, y)) = to_mini(wx, wy) {
                    buf[(x, y)].set_symbol("▒").set_fg(Color::Indexed(245));
                }
            }
        }
    }
    if let Some((x, y, w)) = geo.node_rect(&app.selected) {
        for wx in x..(x + w) {
            if let Some((mx, my)) = to_mini(wx, y) {
                buf[(mx, my)].set_symbol("█").set_fg(Color::Yellow);
            }
        }
    }
}

const UP: u8 = 1;
const DOWN: u8 = 2;
const LEFT: u8 = 4;
const RIGHT: u8 = 8;

/// Lines as bits first, glyphs second: box borders, pass-through rows
/// and edge legs all OR their direction bits into one grid, so a line
/// leaving a box becomes `├`, two lines crossing become `┼`, and
/// nothing overwrites anything.
struct LineGrid {
    w: i32,
    h: i32,
    bits: Vec<u8>,
    color: Vec<Option<Color>>,
}

impl LineGrid {
    fn new(w: i32, h: i32) -> Self {
        let n = (w.max(0) * h.max(0)) as usize;
        Self {
            w,
            h,
            bits: vec![0; n],
            color: vec![None; n],
        }
    }

    fn set(&mut self, x: i32, y: i32, bits: u8, color: Option<Color>) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let i = (y * self.w + x) as usize;
        self.bits[i] |= bits;
        if color.is_some() {
            self.color[i] = color;
        }
    }

    fn hline(&mut self, x0: i32, x1: i32, y: i32, color: Option<Color>) {
        let (a, b) = (x0.min(x1), x0.max(x1));
        for x in a..=b {
            let mut bits = 0;
            if x > a {
                bits |= LEFT;
            }
            if x < b {
                bits |= RIGHT;
            }
            self.set(x, y, bits, color);
        }
    }

    fn vline(&mut self, x: i32, y0: i32, y1: i32, color: Option<Color>) {
        let (a, b) = (y0.min(y1), y0.max(y1));
        for y in a..=b {
            let mut bits = 0;
            if y > a {
                bits |= UP;
            }
            if y < b {
                bits |= DOWN;
            }
            self.set(x, y, bits, color);
        }
    }

    fn paint(&self, buf: &mut Buffer) {
        for y in 0..self.h {
            for x in 0..self.w {
                let i = (y * self.w + x) as usize;
                let bits = self.bits[i];
                if bits == 0 {
                    continue;
                }
                let ch = box_char(
                    bits & UP != 0,
                    bits & DOWN != 0,
                    bits & LEFT != 0,
                    bits & RIGHT != 0,
                );
                let style = Style::default().fg(self.color[i].unwrap_or(Color::DarkGray));
                buf.set_string(x as u16, y as u16, ch.to_string(), style);
            }
        }
    }
}

/// The colour of the selected node's own edges, and of the rest of the
/// chains upstream and downstream of it. Not used by any status.
const HIGHLIGHT: Color = Color::LightMagenta;
const HIGHLIGHT_CHAIN: Color = Color::Magenta;

fn edge_color(status: Status) -> Option<Color> {
    match status {
        Status::Added => Some(Color::Green),
        Status::Removed => Some(Color::Red),
        Status::Changed | Status::Unchanged => None,
    }
}

struct GraphWidget<'a> {
    app: &'a App,
    geo: &'a Geometry,
}

impl App {
    /// Which packages a diff touches and which it reaches: the packages
    /// with changed functions, the packages whose code calls into those
    /// (followed upstream as far as the callers search can see), and the
    /// calls the changed code makes into other packages. What `--impact`
    /// prints, so a script can tell "frontend only" from "backend too".
    /// Follows callers upstream from the changed code, `hops` further
    /// than the one hop the graph starts with — only along the upstream
    /// chain, never from the callees (whose callers would be the whole
    /// monorepo). What `--impact` and `--html` do before reporting.
    pub fn walk_upstream(&mut self, hops: usize) {
        self.walk_upstream_inner(hops);
        self.refresh_seen();
    }

    /// Returns whether the walk was cut short by the node cap (the hop
    /// limit shows up as callers never looked for, see `unexplored`).
    fn walk_upstream_inner(&mut self, hops: usize) -> bool {
        for _ in 0..hops {
            if self.nodes.len() > WALK_NODE_CAP {
                debug_log(&format!(
                    "walk_upstream: stopping at {} nodes",
                    self.nodes.len()
                ));
                return true;
            }
            let upstream = self.upstream_closure();
            let pending: Vec<String> = upstream
                .into_iter()
                .chain(self.origins.iter().cloned())
                .filter(|id| !is_ext_id(id) && !split_fn_id(id).1.starts_with("module@"))
                .filter(|id| !self.callers_sought.contains(id))
                .collect();
            if pending.is_empty() {
                break;
            }
            let before = self.nodes.len();
            for id in pending {
                self.expand(id, Dir::Callers);
            }
            if self.nodes.len() == before {
                break;
            }
        }
        false
    }

    pub fn impact(&mut self, hops: usize) -> Impact {
        self.walk_upstream(hops);
        self.package_impact()
    }

    fn package_impact(&self) -> Impact {
        let upstream = self.upstream_closure();
        let pkg_of = |id: &str| self.cluster_label(id);
        let mut changed: std::collections::BTreeMap<String, (HashSet<PathBuf>, usize)> =
            std::collections::BTreeMap::new();
        for id in &self.origins {
            let (file, _) = split_fn_id(id);
            let e = changed.entry(pkg_of(id)).or_default();
            e.0.insert(file);
            e.1 += 1;
        }
        let mut affected: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for id in &upstream {
            *affected.entry(pkg_of(id)).or_default() += 1;
        }
        // Calls across package borders that touch the changed code: into
        // it or its upstream chain, or out of it.
        let mut crossings: std::collections::BTreeMap<
            (String, String),
            (usize, usize, Vec<String>),
        > = std::collections::BTreeMap::new();
        for ((a, b), status) in &self.edges {
            let (pa, pb) = (pkg_of(a), pkg_of(b));
            if pa == pb {
                continue;
            }
            let into_change = self.origins.contains(b) || upstream.contains(b);
            let out_of_change = self.origins.contains(a);
            if !into_change && !out_of_change {
                continue;
            }
            let e = crossings.entry((pa, pb)).or_default();
            e.0 += 1;
            if *status != Status::Unchanged {
                e.1 += 1;
            }
            if let Some(display) = self
                .nodes
                .iter()
                .find(|n| n.id == *b)
                .map(|n| n.display.clone())
                && display.starts_with(|c: char| c.is_ascii_uppercase())
                && display.contains(" /")
                && !e.2.contains(&display)
            {
                e.2.push(display);
            }
        }
        for e in crossings.values_mut() {
            e.2.sort();
        }
        Impact {
            changed: changed
                .into_iter()
                .map(|(pkg, (files, fns))| (pkg, files.len(), fns))
                .collect(),
            affected: affected.into_iter().collect(),
            crossings: crossings
                .into_iter()
                .map(
                    |((from, to), (calls, changed_calls, routes))| ImpactCrossing {
                        from,
                        to,
                        calls,
                        changed_calls,
                        routes,
                    },
                )
                .collect(),
        }
    }

    /// The impact as a document: the changed functions, and for every
    /// entry point the upstream walk found, the shortest call path from
    /// it to the nearest changed function, each step placed by file,
    /// line range and calling line. See [`crate::impact`].
    pub fn impact_report(&mut self, hops: usize) -> crate::impact::ImpactReport {
        use crate::impact::*;
        let capped = self.walk_upstream_inner(hops);
        let summary = self.package_impact();

        // Who calls whom, over the calls HEAD still makes: a removed
        // call no longer reaches the change.
        let mut callers: HashMap<&str, Vec<&str>> = HashMap::new();
        for ((a, b), status) in &self.edges {
            if *status != Status::Removed && !is_ext_id(a) {
                callers.entry(b.as_str()).or_default().push(a.as_str());
            }
        }
        for v in callers.values_mut() {
            v.sort_unstable();
            v.dedup();
        }
        let mut origins: Vec<String> = self.origins.clone();
        origins.sort();
        origins.dedup();

        // Upward from every changed function at once: each node's
        // `toward` is its next step down to the nearest change.
        let mut toward: HashMap<String, Option<String>> = HashMap::new();
        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        for o in &origins {
            toward.insert(o.clone(), None);
            queue.push_back(o.clone());
        }
        while let Some(cur) = queue.pop_front() {
            for &a in callers.get(cur.as_str()).map(Vec::as_slice).unwrap_or(&[]) {
                if !toward.contains_key(a) {
                    toward.insert(a.to_string(), Some(cur.clone()));
                    queue.push_back(a.to_string());
                }
            }
        }
        // And from each change on its own, for what each entry reaches.
        let mut dist_from: HashMap<&str, HashMap<String, usize>> = HashMap::new();
        for o in &origins {
            let mut dist: HashMap<String, usize> = HashMap::from([(o.clone(), 0)]);
            let mut q = std::collections::VecDeque::from([o.clone()]);
            while let Some(cur) = q.pop_front() {
                let d = dist[&cur];
                for &a in callers.get(cur.as_str()).map(Vec::as_slice).unwrap_or(&[]) {
                    if !dist.contains_key(a) {
                        dist.insert(a.to_string(), d + 1);
                        q.push_back(a.to_string());
                    }
                }
            }
            dist_from.insert(o.as_str(), dist);
        }

        let explored = |id: &str| self.callers_sought.contains(id);
        let mut reached: Vec<&String> = toward.keys().collect();
        reached.sort();
        let mut entries: Vec<(String, EntryKind)> = Vec::new();
        for id in reached {
            let (_, _, route) = self.node_place(id);
            let top = callers.get(id.as_str()).is_none_or(|v| v.is_empty());
            if !top && route.is_none() {
                continue;
            }
            let (file, label) = split_fn_id(id);
            let kind = if route.is_some() {
                EntryKind::Route
            } else if label.starts_with("module@") {
                EntryKind::Module
            } else if file.extension().is_some_and(|e| e == "svelte") {
                EntryKind::Component
            } else if !explored(id) {
                EntryKind::Unexplored
            } else if self
                .function_files
                .get(&file)
                .and_then(|e| find_function(e, label))
                .is_some_and(|(_, f)| f.exported && f.parent.is_none())
            {
                EntryKind::Export
            } else {
                EntryKind::Function
            };
            entries.push((id.clone(), kind));
        }

        let (sites, seam_defs, seam_calls) = {
            let scan = self.hono_scan(true);
            (scan.sites, scan.seam_defs, scan.seam_calls)
        };
        let mut chains = Vec::new();
        for (id, kind) in &entries {
            let mut path = vec![id.clone()];
            while let Some(Some(next)) = toward.get(path.last().unwrap()) {
                path.push(next.clone());
            }
            let changed = path.pop().unwrap_or_default();
            let mut hops_out = Vec::new();
            for (i, step) in path.iter().enumerate() {
                let next = path.get(i + 1).unwrap_or(&changed);
                hops_out.push(Hop {
                    function: self.report_function(step),
                    call_site: self.call_site(step, next, &sites, &seam_defs, &seam_calls),
                });
            }
            let mut reaches: Vec<(usize, &String)> = origins
                .iter()
                .filter_map(|o| dist_from[o.as_str()].get(id).map(|d| (*d, o)))
                .collect();
            reaches.sort();
            let f = self.report_function(id);
            chains.push(Chain {
                entry: Entry {
                    kind: *kind,
                    label: f.route.clone().unwrap_or(f.name.clone()),
                    id: id.clone(),
                },
                changed,
                reaches: reaches.into_iter().map(|(_, o)| o.clone()).collect(),
                hops: hops_out,
            });
        }
        let truncated = capped || entries.iter().any(|(_, k)| *k == EntryKind::Unexplored);
        let mut changed: Vec<Function> = origins.iter().map(|o| self.report_function(o)).collect();
        changed.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));

        ImpactReport {
            version: IMPACT_VERSION,
            base: self.base_rev.label(),
            head: self.head_rev.commit_sha().map(str::to_string),
            changed,
            chains,
            packages: Packages {
                changed: summary
                    .changed
                    .iter()
                    .map(|(p, f, n)| PackageChange {
                        package: p.clone(),
                        files: *f,
                        functions: *n,
                    })
                    .collect(),
                affected: summary
                    .affected
                    .iter()
                    .map(|(p, n)| PackageAffected {
                        package: p.clone(),
                        functions: *n,
                    })
                    .collect(),
                crossings: summary
                    .crossings
                    .iter()
                    .map(|c| Crossing {
                        from: c.from.clone(),
                        to: c.to.clone(),
                        calls: c.calls,
                        changed_calls: c.changed_calls,
                        routes: c.routes.clone(),
                    })
                    .collect(),
            },
            truncated,
            limits: Limits {
                hops,
                nodes: WALK_NODE_CAP,
            },
        }
    }

    /// The plan: the changed functions, how far each reaches, and the
    /// reach as a whole — facts only; `prognost assess` judges them. See
    /// [`crate::plan`].
    pub fn plan_report(&mut self, hops: usize) -> anyhow::Result<crate::plan::PlanReport> {
        use crate::impact::Change;
        use crate::plan::*;
        let impact = self.impact_report(hops);
        let (sites, seam_defs, seam_calls) = {
            let scan = self.hono_scan(true);
            (scan.sites, scan.seam_defs, scan.seam_calls)
        };

        // Reach is over the calls HEAD still makes.
        let mut callers: HashMap<&str, Vec<&str>> = HashMap::new();
        for ((a, b), status) in &self.edges {
            if *status != Status::Removed && !is_ext_id(a) {
                callers.entry(b.as_str()).or_default().push(a.as_str());
            }
        }
        let origins: HashSet<&str> = self.origins.iter().map(String::as_str).collect();
        let package_of = |id: &str| -> Option<String> {
            self.node_file(id).and_then(|f| {
                self.head_ws
                    .owning_package(&f)
                    .or_else(|| self.base_ws.owning_package(&f))
                    .and_then(|p| p.name.clone())
            })
        };
        let package_key =
            |id: &str| package_of(id).unwrap_or_else(|| "(outside any package)".to_string());
        let upstream_of = |o: &str| -> HashSet<String> {
            let mut seen: HashSet<String> = HashSet::new();
            let mut stack = vec![o.to_string()];
            while let Some(cur) = stack.pop() {
                for &a in callers.get(cur.as_str()).map(Vec::as_slice).unwrap_or(&[]) {
                    if a != o && seen.insert(a.to_string()) {
                        stack.push(a.to_string());
                    }
                }
            }
            seen
        };
        let entry_of: HashMap<&str, crate::impact::EntryKind> = impact
            .chains
            .iter()
            .map(|c| (c.entry.id.as_str(), c.entry.kind))
            .collect();
        let exported = |id: &str| -> bool {
            let (file, label) = split_fn_id(id);
            self.function_files
                .get(&file)
                .and_then(|e| find_function(e, label))
                .is_some_and(|(_, func)| func.exported && func.parent.is_none())
        };

        // Summary per changed function, and the union upstream.
        let mut symbols: Vec<SymbolSummary> = Vec::new();
        let mut all_up: HashSet<String> = HashSet::new();
        for f in &impact.changed {
            let up = upstream_of(&f.id);
            let files: HashSet<PathBuf> = up.iter().filter_map(|u| self.node_file(u)).collect();
            let pkgs: HashSet<String> = up.iter().map(|u| package_key(u)).collect();
            let own = package_key(&f.id);
            let mut called_from: Vec<String> = callers
                .get(f.id.as_str())
                .map(|v| {
                    v.iter()
                        .map(|a| package_key(a))
                        .filter(|p| *p != own)
                        .collect()
                })
                .unwrap_or_default();
            called_from.sort();
            called_from.dedup();
            symbols.push(SymbolSummary {
                id: f.id.clone(),
                public: f.route.is_some() || (exported(&f.id) && !called_from.is_empty()),
                called_from_packages: called_from,
                reach_functions: up.iter().filter(|u| !origins.contains(u.as_str())).count(),
                reach_files: files.len(),
                reach_packages: pkgs.len(),
                reach_entries: up
                    .iter()
                    .filter(|u| entry_of.contains_key(u.as_str()))
                    .count()
                    + usize::from(entry_of.contains_key(f.id.as_str())),
            });
            all_up.extend(up);
        }
        all_up.retain(|u| !origins.contains(u.as_str()));
        symbols.sort_by(|a, b| {
            b.reach_functions
                .cmp(&a.reach_functions)
                .then_with(|| a.id.cmp(&b.id))
        });

        // Facts: the functions — changed, then upstream — in one shape.
        let mut ids: Vec<String> = impact.changed.iter().map(|f| f.id.clone()).collect();
        let mut up_ids: Vec<String> = all_up.iter().cloned().collect();
        up_ids.sort();
        ids.extend(up_ids);
        let functions: Vec<Function> = ids
            .iter()
            .map(|id| {
                let f = self.report_function(id);
                Function {
                    id: f.id,
                    name: f.name,
                    path: f.path,
                    range: LineRange {
                        start: f.line,
                        end: f.to,
                    },
                    side: f.side,
                    change: if origins.contains(id.as_str()) {
                        f.change
                    } else {
                        Change::Unchanged
                    },
                    package: f.package,
                    exported: exported(id),
                    route: f.route,
                    entry: entry_of.get(id.as_str()).copied(),
                }
            })
            .collect();

        // Facts: the calls between them.
        let in_plan: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut calls: Vec<Call> = self
            .edges
            .iter()
            .filter(|((a, b), _)| in_plan.contains(a.as_str()) && in_plan.contains(b.as_str()))
            .map(|((a, b), status)| Call {
                caller: a.clone(),
                callee: b.clone(),
                line: if *status == Status::Removed {
                    None
                } else {
                    self.call_site(a, b, &sites, &seam_defs, &seam_calls)
                },
                change: match status {
                    Status::Added => CallChange::Added,
                    Status::Removed => CallChange::Removed,
                    _ => CallChange::Unchanged,
                },
                inferred: self.inferred_edges.contains(&(a.clone(), b.clone())),
            })
            .collect();
        calls.sort_by(|x, y| (&x.caller, &x.callee).cmp(&(&y.caller, &y.callee)));

        // Facts: the files.
        let files = crate::plan::file_changes(
            &self.root,
            &self.base_rev,
            &self.head_rev,
            &self.base_ws,
            &self.head_ws,
        )?;

        // Summary: the reach together.
        let mut per_pkg: BTreeMap<String, (usize, HashSet<PathBuf>)> = BTreeMap::new();
        for u in &all_up {
            let e = per_pkg.entry(package_key(u)).or_default();
            e.0 += 1;
            if let Some(f) = self.node_file(u) {
                e.1.insert(f);
            }
        }
        let mut packages: Vec<PackageReach> = per_pkg
            .into_iter()
            .map(|(package, (functions, files))| PackageReach {
                package,
                functions,
                files: files.len(),
            })
            .collect();
        packages.sort_by(|a, b| {
            b.functions
                .cmp(&a.functions)
                .then_with(|| a.package.cmp(&b.package))
        });
        let reach_files: HashSet<PathBuf> =
            all_up.iter().filter_map(|u| self.node_file(u)).collect();
        let mut entries: BTreeMap<String, usize> = BTreeMap::new();
        for f in &functions {
            if let Some(k) = f.entry {
                let kind = serde_json::to_value(k)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                *entries.entry(kind).or_default() += 1;
            }
        }
        let count = |c: Change| {
            functions
                .iter()
                .filter(|f| f.change == c && origins.contains(f.id.as_str()))
                .count()
        };
        let with_functions: HashSet<&str> = functions
            .iter()
            .filter(|f| origins.contains(f.id.as_str()))
            .map(|f| f.path.as_str())
            .collect();

        Ok(PlanReport {
            version: PLAN_VERSION,
            base: impact.base.clone(),
            head: impact.head.clone(),
            summary: Summary {
                changed: ChangeCounts {
                    added: count(Change::Added),
                    changed: count(Change::Changed),
                    removed: count(Change::Removed),
                    files_without_function_changes: files
                        .iter()
                        .filter(|f| !with_functions.contains(f.path.as_str()))
                        .count(),
                },
                reach: Reach {
                    functions: all_up.len(),
                    files: reach_files.len(),
                    packages,
                    entries,
                },
                symbols,
            },
            files,
            functions,
            calls,
            truncated: impact.truncated,
            limits: impact.limits.clone(),
        })
    }

    /// One node as the impact report names and places it.
    fn report_function(&self, id: &str) -> crate::impact::Function {
        use crate::impact::{Change, Function, Side};
        let (line, to, route) = self.node_place(id);
        let node = self.nodes.iter().find(|n| n.id == id);
        let display = node
            .map(|n| n.display.clone())
            .unwrap_or_else(|| split_fn_id(id).1.to_string());
        let name = match display.rsplit_once(" · L") {
            Some((head, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => {
                head.to_string()
            }
            _ => display,
        };
        let (file, label) = split_fn_id(id);
        let in_head = self
            .function_files
            .get(&file)
            .is_none_or(|e| e.head_fns.iter().any(|f| align::label(f) == label))
            || label.starts_with("module@")
            || self.call_file.contains_key(id);
        let path = self.node_file(id);
        Function {
            id: id.to_string(),
            name,
            path: path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            line,
            to,
            side: if in_head { Side::Head } else { Side::Base },
            change: match node.map(|n| n.status) {
                Some(Status::Added) => Change::Added,
                Some(Status::Removed) => Change::Removed,
                Some(Status::Changed) => Change::Changed,
                _ => Change::Unchanged,
            },
            package: path.as_ref().and_then(|p| {
                self.head_ws
                    .owning_package(p)
                    .or_else(|| self.base_ws.owning_package(p))
                    .and_then(|pkg| pkg.name.clone())
            }),
            route,
        }
    }

    /// The line in `caller` (HEAD) that calls `callee`: an RPC call site
    /// for a route handler, a seam call site for a seam definition, else
    /// the first recorded call (callbacks folded in) naming the callee,
    /// else the first line mentioning it — a function handed over as a
    /// value (`steps: [syncTables]`) is still reached from there.
    fn call_site(
        &self,
        caller: &str,
        callee: &str,
        sites: &[RpcSite],
        seam_defs: &[(usize, crate::seam::Def)],
        seam_calls: &[(usize, crate::seam::CallSite)],
    ) -> Option<u32> {
        let (file, label) = split_fn_id(caller);
        if let Some(l) = label.strip_prefix("module@").and_then(|l| l.parse().ok()) {
            return Some(l);
        }
        let entry = self.function_files.get(&file)?;
        let idx = entry
            .head_fns
            .iter()
            .position(|f| align::label(f) == label)?;
        let (start, end) = (entry.head_fns[idx].start_line, entry.head_fns[idx].end_line);
        let within = |l: u32| start <= l && l <= end;

        let (_, _, route) = self.node_place(callee);
        if let Some(route) = route
            && let Some((method, path)) = crate::hono::split_route_label(&route)
        {
            let segments = crate::hono::path_segments(path);
            if let Some(s) = sites.iter().find(|s| {
                s.file == file
                    && within(s.line)
                    && s.call.method == method
                    && crate::hono::same_route(&s.call.segments, &segments)
            }) {
                return Some(s.line);
            }
        }

        let (callee_file, callee_label) = split_fn_id(callee);
        let (cstart, cend) = {
            let (l, t, _) = self.node_place(callee);
            (l, t)
        };
        let keys: Vec<(usize, &str)> = seam_defs
            .iter()
            .filter(|(_, d)| d.file == callee_file && cstart <= d.line && d.line <= cend)
            .map(|(i, d)| (*i, d.key.as_str()))
            .collect();
        if let Some((_, c)) = seam_calls.iter().find(|(i, c)| {
            c.file == file
                && within(c.line)
                && keys.iter().any(|(j, k)| {
                    j == i
                        && self
                            .seams
                            .get(*i)
                            .is_some_and(|s| crate::seam::Seam::same_key(s, k, &c.key))
                })
        }) {
            return Some(c.line);
        }

        // A call an external resolver placed.
        if let Some(calls) = Lang::of(&file).and_then(|l| self.external.get(&(true, l)))
            && let Some(line) = calls
                .by_site
                .iter()
                .filter(|((f, l), targets)| {
                    *f == file
                        && within(*l)
                        && targets
                            .iter()
                            .any(|(tf, tl)| *tf == callee_file && cstart <= *tl && *tl <= cend)
                })
                .map(|((_, l), _)| *l)
                .min()
        {
            return Some(line);
        }

        let name: String = match split_pkg_id(callee) {
            Some((_, call)) => call.to_string(),
            None => callee_label.to_string(),
        };
        if name.starts_with("anon@") || name.starts_with("module@") || name.is_empty() {
            return None;
        }
        // `Repo(…)` calls `Repo.__init__`.
        let name = name
            .strip_suffix(".__init__")
            .map(str::to_string)
            .unwrap_or(name);
        let last = name.rsplit('.').next().unwrap_or(&name).to_string();
        if let Some((_, l)) = inlined_calls(&entry.head_fns, idx)
            .into_iter()
            .find(|(c, _)| c == &name || call_matches(c, &last))
        {
            return Some(l);
        }
        // A function declared inside this one is not called by being
        // declared: its own lines don't count. A Svelte component's
        // script is wrapped as one function, but its markup
        // (`onclick={save}`) is where it hands its functions over.
        let text = self.head_rev.read(&self.root, &file)?;
        let word = regex::Regex::new(&format!(r"\b{}\b", regex::escape(&last))).ok()?;
        let own = |n: u32| callee_file == file && cstart <= n && n <= cend;
        let whole_component =
            file.extension().is_some_and(|e| e == "svelte") && entry.head_fns[idx].parent.is_none();
        text.lines()
            .enumerate()
            .map(|(i, l)| (i as u32 + 1, l))
            .filter(|(n, _)| (whole_component || within(*n)) && *n != start && !own(*n))
            .find(|(_, l)| word.is_match(l))
            .map(|(n, _)| n)
    }

    /// Every node with a path into the changed code, over the real
    /// edges; the changed functions themselves excluded.
    fn upstream_closure(&self) -> HashSet<String> {
        let mut upstream: HashSet<String> = HashSet::new();
        let mut frontier: Vec<String> = self.origins.clone();
        while let Some(cur) = frontier.pop() {
            for (a, b) in self.edges.keys() {
                if *b == cur && !self.origins.contains(a) && upstream.insert(a.clone()) {
                    frontier.push(a.clone());
                }
            }
        }
        upstream
    }

    /// The graph as a self-contained web page (`--html`): the same
    /// structure — packages, directories, files, functions, calls and
    /// statuses — for the browser to lay out and light up on its own.
    pub fn dump_html(&self) -> String {
        self.render_html(false)
    }

    /// The same page, told whether it is served by `--serve` (so it can
    /// read and write seen marks and the PR's Viewed state through the
    /// API) or opened as a file.
    /// Where a node's body sits — (first line, last line, mounted route
    /// for a handler) — HEAD's lines, BASE's for a removed function; a
    /// module-level reference or a factory-built export is one line.
    fn node_place(&self, id: &str) -> (u32, u32, Option<String>) {
        if let Some((_, l)) = self.call_file.get(id) {
            return (*l, *l, None);
        }
        if let Some(l) = split_fn_id(id)
            .1
            .strip_prefix("module@")
            .and_then(|l| l.parse().ok())
        {
            return (l, l, None);
        }
        let (f, label) = split_fn_id(id);
        self.function_files
            .get(&f)
            .and_then(|entry| find_function(entry, label))
            .map(|(_, func)| {
                let route = func.route.as_ref().and_then(|r| {
                    let (m, p) = crate::hono::split_route_label(r)?;
                    let prefix = self.routes.as_ref().and_then(|t| t.mount_paths.get(&f));
                    Some(match prefix {
                        Some(pre) if !pre.is_empty() => format!("{m} /{}{p}", pre.join("/")),
                        _ => r.clone(),
                    })
                });
                (func.start_line, func.end_line, route)
            })
            .unwrap_or((0, 0, None))
    }

    pub fn render_html(&self, api: bool) -> String {
        let status_name = |s: Status| match s {
            Status::Added => "added",
            Status::Removed => "removed",
            Status::Changed => "changed",
            Status::Unchanged => "unchanged",
        };
        let mut nodes = Vec::new();
        for n in self.nodes.iter().filter(|n| self.is_visible(&n.id)) {
            let pkg = self.cluster_label(&n.id);
            let file = self.node_file(&n.id);
            let (file_rel, dir) = match &file {
                Some(f) => (self.path_in_package(f), self.dir_in_package(f)),
                None => (String::new(), String::new()),
            };
            let (line, end, route) = self.node_place(&n.id);
            let path = file
                .as_ref()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            // The line goes in its own field; the label loses its " · L12".
            let label = match n.display.rsplit_once(" · L") {
                Some((head, tail))
                    if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) =>
                {
                    head.to_string()
                }
                _ => n.display.clone(),
            };
            nodes.push(crate::html::ModelNode {
                id: n.id.clone(),
                label,
                status: status_name(n.status),
                pkg,
                dir,
                file: file_rel,
                line,
                end,
                path,
                origin: self.origins.contains(&n.id),
                external: !n.drillable,
                route,
            });
        }
        // Whole-file diffs for every file holding a changed node — an
        // upstream walk touches far more files than a reader will open,
        // and an unchanged file's "diff" is just the file.
        let mut paths: Vec<String> = nodes
            .iter()
            .filter(|n| n.status != "unchanged" || n.origin)
            .map(|n| n.path.clone())
            .filter(|p| !p.is_empty())
            .collect();
        paths.sort();
        paths.dedup();
        let files: Vec<(String, Vec<crate::html::DiffRow>)> = paths
            .into_iter()
            .map(|p| {
                let path = PathBuf::from(&p);
                let base_src = self.base_rev.read(&self.root, &path).unwrap_or_default();
                let head_src = self.head_rev.read(&self.root, &path).unwrap_or_default();
                let rows = diff_lines(&base_src, &head_src)
                    .into_iter()
                    .map(|l| {
                        let kind = match (l.changed, l.head_line) {
                            (false, _) => 0,
                            (true, Some(_)) => 1,
                            (true, None) => 2,
                        };
                        let text = l.text.get(8..).unwrap_or("").to_string();
                        (kind, l.head_line.unwrap_or(0), text)
                    })
                    .collect();
                (p, rows)
            })
            .collect();
        let ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        let mut edges: Vec<crate::html::ModelEdge> = self
            .edges
            .iter()
            .filter(|((a, b), _)| ids.contains(a.as_str()) && ids.contains(b.as_str()))
            .map(|((a, b), s)| crate::html::ModelEdge {
                from: a.clone(),
                to: b.clone(),
                status: status_name(*s),
                inferred: self.inferred_edges.contains(&(a.clone(), b.clone())),
            })
            .collect();
        edges.sort_by(|x, y| (&x.from, &x.to).cmp(&(&y.from, &y.to)));
        crate::html::Model {
            base: self.base_rev.label(),
            head: self.head_rev.label(),
            nodes,
            edges,
            files,
            api,
        }
        .render()
    }

    /// The whole canvas as plain text, one line per world row — what
    /// `--dump` writes. Styling is dropped; the geometry is exact.
    pub fn dump_world(&self) -> String {
        let geo = self.geometry();
        let world = draw_world(self, &geo);
        let mut out = String::new();
        for y in 0..world.area.height {
            let mut line = String::new();
            for x in 0..world.area.width {
                line.push_str(world[(x, y)].symbol());
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}

/// Everything drawn onto the world, with no clipping to think about;
/// the camera's window of it is copied to the screen afterwards.
fn draw_world(app: &App, geo: &Geometry) -> Buffer {
    let world_rect = Rect::new(
        0,
        0,
        geo.world_w.min(u16::MAX as i32) as u16,
        geo.world_h.min(u16::MAX as i32) as u16,
    );
    let mut world = Buffer::empty(world_rect);
    let mut lines = LineGrid::new(world_rect.width as i32, world_rect.height as i32);

    // Boxes and edges as line bits. The selected node's own edges —
    // every caller into it, every callee out of it — are drawn last, in
    // one colour nothing else uses, so they stay traceable through the
    // trunks; "who calls this" is answered by the picture even when ←
    // would have to choose between several.
    // The box the selection is in gets a brighter border, so the
    // package under the cursor is obvious at a glance — z/Z act on it.
    let selected_pkg = app.view_pkg(&app.selected);
    for b in &geo.boxes {
        let color = (b.title == selected_pkg).then_some(Color::Cyan);
        lines.hline(b.x0, b.x1, b.y0, color);
        lines.hline(b.x0, b.x1, b.y1, color);
        lines.vline(b.x0, b.y0, b.y1, color);
        lines.vline(b.x1, b.y0, b.y1, color);
    }
    // Everything upstream of the selection (its callers, their callers,
    // …) and everything downstream is lit too, a shade darker than the
    // direct edges: from a leaf in the database the whole path back to
    // the route handler and the frontend reads at once.
    let mut upstream: HashSet<&str> = HashSet::new();
    let mut frontier = vec![app.selected.as_str()];
    while let Some(cur) = frontier.pop() {
        for (a, b) in &geo.edges {
            if b == cur && upstream.insert(a.as_str()) {
                frontier.push(a.as_str());
            }
        }
    }
    let mut downstream: HashSet<&str> = HashSet::new();
    let mut frontier = vec![app.selected.as_str()];
    while let Some(cur) = frontier.pop() {
        for (a, b) in &geo.edges {
            if a == cur && downstream.insert(b.as_str()) {
                frontier.push(b.as_str());
            }
        }
    }
    let direct = |leg: &Leg| leg.from == app.selected || leg.to == app.selected;
    let chained = |leg: &Leg| {
        (upstream.contains(leg.to.as_str()) && upstream.contains(leg.from.as_str()))
            || (downstream.contains(leg.from.as_str()) && downstream.contains(leg.to.as_str()))
    };
    let lit = |leg: &Leg| direct(leg) || chained(leg);
    let mut ordered: Vec<&Leg> = geo.legs.iter().collect();
    ordered.sort_by_key(|l| (lit(l), direct(l)));
    for leg in &ordered {
        let color = if direct(leg) {
            Some(HIGHLIGHT)
        } else if chained(leg) {
            Some(HIGHLIGHT_CHAIN)
        } else {
            edge_color(leg.status)
        };
        for w in leg.points.windows(2) {
            let ((x0, y0), (x1, y1)) = (w[0], w[1]);
            if y0 == y1 {
                lines.hline(x0, x1, y0, color);
            } else {
                lines.vline(x0, y0, y1, color);
            }
        }
    }
    lines.paint(&mut world);
    for leg in &ordered {
        if let Some((x, y)) = leg.arrow {
            let color = if direct(leg) {
                HIGHLIGHT
            } else if chained(leg) {
                HIGHLIGHT_CHAIN
            } else {
                edge_color(leg.status).unwrap_or(Color::DarkGray)
            };
            // A hollow head: a call guessed through a Go interface.
            let head = if app
                .inferred_edges
                .contains(&(leg.from.clone(), leg.to.clone()))
            {
                "▷"
            } else {
                "▶"
            };
            world.set_string(x as u16, y as u16, head, Style::default().fg(color));
        }
    }

    // Caption.
    world.set_string(
        1,
        0,
        "packages left → right by who calls whom · inside a package, left → right by call depth",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );

    // Box titles over the top border.
    for b in &geo.boxes {
        world.set_string(
            (b.x0 + 1) as u16,
            b.y0 as u16,
            truncate(&format!(" {} ", b.title), (b.x1 - b.x0 - 1).max(0) as usize),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    }

    // Directory headers across the box.
    for (x, row, label) in &geo.dir_headers {
        world.set_string(
            *x as u16,
            *row as u16,
            label,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        );
    }

    // File headers, before the nodes so a node's own styling wins where
    // they'd overlap.
    for (x, row, label, w) in &geo.headers {
        world.set_string(
            (*x - 2) as u16,
            *row as u16,
            truncate(&format!("▾ {label}"), *w as usize + 2),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::ITALIC),
        );
    }

    // Nodes.
    for n in &geo.nodes {
        let Some((x, y, w)) = geo.node_rect(&n.id) else {
            continue;
        };
        let mut style = status_style(n.status, n.id == app.selected);
        if n.origin {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        if n.dim {
            style = style.add_modifier(Modifier::DIM);
        }
        if is_agg_id(&n.id) {
            style = style.add_modifier(Modifier::BOLD);
        }
        let text = truncate(&n.display, w as usize);
        world.set_string(x as u16, y as u16, &text, style);
        if let Some(sub) = &n.sub {
            world.set_string(
                x as u16,
                (y + 1) as u16,
                truncate(sub, w as usize),
                Style::default().fg(Color::DarkGray),
            );
        }
    }

    world
}

impl Widget for GraphWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let app = self.app;
        let geo = self.geo;
        if area.width == 0 || area.height == 0 {
            return;
        }
        let world = draw_world(app, geo);
        let world_rect = world.area;
        // Blit the camera's window: the header row stays put, the rest
        // scrolls by the camera.
        let (view_w, view_h) = viewport(geo, area);
        let (cam_x, cam_y) = app.camera;
        for sy in 0..(HEADER_ROW + view_h).min(area.height as i32) {
            let wy = if sy < HEADER_ROW { sy } else { sy + cam_y };
            if wy < 0 || wy >= world_rect.height as i32 {
                continue;
            }
            for sx in 0..view_w.min(area.width as i32) {
                let wx = sx + cam_x;
                if wx < 0 || wx >= world_rect.width as i32 {
                    continue;
                }
                buf[(area.x + sx as u16, area.y + sy as u16)] =
                    world[(wx as u16, wy as u16)].clone();
            }
        }

        // Scrollbars only where there's more world than window, so the
        // reader can tell how much of the graph is off-screen.
        if view_w < area.width as i32 {
            let bar = Rect {
                y: area.y + HEADER_ROW as u16,
                height: area.height - HEADER_ROW as u16,
                ..area
            };
            let mut state = ScrollbarState::new((geo.world_h - HEADER_ROW).max(0) as usize)
                .viewport_content_length(view_h as usize)
                .position(cam_y as usize);
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .symbols(ratatui::symbols::scrollbar::VERTICAL)
                .begin_symbol(None)
                .end_symbol(None)
                .style(Style::default().fg(Color::DarkGray))
                .render(bar, buf, &mut state);
        }
        if view_h < area.height as i32 - HEADER_ROW {
            let mut state = ScrollbarState::new(geo.world_w.max(0) as usize)
                .viewport_content_length(view_w as usize)
                .position(cam_x as usize);
            Scrollbar::new(ScrollbarOrientation::HorizontalBottom)
                .symbols(ratatui::symbols::scrollbar::HORIZONTAL)
                .begin_symbol(None)
                .end_symbol(None)
                .style(Style::default().fg(Color::DarkGray))
                .render(area, buf, &mut state);
        }
        if let Some(mm) = app.minimap {
            draw_minimap(app, geo, mm, area, buf);
        }
    }
}

fn status_style(status: Status, selected: bool) -> Style {
    let mut style = Style::default().fg(match status {
        Status::Unchanged => Color::Gray,
        Status::Added => Color::Green,
        Status::Removed => Color::Red,
        Status::Changed => Color::Yellow,
    });
    if status == Status::Removed {
        style = style.add_modifier(Modifier::DIM);
    }
    if selected {
        style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
    }
    style
}

fn box_char(up: bool, down: bool, left: bool, right: bool) -> char {
    match (up, down, left, right) {
        (false, false, _, _) => '─',
        (false, true, false, true) => '┌',
        (false, true, true, false) => '┐',
        (false, true, true, true) => '┬',
        (true, false, false, true) => '└',
        (true, false, true, false) => '┘',
        (true, false, true, true) => '┴',
        (true, true, false, true) => '├',
        (true, true, true, false) => '┤',
        (true, true, true, true) => '┼',
        (true, true, false, false) | (true, false, false, false) | (false, true, false, false) => {
            '│'
        }
    }
}

/// A full-screen colored unified diff, replacing the graph entirely
/// while open — its own small header/footer, scrollable independently
/// of the graph's own scroll state.
fn draw_diff(f: &mut Frame, area: Rect, view: &DiffView) {
    let header = Rect { height: 1, ..area };
    let keys = Rect {
        height: 1,
        y: area.y + area.height.saturating_sub(1),
        ..area
    };
    let body = Rect {
        y: header.y + 1,
        height: area.height.saturating_sub(2),
        ..area
    };
    f.render_widget(
        Paragraph::new(format!(
            "file — {}{}   (+ added, - removed)",
            view.title, view.seen_note
        ))
        .style(Style::default().add_modifier(Modifier::BOLD)),
        header,
    );
    let text = ratatui::text::Text::from_iter(
        view.lines
            .iter()
            .map(|l| ratatui::text::Line::styled(l.text.as_str(), l.style)),
    );
    f.render_widget(Paragraph::new(text).scroll((view.scroll, 0)), body);
    f.render_widget(
        Paragraph::new("j/k or ↑/↓ scroll · PageUp/PageDown · v mark this file seen · enter/q/esc back to graph")
            .style(Style::default().fg(Color::DarkGray)),
        keys,
    );
}

/// Draws a horizontal run, clipped to `area` — `x`/`len` come from
/// column math that can legitimately land partway or fully off either
/// edge once horizontal scrolling is in play, so the clipping happens
/// here rather than requiring every call site to work it out.
fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else if width <= 1 {
        s.chars().take(width).collect()
    } else {
        format!("{}…", s.chars().take(width - 1).collect::<String>())
    }
}

/// Extracts and aligns one file's functions in both revisions. `None`
/// only when HEAD itself can't be read — a file with zero functions is
/// still `Some`, just with empty lists, since the caller may only need
/// this to check "does anything here call X", not to pick a focus.
type ParsedFunctions = (Vec<TsFunction>, Vec<TsFunction>, Vec<Alignment>);

fn parse_functions(
    root: &std::path::Path,
    base_rev: &Rev,
    head_rev: &Rev,
    path: &std::path::Path,
) -> Option<ParsedFunctions> {
    let base_src = base_rev.read(root, path).unwrap_or_default();
    let head_src = head_rev.read(root, path)?;
    let base_fns = crate::lang::extract_for_path(path, &base_src).ok()?;
    let head_fns = crate::lang::extract_for_path(path, &head_src).ok()?;
    let alignment = align::align(&base_fns, &head_fns);
    Some((base_fns, head_fns, alignment))
}

/// [`parse_functions`] plus a starting focus within the file: the first
/// exported function in HEAD, or the first function at all when nothing
/// is exported. Used only where a graph is being rooted fresh at a file
/// (the initial launch, and package → function zoom) — everywhere else
/// just needs the parsed functions themselves.
type LoadedFunctions = (Vec<TsFunction>, Vec<TsFunction>, Vec<Alignment>, String);

fn load_functions(
    root: &std::path::Path,
    base_rev: &Rev,
    head_rev: &Rev,
    path: &std::path::Path,
) -> Option<LoadedFunctions> {
    let (base_fns, head_fns, alignment) = parse_functions(root, base_rev, head_rev, path)?;
    let focus = head_fns
        .iter()
        .find(|f| f.exported)
        .or_else(|| head_fns.first())
        .map(align::label)?;
    Some((base_fns, head_fns, alignment, focus))
}

/// Which functions in one file a change touched — added, removed, or
/// matched with a different body — outermost only, each with the line
/// to sort by. Body hash rather than call set: a handler whose only
/// change is a new argument inside a nested callback still *changed*,
/// even though it calls the same things.
fn changed_roots(entry: &FunctionFileEntry) -> Vec<(u32, String, Status)> {
    let mut changed_head: HashSet<usize> = HashSet::new();
    let mut added_head: HashSet<usize> = HashSet::new();
    let mut removed_base: HashSet<usize> = HashSet::new();
    let mut base_to_head: HashMap<usize, usize> = HashMap::new();
    for a in &entry.alignment {
        match a {
            Alignment::Matched { base, head, .. } => {
                base_to_head.insert(*base, *head);
                if entry.base_fns[*base].body_hash != entry.head_fns[*head].body_hash {
                    changed_head.insert(*head);
                }
            }
            Alignment::HeadOnly(h) => {
                changed_head.insert(*h);
                added_head.insert(*h);
            }
            Alignment::BaseOnly(b) => {
                removed_base.insert(*b);
            }
        }
    }
    fn outermost(fns: &[TsFunction], changed: impl Fn(usize) -> bool, idx: usize) -> bool {
        let mut p = fns[idx].parent;
        while let Some(i) = p {
            if changed(i) {
                return false;
            }
            p = fns[i].parent;
        }
        true
    }
    let mut out = Vec::new();
    for &h in &changed_head {
        if !outermost(&entry.head_fns, |i| changed_head.contains(&i), h) {
            continue;
        }
        let f = &entry.head_fns[h];
        let status = if added_head.contains(&h) {
            Status::Added
        } else {
            Status::Changed
        };
        out.push((f.start_line, align::label(f), status));
    }
    // A removed callback's ancestors are base functions; one of them
    // counts as changed if it was removed too, *or* if it survived into
    // HEAD with a different body — otherwise every callback a changed
    // handler rewrote would surface as its own "removed" root beside it.
    let base_changed = |i: usize| {
        removed_base.contains(&i)
            || base_to_head
                .get(&i)
                .is_some_and(|h| changed_head.contains(h))
    };
    for &b in &removed_base {
        if !outermost(&entry.base_fns, base_changed, b) {
            continue;
        }
        let f = &entry.base_fns[b];
        out.push((f.start_line, align::label(f), Status::Removed));
    }
    out
}

/// A function's calls with those of every *anonymous* function nested
/// inside it folded in (recursively), as (call text, line) in source
/// order. A named nested function is a boundary — it stays its own
/// node — but a callback is just more of this function's body.
fn inlined_calls(fns: &[TsFunction], idx: usize) -> Vec<(String, u32)> {
    let f = &fns[idx];
    let mut out: Vec<(String, u32)> = f
        .calls
        .iter()
        .cloned()
        .zip(f.call_lines.iter().copied())
        .collect();
    for (i, child) in fns.iter().enumerate() {
        if child.parent == Some(idx) && child.name.is_none() && child.route.is_none() {
            out.extend(inlined_calls(fns, i));
        }
    }
    out.sort_by_key(|(_, line)| *line);
    out
}

/// The named functions declared inside `idx`, looking through any
/// anonymous callbacks in between (those are folded into `idx`, so a
/// named function declared inside one of them belongs to `idx` too).
fn named_nested(fns: &[TsFunction], idx: usize, out: &mut Vec<usize>) {
    for (i, child) in fns.iter().enumerate() {
        if child.parent != Some(idx) {
            continue;
        }
        if child.name.is_some() || child.route.is_some() {
            out.push(i);
        } else {
            named_nested(fns, i, out);
        }
    }
}

/// The innermost function whose span contains `line`, if any.
fn innermost_at(fns: &[TsFunction], line: u32) -> Option<usize> {
    fns.iter()
        .enumerate()
        .filter(|(_, f)| f.start_line <= line && line <= f.end_line)
        .max_by_key(|(_, f)| f.start_line)
        .map(|(i, _)| i)
}

/// The function a text hit is attributed to: the innermost one, unless
/// that is an anonymous callback — a `$effect`, a `.then`, a handler
/// closure — in which case the nearest named (or routed) ancestor. A
/// callback is part of its function's body, not a caller of its own,
/// the same way the callee direction folds callbacks into the function.
fn named_enclosing(fns: &[TsFunction], idx: usize) -> usize {
    let mut i = idx;
    while fns[i].name.is_none() && fns[i].route.is_none() {
        match fns[i].parent {
            Some(p) => i = p,
            None => break,
        }
    }
    i
}

/// The named (or routed) functions of one file whose body — anonymous
/// callbacks folded in — makes one of `calls`, in source order.
fn enclosing_callers(fns: &[TsFunction], calls: &HashSet<String>) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for (i, f) in fns.iter().enumerate() {
        if !f.calls.iter().any(|c| calls.contains(c)) {
            continue;
        }
        let e = named_enclosing(fns, i);
        if !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

/// A caller found by text reference: the enclosing function as a
/// normal node, or — outside any function — a `module@<line>` node that
/// Enter can still open at that line but that has nothing to expand.
fn reference_fragment(path: &std::path::Path, key: &str, line: u32, status: Status) -> Fragment {
    if key == "module" {
        let label = format!("module@{line}");
        (make_fn_id(path, &label), label, status, false)
    } else {
        (
            make_fn_id(path, key),
            key.to_string(),
            status,
            status != Status::Removed,
        )
    }
}

/// Whether a HEAD function is new, has a changed body, or neither —
/// the same criterion `changed_roots` uses to pick roots.
fn body_status(entry: &FunctionFileEntry, label: &str) -> Status {
    let Some(h) = entry.head_fns.iter().position(|f| align::label(f) == label) else {
        return Status::Unchanged;
    };
    match base_counterpart(entry, h) {
        None => Status::Added,
        Some(b) if entry.base_fns[b].body_hash != entry.head_fns[h].body_hash => Status::Changed,
        Some(_) => Status::Unchanged,
    }
}

/// A BASE function's node label — its HEAD counterpart's when matched,
/// so a handler that only shifted lines stays one node.
fn base_label(entry: &FunctionFileEntry, base_idx: usize) -> String {
    align::base_label(&entry.base_fns, &entry.head_fns, &entry.alignment, base_idx)
}

/// The BASE index a HEAD function is aligned to, if it survived.
fn base_counterpart(entry: &FunctionFileEntry, head_idx: usize) -> Option<usize> {
    entry.alignment.iter().find_map(|a| match a {
        Alignment::Matched { base, head, .. } if *head == head_idx => Some(*base),
        _ => None,
    })
}

/// Go interfaces and method sets, by package directory.
#[derive(Default)]
struct GoIndex {
    /// ((directory, interface), the methods it declares).
    interfaces: Vec<((PathBuf, String), Vec<String>)>,
    /// (directory, type) → method → the file it is declared in.
    methods: HashMap<(PathBuf, String), HashMap<String, PathBuf>>,
}

/// More types than this implementing one interface, and a call through
/// it is not followed: it could be anything.
const MAX_IMPLEMENTATIONS: usize = 8;

/// Spec/test files are excluded from a whole-diff graph: a change is
/// asserted there, but it doesn't flow through them.
fn is_test_file(p: &std::path::Path) -> bool {
    let s = p.to_string_lossy();
    let in_test_dir = p.parent().is_some_and(|d| {
        d.components()
            .any(|c| matches!(c.as_os_str().to_str(), Some("tests" | "test" | "__tests__")))
    });
    s.contains(".spec.") || s.contains(".test.") || in_test_dir || crate::lang::is_test_name(p)
}

/// A method's own name without its type or class: `Store.Save` → `Save`.
fn short_name(label: &str) -> &str {
    label.rsplit('.').next().unwrap_or(label)
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Function-level node ids are qualified as `"<file>::<label>"`: once
/// the graph can cross into an importing file, a bare label is no
/// longer unique on its own.
fn debug_log(line: &str) {
    if let Ok(path) = std::env::var("PROGNOST_DEBUG") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// " ✓" when a file is fully seen and unflagged, " 3/12 seen" part-way,
/// nothing before the reader started.
fn seen_note(progress: Option<&(usize, usize, bool)>) -> String {
    match progress {
        Some((_, _, true)) => " ✓".to_string(),
        Some((s, t, false)) if *s > 0 => format!(" {s}/{t} seen"),
        _ => String::new(),
    }
}

/// Nodes after which an upstream walk stops looking for more callers.
const WALK_NODE_CAP: usize = 600;

fn is_ext_id(id: &str) -> bool {
    id.starts_with("pkg::")
}

/// `pkg::<package>::<call>` → (package, call).
fn split_pkg_id(id: &str) -> Option<(&str, &str)> {
    id.strip_prefix("pkg::")?.split_once("::")
}

fn make_fn_id(file: &std::path::Path, label: &str) -> String {
    format!("{}::{label}", file.display())
}

fn split_fn_id(id: &str) -> (PathBuf, &str) {
    match id.split_once("::") {
        Some((f, l)) => (PathBuf::from(f), l),
        None => (PathBuf::new(), id),
    }
}

/// Looks `label` up in a file's functions, HEAD first then BASE (a
/// removed function only exists in the latter), returning which list it
/// was found in alongside it — callers need that list to resolve the
/// function's own `parent` index correctly.
fn find_function<'a>(
    entry: &'a FunctionFileEntry,
    label: &str,
) -> Option<(&'a [TsFunction], &'a TsFunction)> {
    entry
        .head_fns
        .iter()
        .find(|f| align::label(f) == label)
        .map(|f| (entry.head_fns.as_slice(), f))
        .or_else(|| {
            (0..entry.base_fns.len())
                .find(|&i| base_label(entry, i) == label)
                .map(|i| (entry.base_fns.as_slice(), &entry.base_fns[i]))
        })
}

/// Whether a function-level label exists in each revision's list, as a
/// diff status — used for a synthetic node (an enclosing function
/// treated as a caller) that isn't itself one of `hub::build`'s outputs.
fn status_of_label(entry: &FunctionFileEntry, label: &str) -> Status {
    let in_head = entry.head_fns.iter().any(|f| align::label(f) == label);
    let in_base = (0..entry.base_fns.len()).any(|i| base_label(entry, i) == label);
    match (in_base, in_head) {
        (true, true) => Status::Unchanged,
        (false, true) => Status::Added,
        (true, false) => Status::Removed,
        (false, false) => Status::Unchanged,
    }
}

/// Whether a recorded call (a bare identifier, or a dotted/namespaced
/// chain) is plausibly a call to the imported binding named `label` —
/// exact match for a bare call, or the chain's last segment for a
/// namespace import (`poolLib.createPool(...)`).
fn call_matches(call_text: &str, label: &str) -> bool {
    call_text == label || call_text.ends_with(&format!(".{label}"))
}

/// A function-level node's identity (`align::label`) is stable but bare —
/// `anon@869` says nothing to a reviewer. Looks the id up in HEAD then
/// BASE (a removed node only exists in the latter) and renders something
/// a person can actually act on: a name plus the line to jump to, or —
/// for an anonymous callback, which has no name — which named function
/// it's nested inside, so "what is this" has an answer.
fn describe_function_label(head_fns: &[TsFunction], base_fns: &[TsFunction], id: &str) -> String {
    if let Some(f) = head_fns.iter().find(|f| align::label(f) == id) {
        return describe_ts_function(head_fns, f);
    }
    if let Some(f) = base_fns.iter().find(|f| align::label(f) == id) {
        return describe_ts_function(base_fns, f);
    }
    id.to_string()
}

fn describe_ts_function(fns: &[TsFunction], f: &TsFunction) -> String {
    let suffix = format!(" · L{}", f.start_line);
    let name = match (&f.route, &f.name) {
        (Some(route), _) => route.clone(),
        (None, Some(n)) => n.clone(),
        // "in X", not "callback in X": the enclosing label can now be a
        // route ("DELETE /:orderId"), and the longer prefix pushed it
        // past the column width into "DELETE /:…" — the one part that
        // carried the meaning was the part that got cut.
        (None, None) => format!("in {}", enclosing_name(fns, f.parent)),
    };
    // The line number is what lets a reviewer actually find this in the
    // source, so it must survive truncation even when the name doesn't.
    let budget = (MAX_COL_WIDTH as usize).saturating_sub(suffix.chars().count() + 1);
    format!("{}{suffix}", truncate(&name, budget))
}

fn enclosing_name(fns: &[TsFunction], parent: Option<usize>) -> String {
    match parent {
        None => "top level".to_string(),
        // A route handler is unnamed but still identifiable — "DELETE
        // /:orderId" says a lot more than continuing to climb past it in
        // search of a named ancestor and landing on the misleading "top
        // level" (nested three closures deep inside this exact handler
        // is not remotely "the top of the file").
        Some(idx) => match (&fns[idx].name, &fns[idx].route) {
            (Some(n), _) => n.clone(),
            (None, Some(route)) => route.clone(),
            (None, None) => enclosing_name(fns, fns[idx].parent),
        },
    }
}

/// A bare basename ("client.ts") is ambiguous the moment a monorepo has
/// two of them; a full path is unambiguous but usually too long for one
/// column. Keeps as many trailing path segments — ending in the
/// filename — as fit, since the filename is the part a reviewer actually
/// recognizes, and marks the cut with a leading `.../`.
fn shorten_path(path: &str, max_chars: usize) -> String {
    if path.chars().count() <= max_chars {
        return path.to_string();
    }
    let mut segments: Vec<&str> = path.split('/').collect();
    let mut tail = String::new();
    while let Some(seg) = segments.pop() {
        let candidate = if tail.is_empty() {
            seg.to_string()
        } else {
            format!("{seg}/{tail}")
        };
        if candidate.chars().count() + 4 > max_chars {
            break;
        }
        tail = candidate;
    }
    if tail.is_empty() {
        // Even the filename alone doesn't fit: keep the *filename*, cut
        // at its end — "apps/shop/backend/api/…" (the directory,
        // and nothing that identifies the file) is the worst possible
        // choice, and two long-named files in one directory would
        // collapse into the same label.
        return truncate(basename(path), max_chars);
    }
    truncate(&format!(".../{tail}"), max_chars)
}

/// Which row to open the file view scrolled to: the first *changed*
/// line inside the selected function (`[start, end]` in HEAD), with a
/// couple of rows of lead-in — the change is what Enter was pressed to
/// see, and a long handler's edit can sit a screen below its first
/// line. A function with no change of its own (an unchanged callee)
/// lands on its first line instead. The whole file is still there to
/// scroll through; this only picks where the reader lands. Also returns
/// the HEAD line landed on, for the header.
fn scroll_to_line(lines: &[DiffLine], start: u32, end: u32) -> (u16, Option<u32>) {
    let mut last_head = 0u32;
    let mut fn_start: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        if let Some(n) = l.head_line {
            last_head = n;
            if fn_start.is_none() && n >= start {
                fn_start = Some(i);
            }
        }
        // A removed line sits just after the last HEAD line seen.
        let at = l.head_line.unwrap_or(last_head + 1);
        if at > end {
            break;
        }
        if at >= start && l.changed {
            return (i.saturating_sub(2) as u16, Some(at));
        }
    }
    (fn_start.unwrap_or(0).saturating_sub(2) as u16, None)
}

/// Every line of the file, both revisions interleaved — unchanged lines
/// as they are in HEAD, added lines marked `+`, removed lines marked
/// `-` at the spot they were removed from — with a HEAD line-number
/// gutter, so "L855" in the graph and "855" here are the same thing.
/// Built once when the view opens rather than diffed again on every
/// redraw.
fn diff_lines(base_src: &str, head_src: &str) -> Vec<DiffLine> {
    use similar::ChangeTag;
    let diff = similar::TextDiff::from_lines(base_src, head_src);
    diff.iter_all_changes()
        .map(|change| {
            let content = change.value().trim_end_matches(['\n', '\r']);
            let tag = change.tag();
            let (marker, style, head_line) = match tag {
                ChangeTag::Equal => (
                    ' ',
                    Style::default().fg(Color::Gray),
                    change.new_index().map(|i| i as u32 + 1),
                ),
                ChangeTag::Insert => (
                    '+',
                    Style::default().fg(Color::Green),
                    change.new_index().map(|i| i as u32 + 1),
                ),
                ChangeTag::Delete => ('-', Style::default().fg(Color::Red), None),
            };
            let gutter = head_line.map(|n| n.to_string()).unwrap_or_default();
            DiffLine {
                text: format!("{gutter:>5} {marker} {content}"),
                style,
                head_line,
                changed: !matches!(tag, ChangeTag::Equal),
            }
        })
        .collect()
}
