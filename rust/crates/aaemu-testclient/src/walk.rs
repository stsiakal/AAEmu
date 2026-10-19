//! Terrain walker: paced `CSMoveUnitPacket`s at the 1.2 player run speed, with the Z from the
//! server's own `WorldHeightmap` (read from the 1.2 `game_pak` through a `ClientFileManager`).
//!
//! The server applies client positions without collision checks (`world::session::move_unit`
//! only rebroadcasts them, and `SusManager` reports flat speeds above 25 m/s). A player-mode bot
//! therefore has to keep itself on the terrain:
//!
//! * steps are paced at [`MOVE_INTERVAL`] (the client's ~10 Hz movement rate) and scaled by the
//!   1.2 run speed (`game_stances.max_speed` 5.4 m/s for the Nuian actor model 10, stance 2
//!   `relaxed`; the AI's own `Npc.MoveTowards` sends a fixed 4000 velocity for a step, i.e. the
//!   wire velocity is millimetres per second);
//! * Z follows `WorldHeightmap::get_height(files, x, y)` (bilinear over the 2 m grid), a drop
//!   beyond [`WalkerConfig::min_fall_drop_m`] carries a realistic `fall_vel`
//!   (`√(2·g·h)` in the client's units, [`FALL_VEL_UNITS_PER_MPS`]);
//! * steep slopes, the world's ocean level and (optionally) a water level block a step; small
//!   detours (±20° / ±40°) try to walk around them;
//! * a blocked step or a server position that does not move for [`WalkerConfig::stuck_timeout`]
//!   returns [`Step::Teleport`] with a [`BlockReason`] and counts it, so the runner records it as
//!   its `teleport` fallback (`path_blocked` / `water` / `cliff`).
//!
//! The walker never touches the quest runner; [`walk`] is the optional live `walk` subcommand's
//! driver (login, select/spawn, walk, report). Navmesh routing and collision sweeps are the
//! separate path-planner step; this module only needs the terrain.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aaemu_commons::PacketStream;
use aaemu_game::data::client_files::ClientFileManager;
use aaemu_game::data::world_templates::{WorldTemplate, WorldTemplates, REGION_SIZE};
use aaemu_game::models::movement::convert_radian_to_direction;
use aaemu_game::opcodes::cs;
use aaemu_game::systems::ai::collision::fall::FALL_VEL_UNITS_PER_MPS;
use aaemu_game::systems::ai::geodata::astar::PathNode;
use aaemu_game::systems::ai::geodata::bai::UsedAreaNames;
use aaemu_game::systems::ai::geodata::graph::DEFAULT_AGENT_RADIUS;
use aaemu_game::systems::ai::geodata::manager::{AiGeoData, GeoError, TerrainHeights};
use aaemu_game::systems::ai::geodata::ClientBaiFiles;
use aaemu_game::systems::npcs::heightmap::WorldHeightmap;
use aaemu_game::systems::skills::controllers::{add_distance_to_front, Vec3};
use anyhow::{bail, Context};
use serde::Serialize;

use crate::game::GameClient;
use crate::questrun::{self, RunOptions};
use crate::record::Recorder;
use crate::scenario::BotConfig;

/// The 1.2 movement tick: the client sends a `CSMoveUnitPacket` about every 100 ms.
pub const MOVE_INTERVAL: Duration = Duration::from_millis(100);
/// `game_stances.max_speed` of the Nuian actor model (10), stance 2 `relaxed`.
pub const DEFAULT_RUN_SPEED_MPS: f32 = 5.4;
/// `SusManager::PLAYER_CHECK_SPEED` (systems/services/sus.rs): never walk near it.
pub const MAX_ALLOWED_SPEED_MPS: f32 = 25.0;
/// The 1.2 client's `fall_vel` units per m/s (see `collision::fall`).
pub const FALL_UNITS_PER_MPS: f32 = FALL_VEL_UNITS_PER_MPS;
/// Gravity the client's `fall_vel` follows in the server's zone cap (`collision::fall`).
const FALL_GRAVITY: f32 = 20.0;
/// Paths are stored in 256 m blocks (`game/worlds/<world>/paths/<x>_<y>/...`).
const PATH_SECTOR_M: i32 = 256;
/// `WorldManager.CELL_SIZE`.
const CELL_SIZE_M: i32 = 1024;

/// One navmesh search covers at most this distance (the server's A* is bounded).
const NAV_DIRECT_MAX_M: f32 = 450.0;
/// Length of a navmesh leg of a long route.
const NAV_LEG_M: f32 = 300.0;

/// A walk route: the waypoints after the start, the last one being the target.
pub type Route = Vec<(f32, f32, f32)>;

/// Where the walker loads its terrain from.
pub struct Terrain {
    files: Arc<ClientFileManager>,
    map: Arc<WorldHeightmap>,
    /// The server's own navmesh (`.bai` files of the 1.2 `game_pak`), when loaded.
    nav: Option<NavMesh>,
    /// `WorldTemplate.OceanLevel`.
    pub ocean_level: f32,
    pub world_name: String,
    pub cells: (i32, i32),
}

impl Terrain {
    /// Opens the 1.2 `game_pak` and loads `main_world`'s heightmap table (cells load on demand).
    pub fn load(game_pak: &Path, app_path: &Path) -> anyhow::Result<Self> {
        if !game_pak.is_file() {
            bail!("game_pak not found: {}", game_pak.display());
        }
        let files = Arc::new(ClientFileManager::from_sources(
            &[game_pak.to_string_lossy().into_owned()],
            app_path,
        ));
        if files.sources().is_empty() {
            bail!(
                "the game_pak {} could not be opened as a client source",
                game_pak.display()
            );
        }
        let worlds = WorldTemplates::load(&files, app_path)
            .context("loading the world templates from the game_pak")?;
        let template = worlds
            .get_world_template_by_name("main_world")
            .context("main_world is not in the game_pak")?;
        let map = Arc::new(WorldHeightmap::new(template));
        let nav = Some(NavMesh::new(template, files.clone(), map.clone()));
        Ok(Self {
            files,
            map,
            nav,
            ocean_level: template.ocean_level,
            world_name: template.name.clone(),
            cells: (template.cell_x, template.cell_y),
        })
    }

    /// A heightmap without client files (unit tests / tools); the cells must already be
    /// inserted through [`WorldHeightmap::insert_cell`].
    pub fn from_heightmap(map: WorldHeightmap, ocean_level: f32) -> Self {
        Self {
            files: Arc::new(ClientFileManager::new()),
            world_name: map.name.clone(),
            cells: (map.cell_x, map.cell_y),
            map: Arc::new(map),
            nav: None,
            ocean_level,
        }
    }

    /// `WorldTemplate.GetHeight(x, y)` (bilinear, 2 m grid); the cell is loaded on demand.
    pub fn height(&self, x: f32, y: f32) -> anyhow::Result<f32> {
        self.map
            .get_height(&self.files, x, y)
            .map_err(|e| anyhow::anyhow!("heightmap of {}: {e}", self.world_name))
    }

    /// The client files the heightmap is read from.
    pub fn files(&self) -> &Arc<ClientFileManager> {
        &self.files
    }

    /// What the 1.2 `game_pak` ships for the route: heightmaps, navmesh `.bai` blocks and the
    /// CryPhysics collision files, for every 256 m path block / 1 km cell the line touches.
    pub fn pak_info(&self, from: (f32, f32, f32), to: (f32, f32, f32)) -> anyhow::Result<String> {
        let Some(source) = self.files.sources().first() else {
            bail!("no client source");
        };
        let Some(pak) = source.game_pak() else {
            bail!("the client source is not a game_pak");
        };
        let mut out = String::new();
        let _ = writeln!(out, "game_pak: {}", source.path_name());
        let _ = writeln!(
            out,
            "world: {} (cells {}x{}, {CELL_SIZE_M} m each, ocean level {})",
            self.world_name, self.cells.0, self.cells.1, self.ocean_level
        );
        let (mut bai, mut heightmaps, mut objects, mut big_objects) = (0usize, 0, 0, 0);
        for name in pak.file_names() {
            let lower = name.to_lowercase();
            if lower.ends_with(".bai") && lower.contains("/paths/") {
                bai += 1;
            } else if lower.ends_with("/terrain/heightmap.dat") {
                heightmaps += 1;
            } else if lower.ends_with("/object.dat") {
                objects += 1;
            } else if lower.ends_with("/big_object.dat") {
                big_objects += 1;
            }
        }
        let _ = writeln!(
            out,
            "main_world: {bai} .bai path files, {heightmaps} heightmap.dat, {objects} object.dat, \
             {big_objects} big_object.dat"
        );

        let mut sectors = BTreeSet::new();
        let mut cells = BTreeSet::new();
        let dx = to.0 - from.0;
        let dy = to.1 - from.1;
        let dist = (dx * dx + dy * dy).sqrt();
        let steps = ((dist / 64.0).ceil() as usize).max(1);
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let (x, y) = (from.0 + dx * t, from.1 + dy * t);
            sectors.insert((
                (x as i32).div_euclid(PATH_SECTOR_M),
                (y as i32).div_euclid(PATH_SECTOR_M),
            ));
            cells.insert((
                (x as i32).div_euclid(CELL_SIZE_M),
                (y as i32).div_euclid(CELL_SIZE_M),
            ));
        }
        let _ = writeln!(
            out,
            "route: {dist:.1} m, {} path blocks, {} cells",
            sectors.len(),
            cells.len()
        );
        for (sx, sy) in &sectors {
            let dir = format!("game/worlds/{}/paths/{sx:03}_{sy:03}", self.world_name);
            let has = |f: &str| self.files.file_exists(&format!("{dir}/{f}"));
            let _ = writeln!(
                out,
                "  sector {sx:03}_{sy:03}: netmission0={} vertsmission0={} areasmission0={} \
                 hidemission0={}",
                has("netmission0.bai"),
                has("vertsmission0.bai"),
                has("areasmission0.bai"),
                has("hidemission0.bai")
            );
        }
        for (cx, cy) in &cells {
            let dir = format!(
                "game/worlds/{}/cells/{cx:03}_{cy:03}/client",
                self.world_name
            );
            let has = |f: &str| self.files.file_exists(&format!("{dir}/{f}"));
            let _ = writeln!(
                out,
                "  cell {cx:03}_{cy:03}: heightmap.dat={} object.dat={} big_object.dat={} \
                 statobjs.dat={}",
                has("terrain/heightmap.dat"),
                has("object.dat"),
                has("big_object.dat"),
                has("statobjs.dat")
            );
        }
        if let Ok(z) = self.height(to.0, to.1) {
            let _ = writeln!(out, "terrain height at target ({}, {}): {z}", to.0, to.1);
        }
        Ok(out)
    }

    /// A walkable route over the heightmap: an A* grid (2 m, the heightmap's own resolution) that
    /// rejects water and steps the walker could not take (climb above `max_slope_deg`, a drop
    /// deeper than `max_drop_m`). `None` when the target itself is not walkable or no route
    /// exists; the caller then falls back to the straight line (and the walker's counted
    /// teleport when that is blocked).
    ///
    /// This is deliberately terrain-only (no `.bai`, no collision sweeps): those make up the
    /// separate path-planner step. It keeps the live walker from walking into the sea or up a
    /// cliff when a straight line is not walkable.
    pub fn route(
        &self,
        from: (f32, f32, f32),
        to: (f32, f32, f32),
        config: &WalkerConfig,
    ) -> Option<Vec<(f32, f32, f32)>> {
        self.route_with_source(from, to, config).map(|(r, _)| r)
    }

    /// The route and where it came from: `navmesh` (the server's `.bai` navmesh, the same
    /// `PathNode::find_smooth_path` the AI follows) when it has one, else `heightmap` (the
    /// terrain grid search of [`Terrain::grid_route`]).
    pub fn route_with_source(
        &self,
        from: (f32, f32, f32),
        to: (f32, f32, f32),
        config: &WalkerConfig,
    ) -> Option<(Route, &'static str)> {
        let dist = dist_2d(from, to);
        if let Some(nav) = &self.nav {
            // The server's A* is bounded (`dist * 20 + 500` expansions), so only short routes
            // come back complete from one search.
            if dist <= NAV_DIRECT_MAX_M {
                if let Some(path) = nav.route(from, to) {
                    return Some((path, "navmesh"));
                }
            }
        }
        if dist <= 700.0 {
            return self.grid_route(from, to, config).map(|r| (r, "heightmap"));
        }
        // Long route: a coarse heightmap plan avoids sea and cliffs, then the navmesh refines
        // every ~300 m leg of it (a leg without a navmesh route keeps its coarse points).
        let coarse = self.coarse_route(from, to, config)?;
        let Some(nav) = &self.nav else {
            return Some((coarse, "heightmap"));
        };
        let mut anchors: Vec<usize> = Vec::new();
        let mut since = 0.0f32;
        let mut prev = from;
        for (i, p) in coarse.iter().enumerate() {
            since += dist_2d(prev, *p);
            prev = *p;
            if since >= NAV_LEG_M && i + 1 < coarse.len() {
                anchors.push(i);
                since = 0.0;
            }
        }
        anchors.push(coarse.len() - 1);
        let mut route: Route = Vec::new();
        let mut legs_nav = 0usize;
        let mut legs_grid = 0usize;
        let (mut leg_start, mut leg_from) = (0usize, from);
        for &end in &anchors {
            let leg_to = coarse[end];
            match nav.route(leg_from, leg_to) {
                Some(points) => {
                    route.extend(points);
                    legs_nav += 1;
                }
                None => {
                    route.extend(coarse[leg_start..=end].iter().copied());
                    legs_grid += 1;
                }
            }
            leg_start = end + 1;
            leg_from = leg_to;
        }
        let source = if legs_grid == 0 {
            "navmesh"
        } else if legs_nav == 0 {
            "heightmap"
        } else {
            "navmesh+heightmap"
        };
        Some((route, source))
    }

    /// A coarse heightmap route for a long walk: the grid grows from 2 m until the search
    /// fits, the margin is a fifth of the distance.
    fn coarse_route(
        &self,
        from: (f32, f32, f32),
        to: (f32, f32, f32),
        config: &WalkerConfig,
    ) -> Option<Route> {
        let dist = dist_2d(from, to);
        let margin = (dist * 0.2).clamp(500.0, 1500.0);
        for step in [2.0f32, 4.0, 6.0, 8.0, 12.0, 16.0] {
            let w = ((to.0 - from.0).abs() + 2.0 * margin) / step + 2.0;
            let h = ((to.1 - from.1).abs() + 2.0 * margin) / step + 2.0;
            if w * h <= 850_000.0 {
                return self.grid_route_with(from, to, config, step, margin);
            }
        }
        None
    }

    /// Whether the `.bai` navmesh is loaded for the walker.
    pub fn has_navmesh(&self) -> bool {
        self.nav.is_some()
    }

    /// The terrain-only planner (2 m grid A* over the heightmap, no navmesh).
    pub fn grid_route(
        &self,
        from: (f32, f32, f32),
        to: (f32, f32, f32),
        config: &WalkerConfig,
    ) -> Option<Vec<(f32, f32, f32)>> {
        self.grid_route_with(from, to, config, 2.0, 500.0)
    }

    /// [`Terrain::grid_route`] with a coarser grid for long distances: the same A* over a grid
    /// of `step` metres inside the bounding box of the route grown by `margin`.
    pub fn grid_route_with(
        &self,
        from: (f32, f32, f32),
        to: (f32, f32, f32),
        config: &WalkerConfig,
        step: f32,
        margin: f32,
    ) -> Option<Vec<(f32, f32, f32)>> {
        const MAX_NODES: usize = 900_000;
        let (min_x, min_y) = (from.0.min(to.0) - margin, from.1.min(to.1) - margin);
        let (max_x, max_y) = (from.0.max(to.0) + margin, from.1.max(to.1) + margin);
        let min_x = min_x.max(0.0);
        let min_y = min_y.max(0.0);
        let max_x = max_x.min(self.cells.0 as f32 * CELL_SIZE_M as f32);
        let max_y = max_y.min(self.cells.1 as f32 * CELL_SIZE_M as f32);
        let w = (((max_x - min_x) / step).ceil() as i32 + 1).max(2);
        let h = (((max_y - min_y) / step).ceil() as i32 + 1).max(2);
        let n = w as usize * h as usize;
        if n > MAX_NODES || w < 2 || h < 2 {
            return None;
        }
        let idx = |ix: i32, iy: i32| (ix * h + iy) as usize;
        let pos = |ix: i32, iy: i32| (min_x + ix as f32 * step, min_y + iy as f32 * step);
        let mut heights = vec![f32::NAN; n];
        for ix in 0..w {
            for iy in 0..h {
                let (x, y) = pos(ix, iy);
                heights[idx(ix, iy)] = self.height(x, y).unwrap_or(f32::NAN);
            }
        }
        let water = config.water_level;
        let max_slope = config.max_slope_deg.to_radians().tan();
        let node_ok = |i: usize| -> bool {
            let z = heights[i];
            z.is_finite() && water.is_none_or(|level| z > level + 0.05)
        };
        let goal = (
            ((to.0 - min_x) / step).round() as i32,
            ((to.1 - min_y) / step).round() as i32,
        );
        let start = (
            ((from.0 - min_x) / step).round() as i32,
            ((from.1 - min_y) / step).round() as i32,
        );
        if start.0 < 0 || start.1 < 0 || start.0 >= w || start.1 >= h {
            return None;
        }
        if goal.0 < 0 || goal.1 < 0 || goal.0 >= w || goal.1 >= h {
            return None;
        }
        let start_i = idx(start.0, start.1);
        let goal_i = idx(goal.0, goal.1);
        if !node_ok(start_i) || !node_ok(goal_i) {
            return None;
        }
        let cost = |a: usize, b: usize, d: f32| -> Option<f32> {
            let dz = heights[b] - heights[a];
            if dz / d > max_slope || dz < -config.max_drop_m {
                return None;
            }
            Some(d)
        };
        let heuristic = |i: usize| -> f32 {
            let ix = (i / h as usize) as i32;
            let iy = (i % h as usize) as i32;
            let dx = (goal.0 - ix).abs() as f32;
            let dy = (goal.1 - iy).abs() as f32;
            // Octile distance.
            (dx + dy) + (std::f32::consts::SQRT_2 - 2.0) * dx.min(dy)
        };
        let mut g = vec![f32::INFINITY; n];
        let mut parent = vec![-1i32; n];
        let mut closed = vec![false; n];
        let mut open: std::collections::BinaryHeap<(std::cmp::Reverse<u64>, usize)> =
            std::collections::BinaryHeap::new();
        g[start_i] = 0.0;
        open.push((
            std::cmp::Reverse((heuristic(start_i) * 100.0) as u64),
            start_i,
        ));
        let mut found = false;
        while let Some((_, current)) = open.pop() {
            if current == goal_i {
                found = true;
                break;
            }
            if closed[current] {
                continue;
            }
            closed[current] = true;
            let ix = (current / h as usize) as i32;
            let iy = (current % h as usize) as i32;
            for (dx, dy) in [
                (-1, 0),
                (1, 0),
                (0, -1),
                (0, 1),
                (-1, -1),
                (-1, 1),
                (1, -1),
                (1, 1),
            ] {
                let (nx, ny) = (ix + dx, iy + dy);
                if nx < 0 || ny < 0 || nx >= w || ny >= h {
                    continue;
                }
                let ni = idx(nx, ny);
                if closed[ni] || !node_ok(ni) {
                    continue;
                }
                let d = if dx == 0 || dy == 0 {
                    step
                } else {
                    step * std::f32::consts::SQRT_2
                };
                let Some(step) = cost(current, ni, d) else {
                    continue;
                };
                let ng = g[current] + step;
                if ng + 1e-4 >= g[ni] {
                    continue;
                }
                g[ni] = ng;
                parent[ni] = current as i32;
                open.push((std::cmp::Reverse(((ng + heuristic(ni)) * 100.0) as u64), ni));
            }
        }

        if !found {
            return None;
        }
        let mut nodes = Vec::new();
        let mut current = goal_i;
        while current != start_i {
            nodes.push(current);
            let p = parent[current];
            if p < 0 {
                return None;
            }
            current = p as usize;
        }

        nodes.reverse();
        // Keep the endpoints of every straight run (the turn points), never a sparse subset of
        // a staircase: a straight line between two otherwise collinear nodes would cut corners.
        let coord = |i: usize| {
            (
                (nodes[i] / h as usize) as i32,
                (nodes[i] % h as usize) as i32,
            )
        };
        let mut route = Vec::with_capacity(nodes.len());
        for i in 0..nodes.len() {
            let keep = if i == 0 || i + 1 == nodes.len() {
                true
            } else {
                let (ax, ay) = coord(i - 1);
                let (bx, by) = coord(i);
                let (cx, cy) = coord(i + 1);
                (bx - ax, by - ay) != (cx - bx, cy - by)
            };
            if keep {
                let (ix, iy) = coord(i);
                let (x, y) = pos(ix, iy);
                route.push((x, y, heights[nodes[i]]));
            }
        }
        let (zx, zy) = (to.0, to.1);
        let zz = self.height(zx, zy).unwrap_or(to.2);
        if route.is_empty() {
            route.push((zx, zy, zz));
        } else {
            let last = route.len() - 1;
            route[last] = (zx, zy, zz);
        }
        Some(route)
    }

    /// Terrain profile along the line: height every 4 m and the steepest positive / negative
    /// grade the walker would see at its 0.54 m step (1 m samples). Prints the samples beyond
    /// 30° with their grade, so a "cliff" fallback can be explained from the data.
    pub fn slope_profile(
        &self,
        from: (f32, f32, f32),
        to: (f32, f32, f32),
    ) -> anyhow::Result<String> {
        let mut out = String::new();
        let dx = to.0 - from.0;
        let dy = to.1 - from.1;
        let dist = (dx * dx + dy * dy).sqrt();
        let _ = writeln!(
            out,
            "terrain profile ({}, {}) -> ({}, {}) ({dist:.1} m):",
            from.0, from.1, to.0, to.1
        );
        let samples = (dist.ceil() as usize).max(1);
        let (mut prev, mut max_up, mut max_down) = (None::<(f32, f32, f32)>, 0.0f32, 0.0f32);
        for i in 0..=samples {
            let t = i as f32 / samples as f32;
            let (x, y) = (from.0 + dx * t, from.1 + dy * t);
            let z = self.height(x, y).unwrap_or(f32::NAN);
            if let Some((px, py, pz)) = prev {
                let d = ((x - px) * (x - px) + (y - py) * (y - py)).sqrt();
                if d > 0.0 && z.is_finite() && pz.is_finite() {
                    let grade = (z - pz) / d;
                    max_up = max_up.max(grade);
                    max_down = max_down.min(grade);
                    if grade.abs() >= 0.577 && i % 2 == 0 {
                        let _ = writeln!(
                            out,
                            "  ({x:.1}, {y:.1}, {z:.2}) grade {grade:+.2} ({:.0}°)",
                            grade.atan().to_degrees()
                        );
                    }
                }
            }
            if i % 8 == 0 {
                let _ = writeln!(out, "  {i:>3} m ({x:.1}, {y:.1}): z {z:.2}");
            }
            prev = Some((x, y, z));
        }
        let _ = writeln!(
            out,
            "max climb grade {:+.2} ({:.1}°), max drop grade {:+.2} ({:.1}°)",
            max_up,
            max_up.atan().to_degrees(),
            max_down,
            max_down.atan().to_degrees()
        );
        Ok(out)
    }
}

/// The server's heightmap as the geodata's terrain fallback.
struct WalkHeights {
    files: Arc<ClientFileManager>,
    map: Arc<WorldHeightmap>,
}

impl TerrainHeights for WalkHeights {
    fn raw_height(&self, x: i32, y: i32) -> Result<f32, GeoError> {
        self.map
            .get_raw_height_map_height(&self.files, x, y)
            .map_err(|_| GeoError)
    }

    fn height(&self, x: f32, y: f32) -> Result<f32, GeoError> {
        self.map.get_height(&self.files, x, y).map_err(|_| GeoError)
    }
}

/// The 1.2 `game_pak`'s `.bai` navmesh behind the server's own [`AiGeoData`]: the same files
/// (`game/worlds/<world>/paths/<x>_<y>/netmission*.bai`, 256 m blocks loaded per cell on first
/// use) and the same smooth-path search the NPCs chase with, so a bot path is a path an NPC
/// could walk too.
pub struct NavMesh {
    geo: AiGeoData,
    template: WorldTemplate,
}

impl NavMesh {
    pub fn new(
        template: &WorldTemplate,
        files: Arc<ClientFileManager>,
        map: Arc<WorldHeightmap>,
    ) -> Self {
        let terrain: Arc<dyn TerrainHeights> = Arc::new(WalkHeights {
            files: files.clone(),
            map,
        });
        let bai = Arc::new(ClientBaiFiles(files));
        let geo = AiGeoData::new(
            template,
            true,
            bai,
            Arc::new(UsedAreaNames::default()),
            Some(terrain),
        );
        Self {
            geo,
            template: template.clone(),
        }
    }

    /// The zone key of a position (`ZoneKeyByRegions`).
    fn zone_key(&self, p: (f32, f32)) -> u32 {
        self.template
            .zone_key_at(p.0 as i32 / REGION_SIZE, p.1 as i32 / REGION_SIZE)
            .unwrap_or(0)
    }

    /// The navmesh route from `from` to `to` without the start point (the last point is `to`),
    /// `None` when the navmesh has no triangle at either end or no complete corridor.
    pub fn route(
        &self,
        from: (f32, f32, f32),
        to: (f32, f32, f32),
    ) -> Option<Vec<(f32, f32, f32)>> {
        let mut node = PathNode {
            zone_key: self.zone_key((from.0, from.1)),
            ..PathNode::default()
        };
        let start = Vec3::new(from.0, from.1, from.2);
        let goal = Vec3::new(to.0, to.1, to.2);
        let smooth = node
            .find_smooth_path(&self.geo, start, goal, DEFAULT_AGENT_RADIUS)
            .ok()
            .flatten()?;
        let mut points: Vec<(f32, f32, f32)> = smooth
            .points
            .iter()
            .skip(1)
            .map(|p| (p.x, p.y, p.z))
            .collect();
        if points.is_empty() {
            points.push(to);
        }
        // The last point is the requested target itself (its height comes from the terrain).
        let last = points.len() - 1;
        points[last] = to;
        Some(points)
    }
}

/// The 1.2 run speed for `race` from the same `compact.sqlite3` the server loads:
/// `characters.model_id` -> `game_stances.max_speed` (stance 2 `relaxed`, male template).
pub fn base_run_speed(compact_db: &Path, race: u8) -> anyhow::Result<f32> {
    use rusqlite::OptionalExtension;
    let conn = rusqlite::Connection::open_with_flags(
        compact_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .with_context(|| format!("opening {}", compact_db.display()))?;
    let model: Option<u32> = conn
        .query_row(
            "SELECT model_id FROM characters WHERE char_race_id = ?1 AND char_gender_id = 1",
            [race],
            |r| r.get(0),
        )
        .optional()?;
    let model = model.with_context(|| format!("no character template for race {race}"))?;
    let speed: Option<f64> = conn
        .query_row(
            "SELECT max_speed FROM game_stances WHERE actor_model_id = ?1 AND stance_id = 2",
            [model],
            |r| r.get(0),
        )
        .optional()?;
    let speed = speed.with_context(|| format!("no relaxed stance for actor model {model}"))?;
    Ok(speed as f32)
}

/// Why a step was refused / a teleport fallback was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockReason {
    /// No valid step (missing height data / not progressing).
    PathBlocked,
    /// The candidate terrain is at or below the water level.
    Water,
    /// The slope up or down exceeds [`WalkerConfig::max_slope_deg`] / the drop is too deep.
    Cliff,
}

impl BlockReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PathBlocked => "path_blocked",
            Self::Water => "water",
            Self::Cliff => "cliff",
        }
    }
}

/// One counted teleport fallback, ready to be recorded by the runner.
#[derive(Debug, Clone, Serialize)]
pub struct WalkFallback {
    /// `teleport` (the runner's `Fallback` kind; it uses the GM `/move` command).
    pub kind: &'static str,
    pub target: String,
    /// `path_blocked` / `water` / `cliff`.
    pub reason: String,
}

/// Walker knobs. [`Default`] is the Nuian 1.2 run (5.4 m/s, 10 Hz, 65° climb / 3 m drop).
#[derive(Debug, Clone, Copy)]
pub struct WalkerConfig {
    pub speed_mps: f32,
    pub interval: Duration,
    /// Stop within this horizontal distance of the target.
    pub arrive_radius: f32,
    /// A blocked step / a server position that does not move for this long becomes a teleport.
    pub stuck_timeout: Duration,
    pub max_slope_deg: f32,
    /// A single step may not drop more than this (metres).
    pub max_drop_m: f32,
    /// `None` = no water check; the live driver passes the world's ocean level.
    pub water_level: Option<f32>,
    /// A drop at least this deep carries a `fall_vel` (metres).
    pub min_fall_drop_m: f32,
    /// Teleports the driver may take before giving up.
    pub max_teleports: u32,
}

impl Default for WalkerConfig {
    fn default() -> Self {
        Self {
            speed_mps: DEFAULT_RUN_SPEED_MPS,
            interval: MOVE_INTERVAL,
            arrive_radius: 1.5,
            stuck_timeout: Duration::from_secs(8),
            // The 2 m heightmap grid exaggerates grades: the walkable Nuian hillsides show up
            // to ~60°. Anything steeper is treated as a cliff; descents are limited by
            // `max_drop_m` (a step down to a lower ledge is a real, short fall).
            max_slope_deg: 65.0,
            max_drop_m: 3.0,
            water_level: None,
            min_fall_drop_m: 0.3,
            max_teleports: 3,
        }
    }
}

/// The packet fields of one paced move.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveStep {
    pub pos: (f32, f32, f32),
    /// Wire velocity (mm/s, like `Npc.MoveTowards`'s fixed 4000).
    pub velocity: (i16, i16, i16),
    pub yaw: i8,
    /// `Some` when the step drops (the `fall_vel` actor flag is set).
    pub fall_vel: Option<u16>,
}

/// What the walker wants next.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    Move(MoveStep),
    /// Nothing to send yet (paced at [`WalkerConfig::interval`]).
    Wait {
        until: Duration,
    },
    Arrived {
        distance: f32,
    },
    /// The step is refused this tick; the driver sends nothing.
    Blocked {
        reason: BlockReason,
    },
    /// Counted fallback: the driver teleports to `at` and records it.
    Teleport {
        reason: BlockReason,
        at: (f32, f32, f32),
    },
}

/// The terrain walker (pure state machine; the live driver sends the packets).
pub struct Walker {
    terrain: Arc<Terrain>,
    config: WalkerConfig,
    pos: (f32, f32, f32),
    /// The final target (teleport fallback names it).
    target: (f32, f32, f32),
    /// The terrain route's legs; `route_index` is the current one, the last one is `target`.
    route: Vec<(f32, f32, f32)>,
    route_index: usize,
    last_tick: Option<Duration>,
    next_move: Duration,
    blocked_since: Option<Duration>,
    /// Last time the distance to the target decreased, with that distance.
    progress: (Duration, f32),
    /// Last time the server's own position moved, with that position.
    server: Option<(Duration, (f32, f32, f32))>,
    server_stuck: bool,
    teleports: u32,
    fallbacks: Vec<WalkFallback>,
}

impl Walker {
    pub fn new(
        terrain: Arc<Terrain>,
        config: WalkerConfig,
        start: (f32, f32, f32),
        target: (f32, f32, f32),
    ) -> anyhow::Result<Self> {
        Self::with_route(terrain, config, start, vec![target])
    }

    /// A walker that follows `route` (the terrain planner's legs, or a `vec![target]` straight
    /// line). The last leg is the final target.
    pub fn with_route(
        terrain: Arc<Terrain>,
        config: WalkerConfig,
        start: (f32, f32, f32),
        route: Vec<(f32, f32, f32)>,
    ) -> anyhow::Result<Self> {
        if !(config.speed_mps > 0.0 && config.speed_mps <= MAX_ALLOWED_SPEED_MPS) {
            bail!(
                "walk speed {} is outside (0, {MAX_ALLOWED_SPEED_MPS}] m/s",
                config.speed_mps
            );
        }
        if config.interval < Duration::from_millis(50) {
            bail!("walk interval {:?} is below 50 ms", config.interval);
        }
        let Some(target) = route.last().copied() else {
            bail!("a walk route needs at least one waypoint");
        };
        let z = terrain.height(start.0, start.1).unwrap_or(start.2);
        let pos = (start.0, start.1, z);
        let dist = dist_2d(pos, route[0]);
        Ok(Self {
            terrain,
            config,
            pos,
            target,
            route,
            route_index: 0,
            last_tick: None,
            next_move: Duration::ZERO,
            blocked_since: None,
            progress: (Duration::ZERO, dist),
            server: None,
            server_stuck: false,
            teleports: 0,
            fallbacks: Vec::new(),
        })
    }

    /// The number of route legs the walker follows.
    pub fn route_len(&self) -> usize {
        self.route.len()
    }

    fn waypoint(&self) -> (f32, f32, f32) {
        self.route
            .get(self.route_index)
            .copied()
            .unwrap_or(self.target)
    }

    pub fn pos(&self) -> (f32, f32, f32) {
        self.pos
    }

    pub fn target(&self) -> (f32, f32, f32) {
        self.target
    }

    pub fn teleports(&self) -> u32 {
        self.teleports
    }

    pub fn fallbacks(&self) -> &[WalkFallback] {
        &self.fallbacks
    }

    /// The server's echo of our own position (from `SCOneUnitMovementPacket`). A position that
    /// stays within 0.5 m for `stuck_timeout` is a stuck server state.
    pub fn observe_server(&mut self, pos: (f32, f32, f32), now: Duration) {
        match self.server {
            None => self.server = Some((now, pos)),
            Some((since, anchor)) => {
                if dist_2d(anchor, pos) >= 0.5 {
                    self.server = Some((now, pos));
                } else if now.saturating_sub(since) >= self.config.stuck_timeout {
                    self.server_stuck = true;
                }
            }
        }
    }

    /// The driver teleported us (a counted fallback); restart the pacing anchors here and walk
    /// the remaining straight line to the final target.
    pub fn teleported(&mut self, pos: (f32, f32, f32), now: Duration) {
        let z = self.terrain.height(pos.0, pos.1).unwrap_or(pos.2);
        self.pos = (pos.0, pos.1, z);
        self.route = vec![self.target];
        self.route_index = 0;
        self.last_tick = Some(now);
        self.next_move = now;
        self.blocked_since = None;
        self.progress = (now, dist_2d(self.pos, self.target));
        self.server = None;
        self.server_stuck = false;
    }

    fn teleport(&mut self, reason: BlockReason) -> Step {
        self.teleports += 1;
        self.fallbacks.push(WalkFallback {
            kind: "teleport",
            target: format!(
                "{:.1},{:.1},{:.1}",
                self.target.0, self.target.1, self.target.2
            ),
            reason: reason.as_str().to_string(),
        });
        self.server_stuck = false;
        Step::Teleport {
            reason,
            at: self.target,
        }
    }

    /// One tick at `now` (time since the walk started).
    pub fn tick(&mut self, now: Duration) -> Step {
        // Consume every waypoint we already stand on; the last one is the final target.
        loop {
            let waypoint = self.waypoint();
            if dist_2d(self.pos, waypoint) > self.config.arrive_radius {
                break;
            }
            if self.route_index + 1 < self.route.len() {
                self.route_index += 1;
                let next = self.waypoint();
                self.progress = (now, dist_2d(self.pos, next));
                self.blocked_since = None;
            } else {
                return Step::Arrived {
                    distance: dist_2d(self.pos, self.target),
                };
            }
        }
        let waypoint = self.waypoint();
        let dist = dist_2d(self.pos, waypoint);
        if self.server_stuck {
            return self.teleport(BlockReason::PathBlocked);
        }
        if now < self.next_move {
            return Step::Wait {
                until: self.next_move,
            };
        }
        let dt = match self.last_tick {
            Some(prev) => now.saturating_sub(prev).as_secs_f32(),
            None => self.config.interval.as_secs_f32(),
        }
        .clamp(0.01, 0.25);
        self.last_tick = Some(now);
        self.next_move = now + self.config.interval;
        let step_len = (self.config.speed_mps * dt).min(dist);
        if step_len <= 0.0 {
            return Step::Wait {
                until: self.next_move,
            };
        }

        let (dir_x, dir_y) = (
            (waypoint.0 - self.pos.0) / dist,
            (waypoint.1 - self.pos.1) / dist,
        );
        let max_slope = self.config.max_slope_deg.to_radians().tan();
        let mut blocked = None;
        for angle_deg in [0.0f32, 20.0, -20.0, 40.0, -40.0] {
            let (dx, dy) = rotate(dir_x, dir_y, angle_deg);
            let cand_x = self.pos.0 + dx * step_len;
            let cand_y = self.pos.1 + dy * step_len;
            let z = match self.terrain.height(cand_x, cand_y) {
                Ok(z) => z,
                Err(_) => {
                    blocked = Some(BlockReason::PathBlocked);
                    continue;
                }
            };
            if self
                .config
                .water_level
                .is_some_and(|level| z <= level + 0.05)
            {
                blocked = Some(BlockReason::Water);
                continue;
            }
            let dz = z - self.pos.2;
            // Climbing is limited by the slope; a descent is a (short) fall, so only the
            // depth limit applies.
            if dz / step_len.max(0.01) > max_slope || dz < -self.config.max_drop_m {
                blocked = Some(BlockReason::Cliff);
                continue;
            }

            let old_z = self.pos.2;
            self.pos = (cand_x, cand_y, z);
            self.blocked_since = None;
            let new_dist = dist_2d(self.pos, waypoint);
            if new_dist + 0.25 < self.progress.1 {
                self.progress = (now, new_dist);
            }
            if now.saturating_sub(self.progress.0) >= self.config.stuck_timeout {
                return self.teleport(BlockReason::PathBlocked);
            }
            let angle = (dy as f64).atan2(dx as f64);
            let (vx, vy) =
                add_distance_to_front(self.config.speed_mps * 1000.0, 0.0, 0.0, angle as f32);
            let yaw = convert_radian_to_direction(angle - std::f64::consts::FRAC_PI_2);
            let drop = old_z - z;
            let fall_vel = (drop >= self.config.min_fall_drop_m)
                .then(|| fall_velocity(drop))
                .filter(|&v| v > 0);
            return Step::Move(MoveStep {
                pos: self.pos,
                velocity: (vx as i16, vy as i16, 0),
                yaw,
                fall_vel,
            });
        }

        // Every direction is blocked: count the time, then take the counted teleport.
        let reason = blocked.unwrap_or(BlockReason::PathBlocked);
        let since = *self.blocked_since.get_or_insert(now);
        if now.saturating_sub(since) >= self.config.stuck_timeout {
            return self.teleport(reason);
        }
        Step::Blocked { reason }
    }

    /// The driver's safety net after a [`Step::Teleport`]: too many fallbacks is an error.
    pub fn check_teleport_budget(&self) -> anyhow::Result<()> {
        if self.teleports > self.config.max_teleports {
            bail!("walk gave up after {} teleport fallback(s)", self.teleports);
        }
        Ok(())
    }
}

/// The client `fall_vel` for a drop of `height` metres (client units per m/s).
pub fn fall_velocity(height: f32) -> u16 {
    if height <= 0.0 {
        return 0;
    }
    let mps = (2.0 * FALL_GRAVITY * height).sqrt();
    (mps * FALL_UNITS_PER_MPS).min(f32::from(u16::MAX)) as u16
}

fn rotate(dx: f32, dy: f32, degrees: f32) -> (f32, f32) {
    let (sin, cos) = degrees.to_radians().sin_cos();
    (dx * cos - dy * sin, dx * sin + dy * cos)
}

pub fn dist_2d(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt()
}

/// The live `walk` subcommand settings.
pub struct WalkOptions {
    pub target: (f32, f32, f32),
    pub game_pak: PathBuf,
    pub app_path: PathBuf,
    /// Reads the race's `game_stances.max_speed` (else [`DEFAULT_RUN_SPEED_MPS`]).
    pub compact_db: Option<PathBuf>,
    pub speed_mps: Option<f32>,
    pub new_character: bool,
    pub race: u8,
    pub gender: u8,
    pub config: WalkerConfig,
    pub timeout: Duration,
    pub out: Option<PathBuf>,
    /// Print every refused step (debugging a detour).
    pub trace: bool,
}

/// What a live walk did (also written to `--out`).
#[derive(Debug, Clone, Serialize)]
pub struct WalkReport {
    pub character: String,
    pub from: [f32; 3],
    pub target: [f32; 3],
    pub arrived: bool,
    pub distance_m: f32,
    pub elapsed_s: f32,
    pub speed_mps: f32,
    /// The base run speed the server's own data reports (or the default).
    pub source_speed_mps: f32,
    pub teleports: u32,
    pub fallbacks: Vec<WalkFallback>,
    pub move_packets: u32,
    pub max_step_m: f32,
    /// Terrain-planner waypoints (1 = straight line).
    pub waypoints: usize,
    /// Length of the planned route.
    pub route_m: f32,
}

/// Login, select/spawn, then walk to `opts.target` with the terrain walker. A teleport fallback
/// is recorded in the report and the run continues from the landing point.
pub async fn walk(cfg: &BotConfig, opts: WalkOptions) -> anyhow::Result<WalkReport> {
    let terrain = Arc::new(Terrain::load(&opts.game_pak, &opts.app_path)?);
    let source_speed = opts
        .compact_db
        .as_deref()
        .and_then(|db| base_run_speed(db, opts.race).ok())
        .unwrap_or(DEFAULT_RUN_SPEED_MPS);
    let mut config = opts.config;
    if let Some(speed) = opts.speed_mps {
        config.speed_mps = speed;
    }
    // The world's ocean surface is the water gate the client's heightmap can decide alone;
    // lakes / rivers from `object.dat` are the path-planner step.
    if config.water_level.is_none() {
        config.water_level = Some(terrain.ocean_level);
    }
    let run_opts = RunOptions {
        all_ok: false,
        with_prereqs: false,
        new_character: opts.new_character,
        race: opts.race,
        gender: opts.gender,
        step_timeout: Duration::from_secs(60),
        compact_db: None,
        out: None,
        force_blocked: false,
        combat_skill: cfg.skill_id,
        player: None,
    };
    let rec = Recorder::new(true);
    let (mut game, character) = questrun::setup_world(cfg, &rec, &run_opts).await?;
    let from = game.state.self_pos;
    let route = terrain
        .route(from, opts.target, &config)
        .filter(|route| !route.is_empty())
        .unwrap_or_else(|| vec![opts.target]);
    let route_m: f32 = std::iter::once(from)
        .chain(route.iter().copied())
        .collect::<Vec<_>>()
        .windows(2)
        .map(|w| dist_2d(w[0], w[1]))
        .sum();
    println!(
        "[walk] terrain route: {} waypoint(s), {route_m:.1} m",
        route.len()
    );
    let waypoints = route.len();
    let mut walker = Walker::with_route(terrain, config, from, route)?;
    let started = Instant::now();
    let mut move_packets = 0u32;
    let mut max_step_m = 0.0f32;
    let mut arrived = false;

    loop {
        if !arrived {
            let now = started.elapsed();
            if now > opts.timeout {
                bail!(
                    "walk timed out after {:.1}s (still walking)",
                    now.as_secs_f32()
                );
            }
            let before = walker.pos();
            match walker.tick(now) {
                Step::Move(m) => {
                    send_move(&mut game, m, now.as_millis() as u32).await?;
                    move_packets += 1;
                    max_step_m = max_step_m.max(dist_2d(before, m.pos));
                    pump(&mut game, Duration::from_millis(10)).await?;
                    if let Some(unit) = game.state.units.get(&game.state.self_obj) {
                        walker.observe_server(unit.pos, started.elapsed());
                    }
                }
                Step::Wait { until } => {
                    let left = until.saturating_sub(started.elapsed());
                    pump(&mut game, left.min(Duration::from_millis(100))).await?;
                }
                Step::Blocked { reason } => {
                    if opts.trace {
                        println!(
                            "[walk] blocked {} at {:.1},{:.1},{:.2}",
                            reason.as_str(),
                            walker.pos().0,
                            walker.pos().1,
                            walker.pos().2
                        );
                    }
                    pump(&mut game, Duration::from_millis(25)).await?;
                }
                Step::Arrived { .. } => arrived = true,
                Step::Teleport { at, reason } => {
                    walker.check_teleport_budget()?;
                    game.teleport(near_point(walker.pos(), at)).await?;
                    walker.teleported(game.state.self_pos, started.elapsed());
                    println!(
                        "[walk] teleport fallback: {} -> {:.1},{:.1},{:.1}",
                        reason.as_str(),
                        at.0,
                        at.1,
                        at.2
                    );
                }
            }
        } else {
            break;
        }
    }

    let report = WalkReport {
        character: character.name,
        from: [from.0, from.1, from.2],
        target: [opts.target.0, opts.target.1, opts.target.2],
        arrived,
        distance_m: dist_2d(from, walker.pos()),
        elapsed_s: started.elapsed().as_secs_f32(),
        speed_mps: config.speed_mps,
        source_speed_mps: source_speed,
        teleports: walker.teleports(),
        fallbacks: walker.fallbacks().to_vec(),
        move_packets,
        max_step_m,
        waypoints,
        route_m,
    };
    if let Some(path) = &opts.out {
        std::fs::write(path, serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(report)
}

/// A point 1.5 m before `target` along the approach (the GM `/move` landing must not be inside
/// the NPC).
pub fn near_point(from: (f32, f32, f32), target: (f32, f32, f32)) -> (f32, f32, f32) {
    let (dx, dy) = (target.0 - from.0, target.1 - from.1);
    let dist = (dx * dx + dy * dy).sqrt();
    if dist <= 2.0 {
        return target;
    }
    let k = (dist - 1.5) / dist;
    (from.0 + dx * k, from.1 + dy * k, target.2)
}

pub async fn pump(game: &mut GameClient, wait: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        let _ = game
            .recv_within(left.min(Duration::from_millis(20)))
            .await?;
    }
}

/// `CSMoveUnitPacket` (`MoveTypeEnum.Unit`) with the walker's velocity / yaw / fall velocity.
pub async fn send_move(game: &mut GameClient, m: MoveStep, time: u32) -> anyhow::Result<()> {
    let mut s = PacketStream::new();
    let actor_flags = if m.fall_vel.is_some() { 0x82 } else { 0x02 };
    s.write_bc(game.state.self_obj)
        .write_u8(1) // MoveTypeEnum.Unit
        .write_u32(time)
        .write_u8(0x02) // MoveTypeFlags.Moving
        .write_position(m.pos.0, m.pos.1, m.pos.2)
        .write_i16(m.velocity.0)
        .write_i16(m.velocity.1)
        .write_i16(m.velocity.2)
        .write_i8(0)
        .write_i8(0)
        .write_i8(m.yaw)
        .write_i8(0)
        .write_i8(0)
        .write_i8(0) // delta movement
        .write_u8(1) // stance: relaxed
        .write_u8(0) // alertness
        .write_u8(actor_flags);
    if let Some(v) = m.fall_vel {
        s.write_u16(v);
    }
    game.send_cs(cs::CSMoveUnitPacket, s.as_bytes()).await?;
    game.state.self_pos = m.pos;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaemu_game::systems::npcs::heightmap::CELL_HMAP_RESOLUTION;

    const COEF: f64 = 65_535.0 / (4096.0 / 4.0);

    fn terrain_with(f: impl Fn(i32) -> f32) -> Arc<Terrain> {
        let map = WorldHeightmap::with_size(0, "test_world", 1, 1, COEF);
        let mut heights = vec![0u16; (CELL_HMAP_RESOLUTION * CELL_HMAP_RESOLUTION) as usize];
        for ix in 0..CELL_HMAP_RESOLUTION {
            let raw = (f64::from(f(ix)) * COEF) as u16;
            for iy in 0..CELL_HMAP_RESOLUTION {
                heights[(ix * CELL_HMAP_RESOLUTION + iy) as usize] = raw;
            }
        }
        assert!(map.insert_cell(0, 0, heights));
        Arc::new(Terrain::from_heightmap(map, 0.0))
    }

    fn terrain_2d(f: impl Fn(i32, i32) -> f32) -> Arc<Terrain> {
        let map = WorldHeightmap::with_size(0, "test_world", 1, 1, COEF);
        let mut heights = vec![0u16; (CELL_HMAP_RESOLUTION * CELL_HMAP_RESOLUTION) as usize];
        for ix in 0..CELL_HMAP_RESOLUTION {
            for iy in 0..CELL_HMAP_RESOLUTION {
                let raw = (f64::from(f(ix, iy)) * COEF) as u16;
                heights[(ix * CELL_HMAP_RESOLUTION + iy) as usize] = raw;
            }
        }
        assert!(map.insert_cell(0, 0, heights));
        Arc::new(Terrain::from_heightmap(map, 0.0))
    }

    fn flat(height: f32) -> Arc<Terrain> {
        terrain_with(|_| height)
    }

    #[test]
    fn paced_steps_use_the_run_speed() {
        let mut w = Walker::new(
            flat(10.0),
            WalkerConfig::default(),
            (0.0, 0.0, 10.0),
            (100.0, 0.0, 10.0),
        )
        .unwrap();
        let mut moved = 0.0f32;
        for i in 0..10 {
            let now = Duration::from_millis(100 * i);
            match w.tick(now) {
                Step::Move(m) => {
                    let expected = 0.54 * (i + 1) as f32;
                    assert!(
                        (m.pos.0 - expected).abs() < 1e-3,
                        "tick {i}: x {} != {expected}",
                        m.pos.0
                    );
                    assert!((m.pos.2 - 10.0).abs() < 0.05, "z {} != 10", m.pos.2);
                    assert!((f32::from(m.velocity.0) - 5400.0).abs() < 1.0);
                    assert_eq!(m.velocity.1, 0);
                    moved += 0.54;
                }
                other => panic!("tick {i}: {other:?}"),
            }
        }
        assert!((moved - 5.4).abs() < 1e-3);
        assert!((w.pos().0 - 5.4).abs() < 1e-3);
    }

    #[test]
    fn follows_the_heightmap() {
        let terrain = terrain_with(|ix| 10.0 + ix as f32 * 0.5);
        let mut w = Walker::new(
            terrain.clone(),
            WalkerConfig::default(),
            (0.0, 0.0, 0.0),
            (100.0, 0.0, 0.0),
        )
        .unwrap();
        let mut last_z = w.pos().2;
        for i in 0..10 {
            match w.tick(Duration::from_millis(100 * i)) {
                Step::Move(m) => {
                    let z = terrain.height(m.pos.0, m.pos.1).unwrap();
                    assert!((m.pos.2 - z).abs() < 1e-3, "{m:?} vs {z}");
                    assert!(m.pos.2 >= last_z);
                    last_z = m.pos.2;
                }
                other => panic!("tick {i}: {other:?}"),
            }
        }
        assert!(last_z > 11.0);
    }

    #[test]
    fn drops_carry_a_realistic_fall_velocity() {
        let terrain = terrain_with(|ix| 10.0 - ix as f32 * 1.6);
        let mut w = Walker::new(
            terrain,
            WalkerConfig::default(),
            (0.0, 0.0, 10.0),
            (100.0, 0.0, 0.0),
        )
        .unwrap();
        let Step::Move(m) = w.tick(Duration::ZERO) else {
            panic!("expected a move");
        };
        let v = m.fall_vel.expect("a downhill step carries fall_vel");
        assert!(v > 0 && v < 4000, "{v}");
    }

    #[test]
    fn speed_is_capped_at_25_mps() {
        let err = Walker::new(
            flat(0.0),
            WalkerConfig {
                speed_mps: 30.0,
                ..Default::default()
            },
            (0.0, 0.0, 0.0),
            (10.0, 0.0, 0.0),
        )
        .err()
        .expect("a 30 m/s walk must be rejected");
        assert!(err.to_string().contains("25"), "{err}");
    }

    #[test]
    fn water_blocks_and_teleports_counted() {
        let mut w = Walker::new(
            flat(5.0),
            WalkerConfig {
                water_level: Some(10.0),
                stuck_timeout: Duration::from_millis(500),
                ..Default::default()
            },
            (0.0, 0.0, 5.0),
            (100.0, 0.0, 5.0),
        )
        .unwrap();
        assert_eq!(
            w.tick(Duration::ZERO),
            Step::Blocked {
                reason: BlockReason::Water
            }
        );
        assert_eq!(
            w.tick(Duration::from_millis(600)),
            Step::Teleport {
                reason: BlockReason::Water,
                at: (100.0, 0.0, 5.0)
            }
        );
        assert_eq!(w.teleports(), 1);
        assert_eq!(w.fallbacks()[0].reason, "water");
    }

    #[test]
    fn a_cliff_blocks_and_teleports_counted() {
        let mut w = Walker::new(
            terrain_with(|ix| if ix >= 10 { 100.0 } else { 0.0 }),
            WalkerConfig {
                stuck_timeout: Duration::from_millis(300),
                ..Default::default()
            },
            (18.9, 1.0, 0.0),
            (40.0, 1.0, 0.0),
        )
        .unwrap();
        let mut blocked = None;
        for i in 0..10 {
            match w.tick(Duration::from_millis(100 * i)) {
                Step::Blocked { reason } => {
                    blocked = Some(reason);
                    break;
                }
                Step::Move(_) | Step::Wait { .. } => {}
                other => panic!("tick {i}: {other:?}"),
            }
        }
        assert_eq!(blocked, Some(BlockReason::Cliff));
        assert_eq!(
            w.tick(Duration::from_millis(1000)),
            Step::Teleport {
                reason: BlockReason::Cliff,
                at: (40.0, 1.0, 0.0)
            }
        );
        assert_eq!(w.teleports(), 1);
        assert_eq!(w.fallbacks()[0].reason, "cliff");
    }

    #[test]
    fn a_server_position_that_does_not_move_is_stuck() {
        let mut w = Walker::new(
            flat(10.0),
            WalkerConfig {
                stuck_timeout: Duration::from_millis(500),
                ..Default::default()
            },
            (0.0, 0.0, 10.0),
            (100.0, 0.0, 10.0),
        )
        .unwrap();
        let mut teleport = None;
        for i in 0..=10 {
            let now = Duration::from_millis(100 * i);
            w.observe_server((0.0, 0.0, 10.0), now);
            if let Step::Teleport { reason, .. } = w.tick(now) {
                teleport = Some(reason);
                break;
            }
        }
        assert_eq!(teleport, Some(BlockReason::PathBlocked));
        assert_eq!(w.teleports(), 1);
        assert_eq!(w.fallbacks()[0].reason, "path_blocked");
    }

    #[test]
    fn planner_routes_around_water() {
        // A water strip x 16..24, y 0..40; the direct line at y = 0 is blocked.
        let terrain = terrain_2d(|ix, iy| {
            if (8..12).contains(&ix) && (0..20).contains(&iy) {
                5.0
            } else {
                20.0
            }
        });
        let config = WalkerConfig {
            water_level: Some(10.0),
            ..Default::default()
        };
        let route = terrain
            .route((0.0, 0.0, 20.0), (40.0, 0.0, 20.0), &config)
            .expect("a land route around the water");
        assert!(route.len() > 1, "{route:?}");
        assert!(route.iter().all(|p| p.2 > 10.0), "{route:?}");
        assert!(
            route.iter().any(|p| p.1 > 20.0),
            "the route must go around the strip: {route:?}"
        );
        let last = route.last().unwrap();
        assert!((last.0 - 40.0).abs() < 1e-3 && (last.1).abs() < 1e-3);
    }

    #[test]
    fn planner_rejects_a_water_target() {
        let terrain = flat(5.0);
        let config = WalkerConfig {
            water_level: Some(10.0),
            ..Default::default()
        };
        assert!(terrain
            .route((0.0, 0.0, 5.0), (10.0, 0.0, 5.0), &config)
            .is_none());
    }

    #[test]
    fn fall_velocity_matches_the_client_curve() {
        // √(2·20·10) = 20 m/s -> 20 * 32000 / 63.25
        assert_eq!(fall_velocity(10.0), (20.0 * FALL_UNITS_PER_MPS) as u16);
        assert_eq!(fall_velocity(0.0), 0);
    }
}
