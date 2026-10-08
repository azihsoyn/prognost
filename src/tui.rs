//! The screen. Auto-layout puts every node in a bordered group panel,
//! columns ordered by hops from the origin (callers left, dependencies
//! right); dragging one out of its panel is the one thing this tool lets
//! you do to a box that isn't expand, collapse, or open.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use ratatui_dnd::{Did, Drag, Hits};

use crate::graph::{Direction, Graph, Node};
use crate::rev::Rev;
use crate::workspace::Workspace;

const COL_WIDTH: u16 = 42;
const COL_GAP: u16 = 2;
const FLOAT_WIDTH: u16 = 36;
const FLOAT_HEIGHT: u16 = 3;

pub struct App {
    root: PathBuf,
    workspace: Workspace,
    graph: Graph,
    /// Who a node was reached from, and in which direction — the tree
    /// `collapse` walks back down.
    expanded_from: HashMap<String, (String, Direction)>,
    layer: HashMap<String, i32>,
    dragged: HashMap<String, (u16, u16)>,
    selected: Option<String>,
    status: String,
    drag: Drag<String>,
    hits: Hits<String>,
    quit: bool,
    /// Cached each draw so mouse handling can clamp a drop to the visible
    /// screen without re-reading the terminal.
    last_frame_w: u16,
    last_frame_h: u16,
}

impl App {
    pub fn new(root: PathBuf, workspace: Workspace, graph: Graph) -> Self {
        let mut layer = HashMap::new();
        for n in &graph.nodes {
            layer.insert(n.path.clone(), 0);
        }
        let selected = graph.nodes.first().map(|n| n.path.clone());
        Self {
            root,
            workspace,
            graph,
            expanded_from: HashMap::new(),
            layer,
            dragged: HashMap::new(),
            selected,
            status: "enter opens in hide · h/l expand callers/deps · c collapse · q quits".into(),
            drag: Drag::new(),
            hits: Hits::new(),
            quit: false,
            last_frame_w: 0,
            last_frame_h: 0,
        }
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|f| {
                self.draw(f);
                if !crate::color::enabled() {
                    crate::color::strip_buffer(f.buffer_mut());
                }
            })?;
            self.handle_event(event::read()?);
        }
        Ok(())
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press => self.handle_key(k.code),
            Event::Mouse(m) => {
                let hit = self.hits.at(m.column, m.row);
                // A press picks the node under it immediately, even before
                // a drag has decided whether this becomes a click or a lift.
                if matches!(m.kind, MouseEventKind::Down(_))
                    && let Some((key, _)) = &hit
                {
                    self.selected = Some(key.clone());
                }
                if let Did::Drop { key, x, y } = self.drag.on_mouse(m, hit) {
                    let x = x.min(self.last_frame_w.saturating_sub(FLOAT_WIDTH));
                    let y = y.min(self.last_frame_h.saturating_sub(FLOAT_HEIGHT));
                    self.dragged.insert(key.clone(), (x, y));
                    self.selected = Some(key);
                }
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Enter => self.open_selected(),
            KeyCode::Char('h') | KeyCode::Left => self.expand(Direction::Callers),
            KeyCode::Char('l') | KeyCode::Right => self.expand(Direction::Dependencies),
            KeyCode::Char('c') => self.collapse_selected(),
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            _ => {}
        }
    }

    fn ordered_paths(&self) -> Vec<String> {
        let mut nodes: Vec<&Node> = self.graph.nodes.iter().collect();
        nodes.sort_by_key(|n| {
            (
                self.layer.get(&n.path).copied().unwrap_or(0),
                n.group.clone(),
                n.path.clone(),
            )
        });
        nodes.into_iter().map(|n| n.path.clone()).collect()
    }

    fn move_selection(&mut self, delta: i32) {
        let order = self.ordered_paths();
        if order.is_empty() {
            return;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|s| order.iter().position(|p| p == s))
            .unwrap_or(0) as i32;
        let next = (at + delta).rem_euclid(order.len() as i32) as usize;
        self.selected = Some(order[next].clone());
    }

    fn open_selected(&mut self) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        self.status = format!("opening {path} in hide...");
        if let Err(e) = crate::hide::open(&self.root, &path, None) {
            self.status = format!("hide: {e}");
        }
    }

    fn expand(&mut self, direction: Direction) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        let target = PathBuf::from(&path);
        let fragment = crate::graph::reach(
            &self.root,
            &Rev::working(),
            &target,
            direction,
            &self.workspace,
        );
        if fragment.nodes.is_empty() {
            self.status = match direction {
                Direction::Callers => format!("no callers found for {path}"),
                Direction::Dependencies => format!("no workspace dependencies found for {path}"),
            };
            return;
        }
        let base_layer = self.layer.get(&path).copied().unwrap_or(0);
        let step = match direction {
            Direction::Callers => -1,
            Direction::Dependencies => 1,
        };
        let mut added = 0;
        for n in &fragment.nodes {
            if !self.graph.nodes.iter().any(|e| e.path == n.path) {
                self.layer
                    .entry(n.path.clone())
                    .or_insert(base_layer + step);
                self.expanded_from
                    .entry(n.path.clone())
                    .or_insert((path.clone(), direction));
                added += 1;
            }
        }
        self.status = format!("+{added} node(s)");
        self.graph.merge(fragment);
    }

    fn collapse_selected(&mut self) {
        let Some(root) = self.selected.clone() else {
            return;
        };
        let mut to_remove: Vec<String> = Vec::new();
        let mut frontier = vec![root.clone()];
        while let Some(cur) = frontier.pop() {
            for (child, (parent, _)) in &self.expanded_from {
                if *parent == cur && !to_remove.contains(child) {
                    to_remove.push(child.clone());
                    frontier.push(child.clone());
                }
            }
        }
        if to_remove.is_empty() {
            self.status = "nothing to collapse from here".into();
            return;
        }
        for p in &to_remove {
            self.expanded_from.remove(p);
            self.layer.remove(p);
            self.dragged.remove(p);
        }
        self.graph.nodes.retain(|n| !to_remove.contains(&n.path));
        self.graph
            .edges
            .retain(|e| !to_remove.contains(&e.from) && !to_remove.contains(&e.to));
        let live: std::collections::HashSet<&str> =
            self.graph.nodes.iter().map(|n| n.group.as_str()).collect();
        self.graph.groups.retain(|g| live.contains(g.dir.as_str()));
        self.status = format!("collapsed {} node(s)", to_remove.len());
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        self.last_frame_w = area.width;
        self.last_frame_h = area.height;
        self.hits.clear();

        let status = Rect {
            height: 1,
            y: area.y + area.height.saturating_sub(1),
            ..area
        };
        let context = Rect { height: 1, ..area };
        let header = Rect {
            height: 1,
            y: area.y + 1,
            ..area
        };
        let canvas = Rect {
            y: area.y + 2,
            height: area.height.saturating_sub(3),
            ..area
        };

        f.render_widget(Paragraph::new(self.context_line()), context);
        for (rect, label) in self.column_headers(canvas) {
            let hr = Rect {
                y: header.y,
                ..rect
            };
            f.render_widget(
                Paragraph::new(Span::styled(
                    label,
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                hr,
            );
        }

        for (group_dir, rect, nodes) in self.auto_layout(canvas) {
            let label = self
                .graph
                .groups
                .iter()
                .find(|g| g.dir == group_dir)
                .map(|g| g.label.as_str())
                .unwrap_or(&group_dir);
            let block = Block::default()
                .borders(Borders::ALL)
                .title(label.to_string());
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            for (i, path) in nodes.iter().enumerate() {
                let row = Rect {
                    y: inner.y + i as u16,
                    height: 1,
                    ..inner
                };
                if row.y >= inner.y + inner.height {
                    break;
                }
                if self.dragged.contains_key(path) {
                    continue;
                }
                f.render_widget(self.node_line(path, row.width), row);
                self.hits.put(row, path.clone());
            }
        }

        for (path, (x, y)) in self.dragged.clone() {
            let rect = Rect {
                x: x.min(area.width.saturating_sub(FLOAT_WIDTH)),
                y: y.min(area.height.saturating_sub(FLOAT_HEIGHT)),
                width: FLOAT_WIDTH,
                height: FLOAT_HEIGHT,
            };
            let group_label = self
                .graph
                .nodes
                .iter()
                .find(|n| n.path == path)
                .and_then(|n| self.graph.groups.iter().find(|g| g.dir == n.group))
                .map(|g| g.label.clone())
                .unwrap_or_default();
            let block = Block::default().borders(Borders::ALL).title(group_label);
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            f.render_widget(self.node_line(&path, inner.width), inner);
            self.hits.put(rect, path.clone());
        }

        if let Some(ghost) = self.drag.ghost(canvas)
            && let Some(key) = self.drag.moving()
        {
            let block = Block::default()
                .borders(Borders::ALL)
                .style(Style::default().fg(Color::Yellow));
            let inner = block.inner(ghost);
            f.render_widget(block, ghost);
            f.render_widget(self.node_line(key, inner.width), inner);
        }

        f.render_widget(Paragraph::new(self.status.as_str()), status);
    }

    /// What is selected, and what pressing `h`/`l`/`enter` would do to it —
    /// the one line meant to make the rest of the screen readable without
    /// having to already know the tool.
    fn context_line(&self) -> Line<'static> {
        let Some(path) = &self.selected else {
            return Line::from("nothing to select");
        };
        let name = basename(path);
        Line::from(vec![
            Span::styled("selected: ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                name.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  ({path})  ")),
            Span::styled(self.status.clone(), Style::default().fg(Color::Cyan)),
        ])
    }

    /// A direction label per column, placed above that column's panels.
    fn column_headers(&self, canvas: Rect) -> Vec<(Rect, &'static str)> {
        let mut layers: Vec<i32> = self
            .graph
            .nodes
            .iter()
            .map(|n| self.layer.get(&n.path).copied().unwrap_or(0))
            .collect();
        layers.sort_unstable();
        layers.dedup();
        layers
            .into_iter()
            .enumerate()
            .map_while(|(i, l)| {
                let x = canvas.x + i as u16 * (COL_WIDTH + COL_GAP);
                if x >= canvas.x + canvas.width {
                    return None;
                }
                let label = match l.cmp(&0) {
                    std::cmp::Ordering::Less => "\u{2190} callers",
                    std::cmp::Ordering::Equal => "origin",
                    std::cmp::Ordering::Greater => "dependencies \u{2192}",
                };
                Some((
                    Rect {
                        x,
                        width: COL_WIDTH,
                        height: 1,
                        y: canvas.y,
                    },
                    label,
                ))
            })
            .collect()
    }

    /// How a path is named when it is the *other* end of an arrow, not the
    /// row itself: `package/file.ts`. Either half alone collides too often
    /// in a real monorepo — `index.ts` and `client.ts` both recur across
    /// packages here — to identify a specific box on its own.
    fn far_ref(&self, path: &str) -> String {
        let group = self
            .graph
            .nodes
            .iter()
            .find(|n| n.path == path)
            .map(|n| n.group.as_str());
        let label = group
            .and_then(|dir| self.graph.groups.iter().find(|g| g.dir == dir))
            .map(|g| g.label.as_str())
            .unwrap_or(path);
        format!("{}/{}", basename(label), basename(path))
    }

    fn node_line(&self, path: &str, width: u16) -> Paragraph<'static> {
        let node = self.graph.nodes.iter().find(|n| n.path == path);
        let name = basename(path);
        let selected = self.selected.as_deref() == Some(path);
        let origin = node.is_some_and(|n| n.origin);
        // Every non-origin box says its own edge: who it is reached
        // through, and which way the import runs — a row never needs the
        // rest of the screen to be understood. The far end is named
        // package/filename: "index.ts" and "client.ts" each recur across
        // several packages in a real monorepo, so the filename alone does
        // not say which box is meant.
        let text = match self.expanded_from.get(path) {
            Some((parent, Direction::Callers)) => {
                format!("{name} \u{2192} {}", self.far_ref(parent))
            }
            Some((parent, Direction::Dependencies)) => {
                format!("{} \u{2192} {name}", self.far_ref(parent))
            }
            None => name.to_string(),
        };
        let mut style = Style::default();
        if origin {
            style = style.fg(Color::Yellow).add_modifier(Modifier::BOLD);
        }
        if selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let text = truncate(&text, width as usize);
        Paragraph::new(Line::from(Span::styled(text, style)))
    }

    /// One column per hop layer (callers negative, origin zero,
    /// dependencies positive), one bordered panel per group within a
    /// column, stacked top to bottom. Returns `(group dir, panel rect,
    /// member node paths)` — a node currently held in `dragged` is left
    /// out, it is drawn floating instead.
    fn auto_layout(&self, canvas: Rect) -> Vec<(String, Rect, Vec<String>)> {
        let mut by_layer: std::collections::BTreeMap<i32, Vec<&Node>> = Default::default();
        for n in &self.graph.nodes {
            let layer = self.layer.get(&n.path).copied().unwrap_or(0);
            by_layer.entry(layer).or_default().push(n);
        }

        let mut out = Vec::new();
        for (col_index, (_, mut nodes)) in by_layer.into_iter().enumerate() {
            nodes.sort_by(|a, b| a.group.cmp(&b.group).then(a.path.cmp(&b.path)));
            let mut by_group: Vec<(String, Vec<String>)> = Vec::new();
            for n in nodes {
                match by_group.iter_mut().find(|(g, _)| *g == n.group) {
                    Some((_, list)) => list.push(n.path.clone()),
                    None => by_group.push((n.group.clone(), vec![n.path.clone()])),
                }
            }
            let x = canvas.x + col_index as u16 * (COL_WIDTH + COL_GAP);
            if x >= canvas.x + canvas.width {
                break;
            }
            let mut y = canvas.y;
            for (group_dir, members) in by_group {
                let visible: Vec<String> = members
                    .into_iter()
                    .filter(|p| !self.dragged.contains_key(p))
                    .collect();
                if visible.is_empty() {
                    continue;
                }
                if y >= canvas.y + canvas.height {
                    break;
                }
                let height = (visible.len() as u16 + 2).min(canvas.y + canvas.height - y);
                if height < 3 {
                    break;
                }
                let rect = Rect {
                    x,
                    y,
                    width: COL_WIDTH.min(canvas.width.saturating_sub(x - canvas.x)),
                    height,
                };
                out.push((group_dir, rect, visible));
                y += height;
            }
        }
        out
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else if width <= 1 {
        s.chars().take(width).collect()
    } else {
        format!("{}…", s.chars().take(width - 1).collect::<String>())
    }
}
