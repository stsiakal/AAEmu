//! NPC AI sweep (`aaemu-testclient ai`): reproduces the suspects of `D:\aa-re\zone\ai\AI-ORACLE.md`
//! against a live server and measures the 1.2 Lua / 10.x zone-server expectations from the
//! client's own packets. One JSONL line per NPC + test, classified `suspect`, `matches` or
//! `inconclusive`. GM commands are setup only (`/spawn`, `/move`, `/setpos`, `/damage`,
//! `/testai info`, `/godmode`); every one that ran is recorded in the line's `setup`.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aaemu_game::opcodes::sc;
use aaemu_game::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
use anyhow::{bail, ensure, Context};
use serde::Serialize;
use serde_json::json;

use crate::compactdb::{DEFAULT_COMPACT_DB, DEFAULT_COMPACT_DB_FROM_RUST};
use crate::fight;
use crate::game::{chat_text, GameClient, Packet};
use crate::record::Recorder;
use crate::scenario::BotConfig;

type P = (f32, f32, f32);

const PUMP: Duration = Duration::from_millis(200);
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
const TELEPORT_TIMEOUT: Duration = Duration::from_secs(8);

/// `aaemu-testclient ai` settings.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub tests: Vec<String>,
    pub all: bool,
    pub compact_db: Option<PathBuf>,
    /// `npc_spawns.json` of the world (natural spawn positions for NPCs without /spawn).
    pub spawns: Option<PathBuf>,
    pub out: Option<PathBuf>,
    pub summary: bool,
    pub new_character: bool,
    pub race: u8,
    pub gender: u8,
    pub arena: Option<P>,
    pub keep_npcs: bool,
}

/// One measured NPC + test line (one JSONL row).
#[derive(Debug, Clone, Serialize)]
pub struct AiLine {
    pub test: String,
    pub suspect: String,
    pub npc_template: u32,
    pub npc_name: String,
    pub npc_obj: u32,
    pub classification: String,
    pub expected_source: String,
    pub expected: serde_json::Value,
    pub measured: serde_json::Value,
    pub notes: Vec<String>,
    pub setup: Vec<String>,
    pub evidence: Vec<String>,
}

#[derive(Debug, Default)]
pub struct AiReport {
    pub lines: Vec<AiLine>,
}

impl AiReport {
    pub fn render_summary(&self) -> String {
        let mut out = String::new();
        let mut counts: HashMap<(String, String), u32> = HashMap::new();
        for l in &self.lines {
            *counts
                .entry((l.suspect.clone(), l.classification.clone()))
                .or_insert(0) += 1;
        }
        let mut keys: Vec<_> = counts.keys().cloned().collect();
        keys.sort();
        out.push_str(&format!("[ai] {} result line(s)\n", self.lines.len()));
        for (suspect, class) in keys {
            out.push_str(&format!(
                "[ai] {suspect} {class}: {}\n",
                counts[&(suspect.clone(), class.clone())]
            ));
        }
        for l in self.lines.iter().filter(|l| l.classification == "suspect") {
            out.push_str(&format!(
                "[ai]   SUSPECT {} npc {} ({}): {}\n",
                l.test,
                l.npc_template,
                l.npc_name,
                serde_json::to_string(&l.measured).unwrap_or_default()
            ));
        }
        let n = self
            .lines
            .iter()
            .filter(|l| l.classification == "inconclusive")
            .count();
        if n > 0 {
            out.push_str(&format!("[ai] {n} inconclusive line(s)\n"));
        }
        out
    }
}

// -------------------------------------------------------------------------------------------
// Observations
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Ev {
    UnitState {
        obj: u32,
        tpl: u32,
        pos: P,
    },
    Pos {
        obj: u32,
        pos: P,
    },
    Teleport {
        pos: P,
    },
    BotPos {
        pos: P,
    },
    Aggro {
        npc: u32,
        target: u32,
    },
    AggroCleared {
        npc: u32,
    },
    AiAggro {
        npc: u32,
        hostile: u32,
        count: i32,
    },
    TargetChanged {
        npc: u32,
        target: u32,
    },
    SkillStarted {
        skill: u32,
        caster: u32,
        target: u32,
    },
    Points {
        obj: u32,
        hp: i32,
    },
    Chat {
        text: String,
    },
}

#[derive(Debug, Clone)]
struct ObsEvent {
    t_ms: u64,
    kind: Ev,
}

struct Obs {
    start: Instant,
    events: Vec<ObsEvent>,
}

impl Obs {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            events: Vec::new(),
        }
    }
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
    fn mark(&self) -> usize {
        self.events.len()
    }
    fn push(&mut self, kind: Ev) {
        let t_ms = self.now_ms();
        self.events.push(ObsEvent { t_ms, kind });
    }
    fn since(&self, mark: usize) -> &[ObsEvent] {
        &self.events[mark.min(self.events.len())..]
    }
    fn observe(&mut self, p: &Packet) {
        if let Some(kind) = parse_event(p) {
            self.push(kind);
        }
    }
    fn chats_since(&self, mark: usize) -> Vec<String> {
        self.since(mark)
            .iter()
            .filter_map(|e| match &e.kind {
                Ev::Chat { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
    fn behavior_since(&self, mark: usize) -> Option<String> {
        self.chats_since(mark)
            .iter()
            .find_map(|t| parse_behavior(t))
    }
    fn hostile_onset(&self, npc: u32, bot: u32, from: u64, until: u64) -> Option<(u64, String)> {
        let mut first_target = None;
        for e in &self.events {
            if e.t_ms < from || e.t_ms > until {
                continue;
            }
            let what = match &e.kind {
                Ev::Aggro { npc: n, target } if *n == npc && *target == bot => {
                    "aggro_target_changed"
                }
                Ev::AiAggro {
                    npc: n,
                    hostile,
                    count,
                } if *n == npc && *hostile == bot && *count > 0 => "ai_aggro",
                Ev::SkillStarted { caster, target, .. } if *caster == npc && *target == bot => {
                    "skill_started"
                }
                Ev::TargetChanged { npc: n, target } if *n == npc && *target == bot => {
                    // The alert sends one target change; the attack behavior re-sets the target
                    // every tick, so a repeated change is the combat transition.
                    match first_target {
                        None => {
                            first_target = Some(e.t_ms);
                            continue;
                        }
                        Some(t0) if (1200..=5000).contains(&e.t_ms.saturating_sub(t0)) => {
                            "retarget"
                        }
                        Some(_) => continue,
                    }
                }
                _ => continue,
            };
            return Some((e.t_ms, what.to_string()));
        }
        None
    }

    fn alert_onset(&self, npc: u32, bot: u32, from: u64, until: u64) -> Option<u64> {
        self.events
            .iter()
            .find(|e| {
                e.t_ms >= from
                    && e.t_ms <= until
                    && matches!(&e.kind, Ev::TargetChanged { npc: n, target } if *n == npc && *target == bot)
            })
            .map(|e| e.t_ms)
    }
    fn alert_cleared(&self, npc: u32, from: u64, until: u64) -> Option<u64> {
        self.events
            .iter()
            .find(|e| {
                e.t_ms >= from
                    && e.t_ms <= until
                    && (matches!(&e.kind, Ev::TargetChanged { npc: n, target } if *n == npc && *target == 0)
                        || matches!(&e.kind, Ev::AggroCleared { npc: n } if *n == npc))
            })
            .map(|e| e.t_ms)
    }
    fn skill_starts(&self, npc: u32, from: u64, until: u64) -> Vec<(u64, u32)> {
        self.events
            .iter()
            .filter_map(|e| match &e.kind {
                Ev::SkillStarted { skill, caster, .. }
                    if *caster == npc && e.t_ms >= from && e.t_ms <= until =>
                {
                    Some((e.t_ms, *skill))
                }
                _ => None,
            })
            .collect()
    }
    fn positions(&self, obj: u32, from: u64, until: u64) -> Vec<(u64, P)> {
        self.events
            .iter()
            .filter_map(|e| {
                if e.t_ms < from || e.t_ms > until {
                    return None;
                }
                match &e.kind {
                    Ev::Pos { obj: o, pos } | Ev::UnitState { obj: o, pos, .. } if *o == obj => {
                        Some((e.t_ms, *pos))
                    }
                    _ => None,
                }
            })
            .collect()
    }
    fn last_position(&self, obj: u32, until: u64) -> Option<P> {
        self.events.iter().rev().find_map(|e| match &e.kind {
            Ev::Pos { obj: o, pos } | Ev::UnitState { obj: o, pos, .. }
                if *o == obj && e.t_ms <= until =>
            {
                Some(*pos)
            }
            _ => None,
        })
    }
    fn hp_at(&self, obj: u32, until: u64) -> Option<i32> {
        self.events.iter().rev().find_map(|e| match &e.kind {
            Ev::Points { obj: o, hp } if *o == obj && e.t_ms <= until => Some(*hp),
            _ => None,
        })
    }
    fn bot_pos_at(&self, until: u64) -> Option<P> {
        self.events.iter().rev().find_map(|e| match &e.kind {
            Ev::BotPos { pos } if e.t_ms <= until => Some(*pos),
            _ => None,
        })
    }
}

fn parse_event(p: &Packet) -> Option<Ev> {
    if p.is_sc(sc::SCAggroTargetChanged) {
        let mut s = p.stream();
        let npc = s.read_bc().ok()?;
        let target = s.read_bc().ok()?;
        return Some(if target == 0 {
            Ev::AggroCleared { npc }
        } else {
            Ev::Aggro { npc, target }
        });
    }
    if p.is_sc(sc::SCAiAggroPacket) {
        let mut s = p.stream();
        let npc = s.read_bc().ok()?;
        let count = s.read_i32().ok()?;
        let mut hostile = 0;
        if count > 0 {
            hostile = s.read_bc().ok()?;
            s.read_i32().ok()?;
            s.read_i32().ok()?;
            s.read_i32().ok()?;
            s.read_u8().ok()?;
        }
        return Some(Ev::AiAggro {
            npc,
            hostile,
            count,
        });
    }
    if p.is_sc(sc::SCTargetChangedPacket) {
        let mut s = p.stream();
        let npc = s.read_bc().ok()?;
        let target = s.read_bc().ok()?;
        return Some(Ev::TargetChanged { npc, target });
    }
    if p.is_sc(sc::SCSkillStartedPacket) {
        let (skill, caster, target) = parse_skill_started(p)?;
        return Some(Ev::SkillStarted {
            skill,
            caster,
            target,
        });
    }
    if p.is_sc(sc::SCUnitPointsPacket) {
        let mut s = p.stream();
        let obj = s.read_bc().ok()?;
        let hp = s.read_i32().ok()? / 100;
        return Some(Ev::Points { obj, hp });
    }
    if p.is_sc(sc::SCUnitStatePacket) {
        let mut s = p.stream();
        let obj = s.read_bc().ok()?;
        s.read_string().ok()?;
        let unit_type = s.read_u8().ok()?;
        let tpl = match unit_type {
            0 => {
                let id = s.read_u32().ok()?;
                s.read_u64().ok()?;
                id
            }
            1 => {
                s.read_bc().ok()?;
                let tpl = s.read_u32().ok()?;
                s.read_u32().ok()?;
                s.read_u8().ok()?;
                tpl
            }
            5 => {
                s.read_u16().ok()?;
                let tpl = s.read_u32().ok()?;
                s.read_u32().ok()?;
                tpl
            }
            _ => return None,
        };
        s.read_string().ok()?;
        let pos = s.read_position().ok()?;
        s.read_f32().ok()?;
        s.read_u8().ok()?;
        return Some(Ev::UnitState { obj, tpl, pos });
    }
    if p.is_sc(sc::SCOneUnitMovementPacket) {
        let mut s = p.stream();
        let obj = s.read_bc().ok()?;
        if s.read_u8().ok()? != 1 {
            return None;
        }
        s.read_u32().ok()?;
        let flags = s.read_u8().ok()?;
        if flags & 0x10 != 0 {
            s.read_u32().ok()?;
            s.read_u8().ok()?;
        }
        let pos = s.read_position().ok()?;
        return Some(Ev::Pos { obj, pos });
    }
    if p.is_sc(sc::SCTeleportUnitPacket) {
        let mut s = p.stream();
        s.read_u8().ok()?;
        s.read_i16().ok()?;
        let pos = s.read_position().ok()?;
        s.read_f32().ok()?;
        return Some(Ev::Teleport { pos });
    }
    if p.is_sc(sc::SCChatMessagePacket) {
        return chat_text(p).map(|text| Ev::Chat { text });
    }
    None
}

fn read_cast_target(s: &mut aaemu_commons::PacketStream) -> Option<u32> {
    let kind = s.read_u8().ok()?;
    Some(match SkillCastTarget::read(kind, s).ok()? {
        SkillCastTarget::Unit(id) | SkillCastTarget::Doodad(id) => id,
        SkillCastTarget::Position { obj_id, .. } | SkillCastTarget::Position2 { obj_id, .. } => {
            obj_id
        }
        SkillCastTarget::Item { obj_id, .. } => obj_id,
        _ => 0,
    })
}

fn parse_skill_started(p: &Packet) -> Option<(u32, u32, u32)> {
    let mut s = p.stream();
    let skill = s.read_u32().ok()?;
    s.read_u16().ok()?;
    let kind = s.read_u8().ok()?;
    let caster = SkillCaster::read(kind, &mut s).ok()?.obj_id();
    let target = read_cast_target(&mut s)?;
    let object = s.read_u8().ok()?;
    SkillObject::read(object, &mut s).ok()?;
    s.read_u16().ok()?;
    s.read_u16().ok()?;
    s.read_u8().ok()?;
    let flags = s.read_u8().ok()?;
    if flags & 1 != 0 {
        s.read_u8().ok()?;
    }
    Some((skill, caster, target))
}

fn parse_behavior(text: &str) -> Option<String> {
    let rest = text.split("CurrentBehavior:").nth(1)?.trim_start();
    let name = rest.split([',', ' ', '\r', '\n']).next()?.trim();
    let name = name.rsplit('.').next().unwrap_or(name);
    Some(name.trim_end_matches("Behavior").to_string())
}

// -------------------------------------------------------------------------------------------
// Measurement helpers (unit tested)
// -------------------------------------------------------------------------------------------

fn dist(a: P, b: P) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
}

fn direction(from: P, to: P) -> (f32, f32) {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-3 {
        (0.0, 1.0)
    } else {
        (dx / len, dy / len)
    }
}

fn classify_distance(measured: f32, expected: f32, tol: f32) -> &'static str {
    if (measured - expected).abs() <= tol {
        "matches"
    } else {
        "suspect"
    }
}

fn threshold(samples: &[(f32, bool)]) -> Option<f32> {
    samples
        .iter()
        .filter(|(_, hit)| *hit)
        .map(|(d, _)| *d)
        .fold(None, |acc, d| Some(acc.map_or(d, |a: f32| a.max(d))))
}

fn median(mut values: Vec<f32>) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f32::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    })
}

fn biggest_jump(samples: &[(u64, P)]) -> f32 {
    samples
        .windows(2)
        .map(|w| dist(w[0].1, w[1].1))
        .fold(0.0, f32::max)
}

/// Cosine between the movement `d` and the direction away from `threat`.
fn away_dot(d: P, start: P, threat: P) -> f32 {
    let away = direction(threat, start);
    let len = (d.0 * d.0 + d.1 * d.1).sqrt();
    if len < 1e-3 {
        0.0
    } else {
        (d.0 * away.0 + d.1 * away.1) / len
    }
}

/// Cosine between the movement `d` and the direction towards `home`.
fn toward_dot(d: P, start: P, home: P) -> f32 {
    let toward = direction(start, home);
    let len = (d.0 * d.0 + d.1 * d.1).sqrt();
    if len < 1e-3 {
        0.0
    } else {
        (d.0 * toward.0 + d.1 * toward.1) / len
    }
}
// -------------------------------------------------------------------------------------------
// Compact database lookups
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct NpcRow {
    name: String,
    level: u8,
    ai_file_id: u32,
    npc_ai_param_id: u32,
    aggression: bool,
    attack_scale: f32,
    sight_scale: f32,
    return_distance: f32,
    absolute_return_distance: f32,
    npc_grade_id: u32,
}

impl NpcRow {
    fn json(&self, param: Option<&str>) -> serde_json::Value {
        json!({
            "level": self.level,
            "ai_file_id": self.ai_file_id,
            "npc_ai_param_id": self.npc_ai_param_id,
            "aggression": self.aggression,
            "attack_start_range_scale": self.attack_scale,
            "sight_range_scale": self.sight_scale,
            "return_distance": self.return_distance,
            "absolute_return_distance": self.absolute_return_distance,
            "npc_grade_id": self.npc_grade_id,
            "ai_param": param,
        })
    }
}

#[derive(Debug, Clone)]
struct SkillRow {
    name: String,
    min_range: f32,
    max_range: f32,
    plot_id: u32,
}

struct AiDb {
    conn: rusqlite::Connection,
    path: PathBuf,
}

impl AiDb {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("opening compact database {}", path.display()))?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    fn open_default(compact_db: Option<&Path>) -> anyhow::Result<Self> {
        if let Some(path) = compact_db {
            if !path.exists() {
                bail!("compact database {} not found", path.display());
            }
            return Self::open(path);
        }
        for candidate in [DEFAULT_COMPACT_DB, DEFAULT_COMPACT_DB_FROM_RUST] {
            let path = Path::new(candidate);
            if path.exists() {
                return Self::open(path);
            }
        }
        bail!("no compact database found; pass --compact-db")
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn npc(&self, id: u32) -> anyhow::Result<Option<NpcRow>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT name, LEVEL, COALESCE(ai_file_id,0), COALESCE(npc_ai_param_id,0),
                 COALESCE(aggression,'f'), COALESCE(attack_start_range_scale,0),
                 COALESCE(sight_range_scale,0), COALESCE(return_distance,0),
                 COALESCE(absolute_return_distance,0), COALESCE(npc_grade_id,0)
                 FROM npcs WHERE id = ?1",
                [id],
                |r| {
                    Ok(NpcRow {
                        name: r.get(0)?,
                        level: r.get::<_, i64>(1)? as u8,
                        ai_file_id: r.get::<_, i64>(2)? as u32,
                        npc_ai_param_id: r.get::<_, i64>(3)? as u32,
                        aggression: !r.get::<_, String>(4)?.eq_ignore_ascii_case("f"),
                        attack_scale: r.get::<_, f64>(5)? as f32,
                        sight_scale: r.get::<_, f64>(6)? as f32,
                        return_distance: r.get::<_, f64>(7)? as f32,
                        absolute_return_distance: r.get::<_, f64>(8)? as f32,
                        npc_grade_id: r.get::<_, i64>(9)? as u32,
                    })
                },
            )
            .optional()?)
    }

    fn param(&self, id: u32) -> anyhow::Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT ai_param FROM npc_ai_params WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn skill(&self, id: u32) -> anyhow::Result<Option<SkillRow>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT name, COALESCE(min_range,0), COALESCE(max_range,0), COALESCE(plot_id,0)
                 FROM skills WHERE id = ?1",
                [id],
                |r| {
                    Ok(SkillRow {
                        name: r.get(0)?,
                        min_range: r.get::<_, i64>(1)? as f32,
                        max_range: r.get::<_, i64>(2)? as f32,
                        plot_id: r.get::<_, i64>(3)? as u32,
                    })
                },
            )
            .optional()?)
    }

    fn skills_of_condition(&self, npc: u32, condition: i64) -> anyhow::Result<Vec<u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT skill_id FROM np_skills WHERE owner_id = ?1 AND skill_use_condition_id = ?2
             ORDER BY skill_id",
        )?;
        let rows = stmt.query_map([npc as i64, condition], |r| r.get::<_, i64>(0))?;
        Ok(rows.filter_map(Result::ok).map(|v| v as u32).collect())
    }

    fn count_burst_candidates(&self) -> anyhow::Result<u32> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM np_skills ns JOIN skills s ON s.id = ns.skill_id
             WHERE ns.skill_use_condition_id = 0 AND s.ignore_global_cooldown = 1
             AND COALESCE(s.plot_id,0) = 0",
            [],
            |r| r.get::<_, i64>(0),
        )? as u32)
    }

    fn count_cond0(&self) -> anyhow::Result<u32> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM np_skills WHERE skill_use_condition_id = 0",
            [],
            |r| r.get::<_, i64>(0),
        )? as u32)
    }
}

/// Natural spawn positions from `Worlds/main_world/npc_spawns.json` (JSONC: `//` comments and
/// trailing commas). The file is `[{"UnitId": N, "Position": {"X": .., "Y": .., "Z": ..}}, ..]`.
fn load_spawn_points(path: &Path) -> anyhow::Result<HashMap<u32, Vec<P>>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading spawn points {}", path.display()))?;
    Ok(parse_spawn_points(&text))
}

fn parse_spawn_points(text: &str) -> HashMap<u32, Vec<P>> {
    let mut out: HashMap<u32, Vec<P>> = HashMap::new();
    let mut unit: Option<u32> = None;
    let mut xyz: [Option<f32>; 3] = [None; 3];
    let flush =
        |unit: &mut Option<u32>, xyz: &mut [Option<f32>; 3], out: &mut HashMap<u32, Vec<P>>| {
            if let Some(u) = unit.take() {
                if let (Some(x), Some(y)) = (xyz[0].take(), xyz[1].take()) {
                    let z = xyz[2].take().unwrap_or(0.0);
                    out.entry(u).or_default().push((x, y, z));
                } else {
                    xyz[2] = None;
                }
            }
            *xyz = [None; 3];
        };
    for line in text.lines() {
        let line = line.trim();
        if let Some(at) = line.find("\"UnitId\":") {
            flush(&mut unit, &mut xyz, &mut out);
            unit = line[at + "\"UnitId\":".len()..]
                .split(|c: char| !c.is_ascii_digit())
                .find(|s| !s.is_empty())
                .and_then(|s| s.parse().ok());
        }
        if let Some(at) = line.find("\"X\":") {
            xyz[0] = parse_json_number(&line[at + "\"X\":".len()..]);
        }
        if let Some(at) = line.find("\"Y\":") {
            xyz[1] = parse_json_number(&line[at + "\"Y\":".len()..]);
        }
        if let Some(at) = line.find("\"Z\":") {
            xyz[2] = parse_json_number(&line[at + "\"Z\":".len()..]);
            flush(&mut unit, &mut xyz, &mut out);
        }
    }
    flush(&mut unit, &mut xyz, &mut out);
    out
}

fn parse_json_number(rest: &str) -> Option<f32> {
    rest.split("//")
        .next()?
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'))
        .collect::<String>()
        .parse()
        .ok()
}

/// `--spawns`, `$AAEMU_NPC_SPAWNS`, next to the compact database, or the repository app data.
fn default_spawns_path(db: &AiDb) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("AAEMU_NPC_SPAWNS") {
        return Some(PathBuf::from(p));
    }
    let candidates = [
        db.path()
            .parent()
            .map(|d| d.join("Worlds/main_world/npc_spawns.json")),
        Some(PathBuf::from(
            "crates/aaemu-game/app/Data/Worlds/main_world/npc_spawns.json",
        )),
        Some(PathBuf::from(
            "rust/crates/aaemu-game/app/Data/Worlds/main_world/npc_spawns.json",
        )),
    ];
    candidates.into_iter().flatten().find(|p| p.exists())
}

fn param_number(param: &str, key: &str) -> Option<f32> {
    let at = param.find(key)?;
    let after = param[at + key.len()..].trim_start().strip_prefix('=')?;
    let num: String = after
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    num.parse().ok()
}

fn first_int_after(param: &str, anchor: &str, key: &str) -> Option<u32> {
    let at = param.find(anchor)?;
    let rest = &param[at..];
    let at = rest.find(key)?;
    let after = rest[at + key.len()..].trim_start().strip_prefix('=')?;
    let num: String = after
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    num.parse().ok()
}

fn spurt_skill(param: &str) -> Option<u32> {
    first_int_after(param, "onSpurtSkill", "skillType")
}

fn spurt_health_condition(param: &str) -> Option<f32> {
    param_number(param, "healthCondition")
}

fn preferred_combat_dist(param: &str) -> Option<f32> {
    param_number(param, "preferedCombatDist")
}

// -------------------------------------------------------------------------------------------
// Live sweep
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Spawned {
    obj: u32,
    pos: P,
    /// NPC spawn -> where the bot stood (the NPC spawned facing that way).
    facing: (f32, f32),
    /// A natural world spawn (never despawned by the sweep) instead of a GM `/spawn`.
    natural: bool,
    /// The engagement was forced with a `/damage 1` probe instead of proximity aggro.
    damage_fallback: bool,
}

struct Sweep {
    game: GameClient,
    obs: Obs,
    db: AiDb,
    lines: Vec<AiLine>,
    gm_used: Vec<String>,
    spawned: Vec<u32>,
    spawn_points: HashMap<u32, Vec<P>>,
}

impl Sweep {
    fn bot(&self) -> u32 {
        self.game.state.self_obj
    }

    fn npc_pos(&self, obj: u32) -> Option<P> {
        self.game.state.units.get(&obj).map(|u| u.pos)
    }

    async fn pump_for(&mut self, wait: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            if let Some(p) = self.game.recv_within(left.min(PUMP)).await? {
                self.obs.observe(&p);
            }
        }
    }

    async fn wait_until(
        &mut self,
        timeout: Duration,
        mut pred: impl FnMut(&Sweep) -> bool,
    ) -> anyhow::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            if pred(self) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            self.pump_for(PUMP).await?;
        }
    }

    async fn gm(&mut self, command: &str, wait: Duration) -> anyhow::Result<Vec<String>> {
        let text = if command.starts_with('/') {
            command.to_string()
        } else {
            format!("/{command}")
        };
        let mark = self.obs.mark();
        self.game.chat(0, &text).await?;
        self.gm_used.push(text);
        self.pump_for(wait).await?;
        Ok(self.obs.chats_since(mark))
    }

    async fn teleport(&mut self, pos: P) -> anyhow::Result<()> {
        let command = format!("/move {} {} {}", pos.0, pos.1, pos.2);
        let mark = self.obs.mark();
        self.game.chat(0, &command).await?;
        self.gm_used.push(command);
        let landed = self
            .wait_until(TELEPORT_TIMEOUT, |s| {
                s.obs
                    .since(mark)
                    .iter()
                    .any(|e| matches!(&e.kind, Ev::Teleport { pos: p } if dist(*p, pos) < 1.0))
            })
            .await?;
        ensure!(landed, "no teleport confirmation to {pos:?}");
        self.game.teleport_ended(pos).await?;
        self.game.move_to(pos, 500).await?;
        self.obs.push(Ev::BotPos { pos });
        self.pump_for(Duration::from_millis(250)).await
    }

    async fn place_bot(&mut self, npc: P, dir: (f32, f32), range: f32) -> anyhow::Result<()> {
        self.teleport((npc.0 + dir.0 * range, npc.1 + dir.1 * range, npc.2))
            .await
    }

    async fn fresh_spawn(&mut self, tpl: u32) -> anyhow::Result<Spawned> {
        let before: HashSet<u32> = self
            .game
            .state
            .units
            .values()
            .filter(|u| u.unit_type == 1 && u.id == tpl)
            .map(|u| u.obj_id)
            .collect();
        let bot_pos = self.game.state.self_pos;
        let mark = self.obs.mark();
        self.gm(&format!("spawn npc {tpl}"), Duration::from_millis(400))
            .await?;
        self.wait_until(SPAWN_TIMEOUT, |s| {
            s.obs.since(mark).iter().any(|e| {
                matches!(&e.kind, Ev::UnitState { obj, tpl: t, .. } if *t == tpl && !before.contains(obj))
            })
        })
        .await?;
        let Some(obj) = self.obs.since(mark).iter().find_map(|e| match &e.kind {
            Ev::UnitState { obj, tpl: t, .. } if *t == tpl && !before.contains(obj) => Some(*obj),
            _ => None,
        }) else {
            bail!("NPC {tpl} did not appear after /spawn npc {tpl}");
        };
        self.spawned.push(obj);
        let pos = self
            .npc_pos(obj)
            .with_context(|| format!("NPC {tpl} obj {obj} not in unit state"))?;
        Ok(Spawned {
            obj,
            pos,
            facing: direction(pos, bot_pos),
            natural: false,
            damage_fallback: false,
        })
    }

    /// Finds a naturally spawned NPC of `tpl`: first the units the login already announced,
    /// then up to four world spawn points starting at `occurrence`, then `/spawn npc` for
    /// templates the world data does not place.
    async fn locate_npc(&mut self, tpl: u32, occurrence: usize) -> anyhow::Result<Spawned> {
        let points = self.spawn_points.get(&tpl).cloned().unwrap_or_default();
        if points.is_empty() {
            return self.fresh_spawn(tpl).await;
        }
        let known = |s: &Self, point: P| {
            s.game
                .state
                .units
                .values()
                .find(|u| u.unit_type == 1 && u.id == tpl && dist(u.pos, point) < 80.0)
                .map(|u| (u.obj_id, u.pos))
        };
        let len = points.len();
        let attempts = len.min(4);
        let wait = if len == 1 {
            Duration::from_secs(30)
        } else {
            Duration::from_secs(15)
        };
        for step in 0..attempts {
            let point = points[(occurrence + step) % len];
            // Units announced while the sweep was still setting up live in `GameState` only.
            if let Some((obj, pos)) = known(self, point) {
                return Ok(Spawned {
                    obj,
                    pos,
                    facing: direction(pos, self.game.state.self_pos),
                    natural: true,
                    damage_fallback: false,
                });
            }
            // Two region activations (spawners tick around players), then the point itself.
            self.teleport((point.0 + 40.0, point.1, point.2)).await?;
            self.pump_for(Duration::from_millis(600)).await?;
            self.teleport((point.0 + 6.0, point.1, point.2)).await?;
            let mark = self.obs.mark();
            let found = self
                .wait_until(wait, |s| {
                    s.obs.since(mark).iter().rev().any(|e| {
                        matches!(&e.kind, Ev::UnitState { tpl: t, pos, .. } if *t == tpl && dist(*pos, point) < 80.0)
                    }) || known(s, point).is_some()
                })
                .await?;
            let found_unit = self
                .obs
                .since(mark)
                .iter()
                .rev()
                .find_map(|e| match &e.kind {
                    Ev::UnitState { obj, tpl: t, pos } if *t == tpl && dist(*pos, point) < 80.0 => {
                        Some((*obj, *pos))
                    }
                    _ => None,
                })
                .or_else(|| known(self, point));
            if found || found_unit.is_some() {
                let Some((obj, pos)) = found_unit else {
                    unreachable!("found implies a unit");
                };
                return Ok(Spawned {
                    obj,
                    pos,
                    facing: direction(pos, self.game.state.self_pos),
                    natural: true,
                    damage_fallback: false,
                });
            }
        }
        bail!("no natural spawn of {tpl} found near any of its {attempts} tried spawn point(s)")
    }

    /// Removes a GM-spawned NPC; natural world spawns are left in place.
    async fn release(&mut self, spawned: Spawned) {
        if !spawned.natural {
            self.despawn_npc(spawned.obj).await;
        }
    }

    /// Places the bot `distance` from the NPC and tries four bearings until the NPC reacts
    /// (alert target change or a hostile action) within `settle`. Returns the placement, the
    /// reaction time and its kind.
    async fn place_until_seen(
        &mut self,
        obj: u32,
        npc: P,
        distance: f32,
        settle: Duration,
    ) -> anyhow::Result<Option<(P, u64, String)>> {
        let bot = self.bot();
        for bearing in [0.0f32, 90.0, 180.0, 270.0] {
            let angle = bearing.to_radians();
            let pos = (
                npc.0 + angle.cos() * distance,
                npc.1 + angle.sin() * distance,
                npc.2,
            );
            self.teleport(pos).await?;
            let t0 = self.obs.now_ms();
            self.pump_for(settle).await?;
            let now = self.obs.now_ms();
            if let Some((t, kind)) = self.obs.hostile_onset(obj, bot, t0, now) {
                return Ok(Some((pos, t, kind)));
            }
            if let Some(t) = self.obs.alert_onset(obj, bot, t0, now) {
                return Ok(Some((pos, t, "alert".into())));
            }
        }
        Ok(None)
    }

    async fn despawn_npc(&mut self, obj: u32) {
        let _ = self
            .gm(&format!("despawn npc {obj}"), Duration::from_millis(300))
            .await;
    }

    /// One real auto attack (skill 2) from 2.5 m, facing the target. GM `/damage` changes HP
    /// without an aggro entry, so this is what makes an NPC engage.
    async fn auto_attack(&mut self, obj: u32) -> anyhow::Result<()> {
        let Some(np) = self.npc_pos(obj) else {
            return Ok(());
        };
        let dir = direction(np, self.game.state.self_pos);
        let stand = (np.0 + dir.0 * 2.5, np.1 + dir.1 * 2.5, np.2);
        self.teleport(stand).await?;
        self.game.change_target(obj).await?;
        self.pump_for(Duration::from_millis(150)).await?;
        self.game.move_to_facing(stand, np, 600).await?;
        self.pump_for(Duration::from_millis(150)).await?;
        for _ in 0..3 {
            self.game.start_skill(2, obj).await?;
            self.pump_for(Duration::from_millis(1300)).await?;
        }
        Ok(())
    }

    async fn damage(&mut self, obj: u32, amount: &str) -> anyhow::Result<()> {
        self.game.change_target(obj).await?;
        self.pump_for(Duration::from_millis(150)).await?;
        self.gm(&format!("damage {amount}"), Duration::from_millis(350))
            .await?;
        Ok(())
    }

    async fn setpos(&mut self, obj: u32, pos: P) -> anyhow::Result<()> {
        self.gm(
            &format!("setpos {obj} {} {} {}", pos.0, pos.1, pos.2),
            Duration::from_millis(400),
        )
        .await?;
        Ok(())
    }

    async fn poll_behavior(&mut self, npc: u32) -> anyhow::Result<Option<String>> {
        self.game.change_target(npc).await?;
        self.pump_for(Duration::from_millis(150)).await?;
        let mark = self.obs.mark();
        self.gm("testai info", Duration::from_millis(450)).await?;
        Ok(self.obs.behavior_since(mark))
    }

    fn line(&mut self, line: AiLine) {
        println!(
            "[ai] {:<28} npc {:<6} {:<12} {}",
            line.test, line.npc_template, line.classification, line.npc_name
        );
        self.lines.push(line);
    }

    fn inconclusive(&mut self, test: &str, suspect: &str, tpl: u32, reason: impl Into<String>) {
        let name = self
            .db
            .npc(tpl)
            .ok()
            .flatten()
            .map_or_else(|| format!("npc {tpl}"), |n| n.name);
        self.line(AiLine {
            test: test.to_string(),
            suspect: suspect.to_string(),
            npc_template: tpl,
            npc_name: name,
            npc_obj: 0,
            classification: "inconclusive".into(),
            expected_source: "measurement".into(),
            expected: json!({}),
            measured: json!({}),
            notes: vec![reason.into()],
            setup: self.gm_used.clone(),
            evidence: Vec::new(),
        });
    }

    async fn engage(
        &mut self,
        tpl: u32,
        occurrence: usize,
        distance: f32,
        test: &str,
        suspect: &str,
    ) -> Option<Spawned> {
        let spawned = match self.locate_npc(tpl, occurrence).await {
            Ok(s) => s,
            Err(e) => {
                self.inconclusive(test, suspect, tpl, format!("spawn failed: {e:#}"));
                return None;
            }
        };
        let placed = match self
            .place_until_seen(
                spawned.obj,
                spawned.pos,
                distance,
                Duration::from_millis(4000),
            )
            .await
        {
            Ok(Some((pos, _t, _kind))) => pos,
            Ok(None) => {
                // Proximity aggro did not fire (a measured finding in itself): force the
                // engagement with one real auto attack so the rest of the test can run.
                let t0 = self.obs.now_ms();
                let pos = if let Err(e) = self.auto_attack(spawned.obj).await {
                    self.inconclusive(test, suspect, tpl, format!("attack failed: {e:#}"));
                    self.release(spawned).await;
                    return None;
                } else {
                    self.game.state.self_pos
                };
                let bot = self.bot();
                let ok = self
                    .wait_until(Duration::from_secs(4), |s| {
                        s.obs
                            .hostile_onset(spawned.obj, bot, t0, s.obs.now_ms())
                            .is_some()
                    })
                    .await
                    .unwrap_or(false);
                if !ok {
                    let hp0 = self.obs.hp_at(spawned.obj, t0);
                    let hp1 = self.obs.hp_at(spawned.obj, self.obs.now_ms());
                    let own_casts = self.obs.skill_starts(self.bot(), t0, self.obs.now_ms());
                    let behavior = self.poll_behavior(spawned.obj).await.ok().flatten();
                    self.inconclusive(
                        test,
                        suspect,
                        tpl,
                        format!(
                            "NPC {tpl} did not react to proximity or an auto attack (hp {hp0:?}->                             {hp1:?}, own casts {own_casts:?}, behavior {behavior:?})"
                        ),
                    );
                    self.release(spawned).await;
                    return None;
                }
                return Some(Spawned {
                    facing: direction(spawned.pos, pos),
                    damage_fallback: true,
                    ..spawned
                });
            }
            Err(e) => {
                self.inconclusive(test, suspect, tpl, format!("placement failed: {e:#}"));
                self.release(spawned).await;
                return None;
            }
        };
        Some(Spawned {
            facing: direction(spawned.pos, placed),
            ..spawned
        })
    }

    async fn test_s1(&mut self) -> anyhow::Result<()> {
        for (tpl, with_hits, occurrence) in [
            (7673u32, false, 0usize),
            (10011, false, 1),
            (10011, true, 2),
            (7491, false, 0),
        ] {
            self.leash_case(tpl, occurrence, with_hits).await?;
        }
        Ok(())
    }

    async fn leash_case(
        &mut self,
        tpl: u32,
        occurrence: usize,
        with_hits: bool,
    ) -> anyhow::Result<()> {
        let row = self
            .db
            .npc(tpl)?
            .with_context(|| format!("npc {tpl} missing from compact"))?;
        let test = if with_hits {
            "S1_leash_hits"
        } else {
            "S1_leash"
        };
        let Some(spawned) = self.engage(tpl, occurrence, 4.0, test, "S1").await else {
            return Ok(());
        };
        let aggro_ms = self
            .obs
            .hostile_onset(spawned.obj, self.bot(), 0, self.obs.now_ms())
            .map(|(t, _)| t);
        let mut bot_range = 4.0f32;
        let mut behavior_samples = Vec::new();
        let mut give_up = false;
        let mut give_up_home = None;
        let mut give_up_gap = None;
        let mut give_up_behavior = None;
        for _ in 0..12 {
            bot_range += 8.0;
            let target = (
                spawned.pos.0 + spawned.facing.0 * bot_range,
                spawned.pos.1 + spawned.facing.1 * bot_range,
                spawned.pos.2,
            );
            self.teleport(target).await?;
            if with_hits {
                let _ = self.damage(spawned.obj, "1").await;
            }
            self.pump_for(Duration::from_millis(1800)).await?;
            let behavior = self.poll_behavior(spawned.obj).await?;
            let npc_home = self
                .npc_pos(spawned.obj)
                .map_or(0.0, |p| dist(p, spawned.pos));
            if let Some(b) = &behavior {
                behavior_samples.push(json!({"bot_home_m": bot_range, "behavior": b}));
            }
            if behavior.as_deref() == Some("ReturnState") {
                give_up = true;
                give_up_behavior = behavior;
                give_up_home = Some(npc_home);
                give_up_gap = self
                    .npc_pos(spawned.obj)
                    .zip(self.obs.bot_pos_at(self.obs.now_ms()))
                    .map(|(a, b)| dist(a, b));
                break;
            }
            if npc_home >= 110.0 {
                break;
            }
        }
        if !give_up {
            self.pump_for(Duration::from_millis(4000)).await?;
            if let Some(b) = self.poll_behavior(spawned.obj).await? {
                behavior_samples.push(json!({"bot_home_m": bot_range, "behavior": b}));
                if b == "ReturnState" {
                    give_up = true;
                    give_up_behavior = Some(b);
                    give_up_home = self.npc_pos(spawned.obj).map(|p| dist(p, spawned.pos));
                    give_up_gap = self
                        .npc_pos(spawned.obj)
                        .zip(self.obs.bot_pos_at(self.obs.now_ms()))
                        .map(|(a, b)| dist(a, b));
                }
            }
        }
        let npc_home_final = self
            .npc_pos(spawned.obj)
            .map_or(0.0, |p| dist(p, spawned.pos));
        let followed_past_80 = !give_up && npc_home_final >= 80.0;
        let classification = if followed_past_80 {
            "matches"
        } else if give_up_home.is_some_and(|h| h <= 65.0) {
            "suspect"
        } else {
            "inconclusive"
        };
        self.line(AiLine {
            test: test.into(),
            suspect: "S1".into(),
            npc_template: tpl,
            npc_name: row.name.clone(),
            npc_obj: spawned.obj,
            classification: classification.into(),
            expected_source: "lua12+10x".into(),
            expected: json!({
                "retail_follows_at_least_m": 100.0,
                "retail_per_target_cancel_distance_m": row.return_distance,
                "retail_cancel_grace_ms": 3000,
                "retail_absolute_return_distance_m": row.absolute_return_distance,
                "rust_formula_gives_up_when_npc_farther_than_m": 50.0,
                "rust_grace_ms": 0,
                "npc": row.json(None),
            }),
            measured: json!({
                "gave_up": give_up,
                "gave_up_behavior": give_up_behavior,
                "npc_home_distance_at_give_up_m": give_up_home,
                "npc_bot_distance_at_give_up_m": give_up_gap,
                "npc_home_distance_at_end_m": npc_home_final,
                "followed_past_80_m": followed_past_80,
                "with_hits": with_hits,
                "aggro_source": if spawned.damage_fallback { "damage_fallback" } else { "proximity" },
                "first_hostile_packet_ms": aggro_ms,
                "behavior_samples": behavior_samples,
            }),
            notes: vec![
                "retail (1.2 Lua + 10.x ProcessAggroCancel/ProcessReturn): per-target cancel when the target is farther than return_distance and not hit for 3 s; whole table only past absolute_return_distance from the pull point".into(),
                "Rust should_return: returns immediately when distance_to_target > return_distance OR distance_to_idle > return_distance (50 m defaults); no grace".into(),
            ],
            setup: self.gm_used.clone(),
            evidence: vec![format!("spawned /spawn npc {tpl} at {:?}", spawned.pos)],
        });
        self.release(spawned).await;
        Ok(())
    }

    // ---- S2: alert vs aggression radii -----------------------------------------------------

    async fn test_s2(&mut self) -> anyhow::Result<()> {
        // 10011: attack 0.6 / sight 1.2 -> Rust 6/18 m, retail base 9/12 m.
        // 835 (Bloodmist warrior): attack 0.7 / sight 1.2 -> Rust 7/18 m, retail base 10.5/12 m.
        let plan: [(u32, &[f32], usize); 2] = [
            (10011, &[4.0, 5.5, 6.5, 8.0, 9.5, 12.0, 15.0, 18.0], 1),
            (835, &[4.0, 6.5, 7.5, 9.0, 10.5, 13.0, 15.5, 18.0], 0),
        ];
        for (tpl, distances, occurrence) in plan {
            self.radii_case(tpl, distances, occurrence).await?;
        }
        Ok(())
    }

    async fn radii_case(
        &mut self,
        tpl: u32,
        distances: &[f32],
        occurrence: usize,
    ) -> anyhow::Result<()> {
        let row = self
            .db
            .npc(tpl)?
            .with_context(|| format!("npc {tpl} missing from compact"))?;
        let mut samples = Vec::new();
        let mut alert_samples = Vec::new();
        let mut evidence = Vec::new();
        for (i, d) in distances.iter().enumerate() {
            let spawned = match self.locate_npc(tpl, occurrence + i).await {
                Ok(s) => s,
                Err(e) => {
                    evidence.push(format!("spawn at {d} m failed: {e:#}"));
                    continue;
                }
            };
            let bot = self.bot();
            let mut alert = None;
            let mut hostile = None;
            let mut cleared = None;
            for bearing in [0.0f32, 90.0, 180.0, 270.0] {
                let angle = bearing.to_radians();
                let pos = (
                    spawned.pos.0 + angle.cos() * d,
                    spawned.pos.1 + angle.sin() * d,
                    spawned.pos.2,
                );
                if let Err(e) = self.teleport(pos).await {
                    evidence.push(format!("place at {d} m failed: {e:#}"));
                    break;
                }
                let t0 = self.obs.now_ms();
                self.pump_for(Duration::from_millis(2500)).await?;
                let now = self.obs.now_ms();
                hostile = self.obs.hostile_onset(spawned.obj, bot, t0, now);
                alert = self.obs.alert_onset(spawned.obj, bot, t0, now);
                if hostile.is_some() || alert.is_some() {
                    if let Some(a) = alert {
                        self.pump_for(Duration::from_millis(6000)).await?;
                        let later = self.obs.now_ms();
                        hostile = self.obs.hostile_onset(spawned.obj, bot, t0, later);
                        cleared = self.obs.alert_cleared(spawned.obj, a, later);
                    }
                    break;
                }
            }
            let reacted = hostile.is_some() || alert.is_some();
            samples.push((*d, hostile.is_some()));
            alert_samples.push((*d, alert.is_some()));
            evidence.push(format!(
                "d={d} alert={} hostile={} alert_window_ms={:?}",
                alert.is_some(),
                hostile.is_some(),
                cleared.zip(alert).map(|(c, a)| c.saturating_sub(a)),
            ));
            let _ = reacted;
            self.release(spawned).await;
        }
        let aggro_m = threshold(&samples);
        let alert_m = threshold(&alert_samples);
        let (a, s) = (row.attack_scale, row.sight_scale);
        let rust_aggro = a * 10.0;
        let rust_alert = s * 15.0;
        let retail_attack = a * 15.0;
        let retail_sight = s * 10.0;
        let classification = match aggro_m {
            Some(m)
                if (m - rust_aggro).abs() <= 1.5 && (rust_aggro - retail_attack).abs() > 1.0 =>
            {
                "suspect"
            }
            Some(m) if (m - retail_attack).abs() <= 1.5 => "matches",
            Some(_) => "inconclusive",
            // No aggression at all, not even inside the server's own attack radius.
            None if alert_m.is_some() => "suspect",
            None => "inconclusive",
        };
        let mut notes = vec![
            "Rust radii: aggression = attack_start_range_scale*10, alert = sight_range_scale*15 (check_aggression / check_alert)".into(),
            "retail basicaitable.lua defaults attackStartRange=15, sightrange=10; how the npcs scale columns multiply them is the open [?] item (10.x oracle, weaker)".into(),
            "each distance was tried from four bearings because the NPC's FOV is not on the wire".into(),
        ];
        if let Some(m) = alert_m {
            notes.push(format!("measured alert (TargetChanged) threshold {m} m"));
        }
        self.line(AiLine {
            test: "S2_radii".into(),
            suspect: "S2".into(),
            npc_template: tpl,
            npc_name: row.name.clone(),
            npc_obj: 0,
            classification: classification.into(),
            expected_source: "lua12+10x(weak scale mapping)".into(),
            expected: json!({
                "rust_formula": {"aggro_m": rust_aggro, "alert_m": rust_alert},
                "retail_base_scaled": {"attack_m": retail_attack, "sight_m": retail_sight},
                "npc": row.json(None),
            }),
            measured: json!({
                "aggro_threshold_m": aggro_m,
                "alert_threshold_m": alert_m,
                "samples": samples.iter().map(|(d, h)| json!({"d": d, "hostile": h})).collect::<Vec<_>>(),
                "alert_samples": alert_samples.iter().map(|(d, x)| json!({"d": d, "alert": x})).collect::<Vec<_>>(),
            }),
            notes,
            setup: self.gm_used.clone(),
            evidence,
        });
        Ok(())
    }

    // ---- S3: alert -> attack timing -------------------------------------------------------

    async fn test_s3(&mut self) -> anyhow::Result<()> {
        self.alert_case(
            10011,
            0,
            8.0,
            3000,
            "default alertDuration 3 s (no param row)",
        )
        .await?;
        self.alert_case(8023, 0, 8.0, 0, "param 1220 alertDuration = 0")
            .await?;
        self.alert_case(528, 8, 8.5, 10000, "param 318 alertDuration = 10")
            .await?;
        Ok(())
    }

    async fn alert_case(
        &mut self,
        tpl: u32,
        occurrence: usize,
        distance: f32,
        expected_ms: u64,
        note: &str,
    ) -> anyhow::Result<()> {
        let row = self
            .db
            .npc(tpl)?
            .with_context(|| format!("npc {tpl} missing from compact"))?;
        let param = self.db.param(row.npc_ai_param_id)?;
        let spawned = match self.locate_npc(tpl, occurrence).await {
            Ok(s) => s,
            Err(e) => {
                self.inconclusive(
                    "S3_alert_to_attack",
                    "S3",
                    tpl,
                    format!("spawn failed: {e:#}"),
                );
                return Ok(());
            }
        };
        let bot = self.bot();
        let mut reaction = None;
        for bearing in [0.0f32, 90.0, 180.0, 270.0] {
            let angle = bearing.to_radians();
            let pos = (
                spawned.pos.0 + angle.cos() * distance,
                spawned.pos.1 + angle.sin() * distance,
                spawned.pos.2,
            );
            if let Err(e) = self.teleport(pos).await {
                self.inconclusive(
                    "S3_alert_to_attack",
                    "S3",
                    tpl,
                    format!("placement failed: {e:#}"),
                );
                self.release(spawned).await;
                return Ok(());
            }
            let t0 = self.obs.now_ms();
            self.pump_for(Duration::from_millis(2500)).await?;
            let alert = self
                .obs
                .alert_onset(spawned.obj, bot, t0, self.obs.now_ms());
            if alert.is_some()
                || self
                    .obs
                    .hostile_onset(spawned.obj, bot, t0, self.obs.now_ms())
                    .is_some()
            {
                reaction = Some(alert);
                break;
            }
        }
        if reaction.is_none() {
            let mut diag = Vec::new();
            let _ = self.game.change_target(spawned.obj).await;
            self.pump_for(Duration::from_millis(150)).await?;
            if let Ok(chats) = self.gm("dist", Duration::from_millis(400)).await {
                diag.extend(chats);
            }
            if let Ok(Some(b)) = self.poll_behavior(spawned.obj).await {
                diag.push(format!("behavior={b}"));
            }
            self.inconclusive(
                "S3_alert_to_attack",
                "S3",
                tpl,
                format!(
                    "NPC {tpl} did not notice the bot at {distance} m from any bearing; server: {}",
                    diag.join(" / ")
                ),
            );
            self.release(spawned).await;
            return Ok(());
        }
        let t0 = self.obs.now_ms();
        let watch_ms = (expected_ms + 6000).max(9000);
        let until = t0 + watch_ms;
        self.pump_for(Duration::from_millis(watch_ms)).await?;
        let alert = self.obs.alert_onset(spawned.obj, bot, 0, until);
        let hostile = self.obs.hostile_onset(spawned.obj, bot, 0, until);
        let cleared = alert.and_then(|a| self.obs.alert_cleared(spawned.obj, a, until));
        let (alert_base, hostile_base) = match (alert, hostile.as_ref()) {
            (Some(a), Some((h, _))) => (a, *h),
            (Some(a), None) => (a, a),
            (None, Some((h, _))) => (*h, *h),
            (None, None) => (t0, t0),
        };
        let behavior = self.poll_behavior(spawned.obj).await?;
        let onset = hostile
            .as_ref()
            .map(|(t, _)| t.saturating_sub(hostile_base));
        let alert_len = cleared.zip(alert).map(|(c, a)| c.saturating_sub(a));
        let classification = if alert.is_none() {
            // No alert target change: the NPC attacked straight away (or only after the watch).
            match hostile.as_ref() {
                Some((h, _)) if h.saturating_sub(t0) <= 1500 => "matches",
                Some(_) => "suspect",
                None => "inconclusive",
            }
        } else if let Some(onset) = onset {
            if expected_ms == 0 {
                if onset <= 1500 {
                    "matches"
                } else {
                    "suspect"
                }
            } else if onset.abs_diff(expected_ms) <= 2500 {
                "matches"
            } else {
                "suspect"
            }
        } else {
            "suspect"
        };
        let mut notes = vec![
            note.to_string(),
            "retail: alertDuration < 0.1 s attacks at once; otherwise the alert lasts alertDuration, then an aggressive species attacks (default.lua, 1.2 and 10.x)".into(),
            "Rust alert_end_time = now + 5000 ms fixed; only check_aggression can start combat inside it (behaviors.rs)".into(),
            "the distance was reached from four bearings; the alert/attack radii themselves are measured by S2".into(),
        ];
        if let Some(len) = alert_len {
            notes.push(format!("measured alert window {len} ms"));
        }
        self.line(AiLine {
            test: "S3_alert_to_attack".into(),
            suspect: "S3".into(),
            npc_template: tpl,
            npc_name: row.name.clone(),
            npc_obj: spawned.obj,
            classification: classification.into(),
            expected_source: "lua12".into(),
            expected: json!({
                "distance_m": distance,
                "alert_duration_ms": expected_ms,
                "attack_after_alert_ms": expected_ms,
                "npc": row.json(param.as_deref()),
            }),
            measured: json!({
                "alert_onset_ms": alert.map(|t| t.saturating_sub(alert_base)),
                "alert_window_ms": alert_len,
                "hostile_onset_ms": onset,
                "hostile_kind": hostile.as_ref().map(|(_, k)| k),
                "behavior_after": behavior,
            }),
            notes,
            setup: self.gm_used.clone(),
            evidence: vec![format!("placed at {distance} m; watch {watch_ms} ms")],
        });
        self.release(spawned).await;
        Ok(())
    }

    // ---- S4: ranged stopping distance -----------------------------------------------------

    async fn test_s4(&mut self) -> anyhow::Result<()> {
        self.ranged_case(528, 9, 6.0, 18.0).await?;
        self.ranged_case(529, 0, 6.0, 20.0).await?;
        Ok(())
    }

    async fn ranged_case(
        &mut self,
        tpl: u32,
        occurrence: usize,
        start: f32,
        retreat: f32,
    ) -> anyhow::Result<()> {
        let row = self
            .db
            .npc(tpl)?
            .with_context(|| format!("npc {tpl} missing from compact"))?;
        let param = self.db.param(row.npc_ai_param_id)?;
        let preferred = param.as_deref().and_then(preferred_combat_dist);
        let Some(spawned) = self
            .engage(tpl, occurrence, start, "S4_ranged_stop", "S4")
            .await
        else {
            return Ok(());
        };
        let t0 = self.obs.now_ms();
        self.pump_for(Duration::from_millis(5000)).await?;
        let pre_casts = self.obs.skill_starts(spawned.obj, t0, self.obs.now_ms());
        let target = (
            spawned.pos.0 + spawned.facing.0 * retreat,
            spawned.pos.1 + spawned.facing.1 * retreat,
            spawned.pos.2,
        );
        self.teleport(target).await?;
        let t1 = self.obs.now_ms();
        self.pump_for(Duration::from_millis(9000)).await?;
        let t2 = self.obs.now_ms();
        let post = self.obs.skill_starts(spawned.obj, t1, t2);
        let mut cast_distances = Vec::new();
        for (t, skill) in &post {
            if let (Some(np), Some(bp)) = (
                self.obs.last_position(spawned.obj, *t),
                self.obs.bot_pos_at(*t),
            ) {
                cast_distances.push(json!({
                    "t_ms": t.saturating_sub(t0),
                    "skill": skill,
                    "distance_m": dist(np, bp),
                }));
            }
        }
        let stop = self
            .npc_pos(spawned.obj)
            .zip(self.obs.bot_pos_at(self.obs.now_ms()))
            .map(|(np, bp)| dist(np, bp));
        let skill_ranges: Vec<serde_json::Value> = post
            .iter()
            .map(|(_, skill)| {
                self.db.skill(*skill).ok().flatten().map_or_else(
                    || json!({"id": skill}),
                    |s| {
                        json!({
                            "id": skill,
                            "name": s.name,
                            "min_range": s.min_range,
                            "max_range": s.max_range,
                            "plot_id": s.plot_id,
                            "expected_stop_0_85_range_m": s.max_range * 0.85,
                        })
                    },
                )
            })
            .collect();
        let stop_median = median(
            cast_distances
                .iter()
                .filter_map(|v| v.get("distance_m").and_then(serde_json::Value::as_f64))
                .map(|v| v as f32)
                .collect(),
        );
        let expected_stop = post
            .iter()
            .filter_map(|(_, id)| self.db.skill(*id).ok().flatten())
            .map(|s| s.max_range * 0.85)
            .fold(None, |acc: Option<f32>, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            });
        let classification = match (stop_median, expected_stop) {
            (Some(m), Some(e)) => classify_distance(m, e, 2.5),
            (None, _) => "suspect",
            (Some(_), None) => "inconclusive",
        };
        let mut notes = vec![
            "retail stops at 0.85 x the picked skill range (10.x IsTargetInSkillRange; 1.2 zone unavailable -> weaker)".into(),
            format!("Rust move_in_range stops at attack_start_range_scale x max_weapon_range; archers use preferedCombatDist={preferred:?}"),
        ];
        if post.is_empty() {
            notes.push(format!(
                "NPC cast nothing after the bot retreated to {retreat} m (settled distance {stop:?} m)"
            ));
        }
        self.line(AiLine {
            test: "S4_ranged_stop".into(),
            suspect: "S4".into(),
            npc_template: tpl,
            npc_name: row.name.clone(),
            npc_obj: spawned.obj,
            classification: classification.into(),
            expected_source: "10x(weak)".into(),
            expected: json!({
                "stop_distance_0_85_skill_range_m": expected_stop,
                "prefered_combat_dist_m": preferred,
                "npc": row.json(param.as_deref()),
            }),
            measured: json!({
                "start_distance_m": start,
                "retreat_distance_m": retreat,
                "first_casts": pre_casts.iter().map(|(t, s)| json!({"t_ms": t.saturating_sub(t0), "skill": s})).collect::<Vec<_>>(),
                "post_retreat_casts": cast_distances,
                "cast_stop_distance_median_m": stop_median,
                "final_npc_bot_distance_m": stop,
                "skill_ranges": skill_ranges,
            }),
            notes,
            setup: self.gm_used.clone(),
            evidence: vec![format!("spawned {tpl} at {:?}", spawned.pos)],
        });
        self.release(spawned).await;
        Ok(())
    }

    // ---- S5: idle displacement / return ---------------------------------------------------

    async fn test_s5(&mut self) -> anyhow::Result<()> {
        self.idle_case(10011, 10, 8.0, false).await?;
        self.idle_case(2049, 0, 20.0, true).await?;
        Ok(())
    }

    async fn idle_case(
        &mut self,
        tpl: u32,
        occurrence: usize,
        offset: f32,
        roaming: bool,
    ) -> anyhow::Result<()> {
        let row = self
            .db
            .npc(tpl)?
            .with_context(|| format!("npc {tpl} missing from compact"))?;
        let spawned = match self.locate_npc(tpl, occurrence).await {
            Ok(s) => s,
            Err(e) => {
                self.inconclusive("S5_idle_return", "S5", tpl, format!("spawn failed: {e:#}"));
                return Ok(());
            }
        };
        // Leave the NPC's sight before it settles, so it stays idle for the displacement.
        let away = (
            spawned.pos.0 + spawned.facing.0 * 30.0,
            spawned.pos.1 + spawned.facing.1 * 30.0,
            spawned.pos.2,
        );
        self.teleport(away).await?;
        self.pump_for(Duration::from_millis(4000)).await?;
        let home = self.npc_pos(spawned.obj).unwrap_or(spawned.pos);
        let displaced = (home.0 + offset, home.1, home.2);
        self.setpos(spawned.obj, displaced).await?;
        self.pump_for(Duration::from_millis(1500)).await?;
        let mut samples = vec![(
            self.obs.now_ms(),
            self.npc_pos(spawned.obj).unwrap_or(displaced),
        )];
        let mut behavior_samples = Vec::new();
        for _ in 0..4 {
            self.pump_for(Duration::from_millis(4000)).await?;
            if let Some(p) = self.npc_pos(spawned.obj) {
                samples.push((self.obs.now_ms(), p));
            }
            if let Some(b) = self.poll_behavior(spawned.obj).await? {
                behavior_samples.push(b);
            }
        }
        let final_pos = self.npc_pos(spawned.obj).unwrap_or(displaced);
        let displacement_m = dist(final_pos, home);
        let jump = biggest_jump(&samples);
        let classification = if roaming {
            if jump >= 10.0 {
                "matches"
            } else if displacement_m <= 3.0 {
                "inconclusive"
            } else {
                "suspect"
            }
        } else if displacement_m <= 2.5 {
            "matches"
        } else if displacement_m >= 5.0 {
            "suspect"
        } else {
            "inconclusive"
        };
        self.line(AiLine {
            test: if roaming {
                "S5_idle_return_roaming".into()
            } else {
                "S5_idle_return_hold".into()
            },
            suspect: "S5".into(),
            npc_template: tpl,
            npc_name: row.name.clone(),
            npc_obj: spawned.obj,
            classification: classification.into(),
            expected_source: "lua12+10x".into(),
            expected: json!({
                "displacement_m": offset,
                "return_walk_at_m": 3.0,
                "teleport_home_at_m": if roaming { 15.0 } else { 7.0 },
                "final_displacement_m": 0.0,
                "npc": row.json(None),
            }),
            measured: json!({
                "displaced_m": offset,
                "final_displacement_m": displacement_m,
                "biggest_position_jump_m": jump,
                "home": home,
                "final_pos": final_pos,
                "behavior_samples": behavior_samples,
                "sample_positions": samples.iter().map(|(t, p)| json!({"t_ms": t, "pos": p})).collect::<Vec<_>>(),
            }),
            notes: vec![
                "retail 1.2 hold_position: every 2 s, dist(idlePos) >= 7 m teleports home, >= 3 m signals ReturnToIdlePos (walk); 10.x CheckPos context 2 keeps the 7 m".into(),
                "retail 1.2 roaming: more than 15 m from idlePos teleports home (IsOutOfIdleArea); 10.x CheckPos context 1 keeps 15 m".into(),
                "Rust hold_position/roaming ticks have no idle-position check (behaviors.rs)".into(),
            ],
            setup: self.gm_used.clone(),
            evidence: vec![format!(
                "NPC engaged then released; /setpos {} {} {} {}",
                spawned.obj, displaced.0, displaced.1, displaced.2
            )],
        });
        if spawned.natural {
            let _ = self.setpos(spawned.obj, home).await;
        }
        self.release(spawned).await;
        Ok(())
    }
}
// -------------------------------------------------------------------------------------------
// S6-S10
// -------------------------------------------------------------------------------------------

impl Sweep {
    async fn test_s6(&mut self) -> anyhow::Result<()> {
        self.make_gap_case(528, 10).await?;
        for tpl in [529u32, 702] {
            let row = self.db.npc(tpl)?.with_context(|| format!("npc {tpl}"))?;
            let param = self.db.param(row.npc_ai_param_id)?;
            let skill = param.as_deref().and_then(spurt_skill);
            self.line(AiLine {
                test: "S6_make_a_gap".into(),
                suspect: "S6".into(),
                npc_template: tpl,
                npc_name: row.name.clone(),
                npc_obj: 0,
                classification: "inconclusive".into(),
                expected_source: "lua12".into(),
                expected: json!({
                    "backoff_m": preferred_combat_dist(param.as_deref().unwrap_or("")),
                    "backoff_ms": 3000,
                    "npc": row.json(param.as_deref()),
                }),
                measured: json!({
                    "make_a_gap_skill": skill,
                    "make_a_gap_skill_in_1_2_data": skill.map(|id| self.db.skill(id).ok().flatten().is_some()),
                }),
                notes: vec![
                    "not exercised live: the 1.2 param has no usable make-a-gap skill for this NPC".into(),
                ],
                setup: Vec::new(),
                evidence: Vec::new(),
            });
        }
        Ok(())
    }

    async fn make_gap_case(&mut self, tpl: u32, occurrence: usize) -> anyhow::Result<()> {
        let row = self
            .db
            .npc(tpl)?
            .with_context(|| format!("npc {tpl} missing from compact"))?;
        let param = self.db.param(row.npc_ai_param_id)?;
        let preferred = param.as_deref().and_then(preferred_combat_dist);
        let Some(spawned) = self
            .engage(tpl, occurrence, 3.0, "S6_make_a_gap", "S6")
            .await
        else {
            return Ok(());
        };
        let t0 = self.obs.now_ms();
        self.pump_for(Duration::from_millis(8000)).await?;
        let t1 = self.obs.now_ms();
        let casts = self.obs.skill_starts(spawned.obj, t0, t1);
        let first_cast = casts.first().map(|(t, _)| *t).unwrap_or(t0);
        let window_end = first_cast + 3000;
        let samples = self.obs.positions(spawned.obj, first_cast, window_end);
        let (movement, away, toward) = if let (Some(first), Some(last)) =
            (samples.first().copied(), samples.last().copied())
        {
            let d = (last.1 .0 - first.1 .0, last.1 .1 - first.1 .1, 0.0);
            let bot = self
                .obs
                .bot_pos_at(last.0)
                .unwrap_or(self.game.state.self_pos);
            (
                Some((d.0 * d.0 + d.1 * d.1).sqrt()),
                Some(away_dot(d, first.1, bot)),
                Some(toward_dot(d, first.1, spawned.pos)),
            )
        } else {
            (None, None, None)
        };
        let displacement = movement.unwrap_or(0.0);
        let classification = if displacement >= 6.0 {
            "matches"
        } else if displacement <= 2.0 {
            "suspect"
        } else {
            "inconclusive"
        };
        self.line(AiLine {
            test: "S6_make_a_gap".into(),
            suspect: "S6".into(),
            npc_template: tpl,
            npc_name: row.name.clone(),
            npc_obj: spawned.obj,
            classification: classification.into(),
            expected_source: "lua12".into(),
            expected: json!({
                "backoff_m": preferred,
                "backoff_ms": 3000,
                "npc": row.json(param.as_deref()),
            }),
            measured: json!({
                "casts": casts.iter().map(|(t, s)| json!({"t_ms": t.saturating_sub(t0), "skill": s})).collect::<Vec<_>>(),
                "gap_window_displacement_m": movement,
                "gap_window_away_from_target_dot": away,
                "gap_window_toward_home_dot": toward,
                "final_pos": self.npc_pos(spawned.obj),
            }),
            notes: vec![
                "retail archer: after a melee skill, if still in melee and makeAGapCount < max, cast makeAGap and back off away from the target by preferedCombatDist for 3 s (archer_attack.lua, same in 1.2 and 10.x)".into(),
                "Rust NeedMakeAGap walks towards the idle position until <1 m from it or the target is beyond preferedCombatDist (behaviors.rs)".into(),
            ],
            setup: self.gm_used.clone(),
            evidence: vec![format!("engaged at 3 m from {:?}", spawned.pos)],
        });
        self.release(spawned).await;
        Ok(())
    }

    // ---- S7: wild boar spurt --------------------------------------------------------------

    async fn test_s7(&mut self) -> anyhow::Result<()> {
        self.spurt_case(3469).await?;
        self.spurt_case(7673).await?;
        Ok(())
    }

    async fn spurt_case(&mut self, tpl: u32) -> anyhow::Result<()> {
        let row = self
            .db
            .npc(tpl)?
            .with_context(|| format!("npc {tpl} missing from compact"))?;
        let Some(param) = self.db.param(row.npc_ai_param_id)? else {
            self.inconclusive(
                "S7_boar_spurt",
                "S7",
                tpl,
                format!("npc {tpl} has no npc_ai_params row (no onSpurtSkill)"),
            );
            return Ok(());
        };
        let (Some(skill), Some(condition)) = (spurt_skill(&param), spurt_health_condition(&param))
        else {
            self.inconclusive(
                "S7_boar_spurt",
                "S7",
                tpl,
                format!("npc {tpl} param has no onSpurtSkill/healthCondition: {param:?}"),
            );
            return Ok(());
        };
        let Some(spawned) = self.engage(tpl, 0, 4.0, "S7_boar_spurt", "S7").await else {
            return Ok(());
        };
        // Natural NPCs keep their HP between runs; restore it before the crossing.
        let _ = self.game.change_target(spawned.obj).await;
        self.pump_for(Duration::from_millis(150)).await?;
        let _ = self.gm("heal", Duration::from_millis(400)).await;
        self.pump_for(Duration::from_millis(1500)).await?;
        let hp_before = self.obs.hp_at(spawned.obj, self.obs.now_ms());
        self.damage(spawned.obj, "60%").await?;
        let t1 = self.obs.now_ms();
        self.pump_for(Duration::from_millis(25000)).await?;
        let t2 = self.obs.now_ms();
        let hp_after = self.obs.hp_at(spawned.obj, t1 + 1500);
        let casts = self.obs.skill_starts(spawned.obj, t1, t2);
        let spurt_casts: Vec<_> = casts.iter().filter(|(_, s)| *s == skill).collect();
        let classification = if spurt_casts.len() >= 2 {
            "suspect"
        } else if spurt_casts.len() == 1 {
            "matches"
        } else {
            "inconclusive"
        };
        self.line(AiLine {
            test: "S7_boar_spurt".into(),
            suspect: "S7".into(),
            npc_template: tpl,
            npc_name: row.name.clone(),
            npc_obj: spawned.obj,
            classification: classification.into(),
            expected_source: "lua12".into(),
            expected: json!({
                "spurt_skill": skill,
                "health_condition_pct": condition,
                "casts_per_crossing": 1,
                "npc": row.json(Some(&param)),
            }),
            measured: json!({
                "hp_before": hp_before,
                "hp_after": hp_after,
                "spurt_casts": spurt_casts.len(),
                "spurt_cast_times_ms": spurt_casts.iter().map(|(t, _)| t.saturating_sub(t1)).collect::<Vec<_>>(),
                "all_casts": casts.iter().map(|(t, s)| json!({"t_ms": t.saturating_sub(t1), "skill": s})).collect::<Vec<_>>(),
                "window_ms": t2.saturating_sub(t1),
            }),
            notes: vec![
                format!("retail: onSpurtSkill {{ skillType {skill}, healthCondition {condition} }} casts once on the downward crossing, not again while below (wild_boar_attack.lua)"),
                "Rust tests curr_health < healthCondition every attack tick with no crossing memory (behaviors.rs); only the 15 s skill cooldown limits repeats".into(),
            ],
            setup: self.gm_used.clone(),
            evidence: vec![format!("[/damage 60%] on obj {} after 1.5 s of combat", spawned.obj)],
        });
        self.release(spawned).await;
        Ok(())
    }

    // ---- S8: combat-start skill burst -----------------------------------------------------

    async fn test_s8(&mut self) -> anyhow::Result<()> {
        let rows = self.db.count_burst_candidates()?;
        let cond0 = self.db.count_cond0()?;
        self.line(AiLine {
            test: "S8_combat_start_burst".into(),
            suspect: "S8".into(),
            npc_template: 0,
            npc_name: "(data)".into(),
            npc_obj: 0,
            classification: "inconclusive".into(),
            expected_source: "lua12".into(),
            expected: json!({
                "retail_1_2_combat_start_casts": 0,
                "rust_burst_gate": "np_skills cond 0 + skills.ignore_global_cooldown + no plot",
            }),
            measured: json!({
                "cond0_rows": cond0,
                "cond0_ignore_gcd_plotless_rows": rows,
            }),
            notes: vec![
                "the 1.2 data has no skill row that meets the Rust burst gate, so the burst path has nothing to cast and cannot be exercised on this data".into(),
            ],
            setup: Vec::new(),
            evidence: vec![format!("SELECT COUNT(*) ... -> {rows}")],
        });
        self.burst_control(7673, 1).await?;
        Ok(())
    }

    async fn burst_control(&mut self, tpl: u32, occurrence: usize) -> anyhow::Result<()> {
        let row = self
            .db
            .npc(tpl)?
            .with_context(|| format!("npc {tpl} missing from compact"))?;
        let Some(spawned) = self
            .engage(tpl, occurrence, 4.0, "S8_combat_start_burst_control", "S8")
            .await
        else {
            return Ok(());
        };
        let hostile_at = self
            .obs
            .hostile_onset(spawned.obj, self.bot(), 0, self.obs.now_ms())
            .map(|(t, _)| t)
            .unwrap_or_else(|| self.obs.now_ms());
        self.pump_for(Duration::from_millis(2500)).await?;
        let casts = self
            .obs
            .skill_starts(spawned.obj, hostile_at, hostile_at + 2000);
        let burst = casts
            .windows(2)
            .any(|w| w[1].0.saturating_sub(w[0].0) <= 300);
        self.line(AiLine {
            test: "S8_combat_start_burst_control".into(),
            suspect: "S8".into(),
            npc_template: tpl,
            npc_name: row.name.clone(),
            npc_obj: spawned.obj,
            classification: if burst { "suspect" } else { "matches" }.into(),
            expected_source: "lua12".into(),
            expected: json!({"burst_within_300ms": false, "npc": row.json(None)}),
            measured: json!({
                "casts_within_2s": casts.iter().map(|(t, s)| json!({"t_ms": t.saturating_sub(hostile_at), "skill": s})).collect::<Vec<_>>(),
                "burst": burst,
            }),
            notes: vec![
                "control NPC: neither cond-0 skill ignores the GCD, so the Rust burst gate does not select them".into(),
            ],
            setup: self.gm_used.clone(),
            evidence: Vec::new(),
        });
        self.release(spawned).await;
        Ok(())
    }

    // ---- S9: spawn window -----------------------------------------------------------------

    async fn test_s9(&mut self) -> anyhow::Result<()> {
        self.spawn_window_case(523).await?;
        self.spawn_window_case(1575).await?;
        Ok(())
    }

    async fn spawn_window_case(&mut self, tpl: u32) -> anyhow::Result<()> {
        let row = self
            .db
            .npc(tpl)?
            .with_context(|| format!("npc {tpl} missing from compact"))?;
        let spawn_skills = self.db.skills_of_condition(tpl, 5)?;
        let spawned = match self.locate_npc(tpl, 0).await {
            Ok(s) => s,
            Err(e) => {
                self.inconclusive("S9_spawn_window", "S9", tpl, format!("spawn failed: {e:#}"));
                return Ok(());
            }
        };
        let spawn_t = self
            .obs
            .events
            .iter()
            .rev()
            .find(|e| matches!(&e.kind, Ev::UnitState { obj, .. } if *obj == spawned.obj))
            .map_or_else(|| self.obs.now_ms(), |e| e.t_ms);
        let hp0 = self.obs.hp_at(spawned.obj, self.obs.now_ms());
        self.damage(spawned.obj, "5").await?;
        self.pump_for(Duration::from_millis(600)).await?;
        let hp1 = self.obs.hp_at(spawned.obj, self.obs.now_ms());
        self.damage(spawned.obj, "5").await?;
        self.pump_for(Duration::from_millis(600)).await?;
        let hp2 = self.obs.hp_at(spawned.obj, self.obs.now_ms());
        let damage_applied = match (hp0, hp1, hp2) {
            (Some(a), Some(b), _) => b < a,
            (_, Some(a), Some(b)) => b < a,
            _ => false,
        };
        let behavior = self.poll_behavior(spawned.obj).await?;
        let casts = self.obs.skill_starts(spawned.obj, spawn_t, spawn_t + 3000);
        let first_cast = casts.first().map(|(t, _)| t.saturating_sub(spawn_t));
        let damage_ms = self.obs.now_ms().saturating_sub(spawn_t);
        let classification = if damage_applied {
            "suspect"
        } else if behavior.as_deref() == Some("Spawning") {
            "matches"
        } else {
            "inconclusive"
        };
        self.line(AiLine {
            test: "S9_spawn_window".into(),
            suspect: "S9".into(),
            npc_template: tpl,
            npc_name: row.name.clone(),
            npc_obj: spawned.obj,
            classification: classification.into(),
            expected_source: "lua12+10x(weak window value)".into(),
            expected: json!({
                "behavior_at_spawn": "Spawning",
                "invincible_during_window": true,
                "on_spawn_skills": spawn_skills,
                "window_ms": "spawningEffectTime >= 100 (exact value [?])",
                "npc": row.json(None),
            }),
            measured: json!({
                "behavior_after_spawn": behavior,
                "hp0": hp0,
                "hp_after_first_damage": hp1,
                "hp_after_second_damage": hp2,
                "spawn_age_at_first_damage_ms": damage_ms,
                "damage_applied": damage_applied,
                "first_npc_cast_ms": first_cast,
                "casts_within_3s": casts.iter().map(|(t, s)| json!({"t_ms": t.saturating_sub(spawn_t), "skill": s})).collect::<Vec<_>>(),
            }),
            notes: vec![
                "retail 1.2 spawning.lua: spawn state Busy, aggro update off, NPC_SPAWN_INVINCIBLE buff, AI time event spawningEffectTime (min 0.1 s) then RunCommandSet".into(),
                "Rust spawning_enter fires OnSpawn and spawning_tick goes to RunCommandSet in the same tick; no invincible buff or delay (behaviors.rs)".into(),
            ],
            setup: self.gm_used.clone(),
            evidence: vec![format!("two [/damage 5] probes after /spawn npc {tpl}")],
        });
        self.release(spawned).await;
        Ok(())
    }

    // ---- S10: minAPS ----------------------------------------------------------------------

    async fn test_s10(&mut self) -> anyhow::Result<()> {
        self.min_aps_case(5883).await?;
        self.min_aps_case(8828).await?;
        Ok(())
    }

    async fn min_aps_case(&mut self, tpl: u32) -> anyhow::Result<()> {
        let row = self
            .db
            .npc(tpl)?
            .with_context(|| format!("npc {tpl} missing from compact"))?;
        let param = self.db.param(row.npc_ai_param_id)?;
        let min_aps = param.as_deref().and_then(|p| param_number(p, "minAPS"));
        let spawned = match self.locate_npc(tpl, 0).await {
            Ok(s) => s,
            Err(e) => {
                self.inconclusive("S10_min_aps", "S10", tpl, format!("spawn failed: {e:#}"));
                return Ok(());
            }
        };
        if let Err(e) = self.place_bot(spawned.pos, spawned.facing, 4.0).await {
            self.inconclusive(
                "S10_min_aps",
                "S10",
                tpl,
                format!("placement failed: {e:#}"),
            );
            self.release(spawned).await;
            return Ok(());
        }
        self.pump_for(Duration::from_millis(800)).await?;
        let t0 = self.obs.now_ms();
        // A GM `/damage` changes HP without an aggro entry; one real auto attack does both.
        if let Err(e) = self.auto_attack(spawned.obj).await {
            self.inconclusive("S10_min_aps", "S10", tpl, format!("attack failed: {e:#}"));
            self.release(spawned).await;
            return Ok(());
        }
        let bot = self.bot();
        let aggro = self
            .wait_until(Duration::from_secs(5), |s| {
                s.obs
                    .hostile_onset(spawned.obj, bot, t0, s.obs.now_ms())
                    .is_some()
            })
            .await?;
        if !aggro {
            self.inconclusive(
                "S10_min_aps",
                "S10",
                tpl,
                format!("NPC {tpl} never acquired the bot after the 1-damage probe"),
            );
            self.release(spawned).await;
            return Ok(());
        }
        let probe = self.obs.now_ms();
        let mut behavior_samples = Vec::new();
        let mut cleared_ms = None;
        for _ in 0..4 {
            self.pump_for(Duration::from_millis(3000)).await?;
            if let Some(b) = self.poll_behavior(spawned.obj).await? {
                behavior_samples.push(b);
            }
            if cleared_ms.is_none() {
                cleared_ms = self
                    .obs
                    .alert_cleared(spawned.obj, probe, self.obs.now_ms())
                    .map(|t| t.saturating_sub(probe));
            }
        }
        let kept = behavior_samples
            .iter()
            .any(|b| b == "Attack" || b == "ArcherAttack" || b == "BigMonsterAttack");
        let classification = if cleared_ms.is_some() || !kept {
            "matches"
        } else {
            "suspect"
        };
        self.line(AiLine {
            test: "S10_min_aps".into(),
            suspect: "S10".into(),
            npc_template: tpl,
            npc_name: row.name.clone(),
            npc_obj: spawned.obj,
            classification: classification.into(),
            expected_source: "10x(weak); 1.2 data".into(),
            expected: json!({
                "retail_10x": "an entry whose APS (damage / elapsed seconds) is below minAPS is removed",
                "min_aps": min_aps,
                "npc": row.json(param.as_deref()),
            }),
            measured: json!({
                "probe_damage": 1,
                "cleared_after_ms": cleared_ms,
                "kept_aggro_at_12s": kept,
                "behavior_samples": behavior_samples,
            }),
            notes: vec![
                "the 1.2 data uses minAPS=20 for the basilisks (param 839) and minAPS=100000000 for the 8828 family; the 10.x zone server applies the column, the Rust AI never reads it".into(),
                "one 1-damage hit keeps the bot far below any APS floor, so retail 10.x would drop the entry".into(),
            ],
            setup: self.gm_used.clone(),
            evidence: vec![format!("[/damage 1] on obj {} then 4 behaviour polls", spawned.obj)],
        });
        self.release(spawned).await;
        Ok(())
    }
}

// -------------------------------------------------------------------------------------------
// Run
// -------------------------------------------------------------------------------------------

pub async fn run(cfg: &BotConfig, opts: &RunOptions) -> anyhow::Result<AiReport> {
    let rec = Recorder::new(true);
    let fight_opts = fight::RunOptions {
        new_character: opts.new_character,
        race: opts.race,
        gender: opts.gender,
        ..Default::default()
    };
    let (game, _character, _abilities) = fight::setup_world(cfg, &fight_opts, &rec).await?;
    let db = AiDb::open_default(opts.compact_db.as_deref())?;
    let spawns_path = opts.spawns.clone().or_else(|| default_spawns_path(&db));
    let spawn_points = match &spawns_path {
        Some(path) => load_spawn_points(path)?,
        None => HashMap::new(),
    };
    println!(
        "[ai] {} natural NPC spawn template(s) from {}",
        spawn_points.len(),
        spawns_path
            .as_ref()
            .map_or_else(|| "(none)".to_string(), |p| p.display().to_string())
    );
    let arena = opts.arena.unwrap_or(game.state.self_pos);
    println!(
        "[ai] arena {arena:?}, character obj {}",
        game.state.self_obj
    );
    let mut s = Sweep {
        game,
        obs: Obs::new(),
        db,
        lines: Vec::new(),
        gm_used: Vec::new(),
        spawned: Vec::new(),
        spawn_points,
    };
    s.gm("godmode true", Duration::from_millis(400)).await?;
    s.gm("level 55", Duration::from_millis(600)).await?;
    s.gm("revive", Duration::from_millis(400)).await?;
    s.gm("heal", Duration::from_millis(300)).await?;
    if arena != s.game.state.self_pos {
        s.teleport(arena).await?;
    }

    let wanted: Vec<String> = if opts.all || opts.tests.is_empty() {
        (1..=10).map(|i| format!("s{i}")).collect()
    } else {
        opts.tests.clone()
    };
    for test in wanted {
        let result = match test.as_str() {
            "s1" => s.test_s1().await,
            "s2" => s.test_s2().await,
            "s3" => s.test_s3().await,
            "s4" => s.test_s4().await,
            "s5" => s.test_s5().await,
            "s6" => s.test_s6().await,
            "s7" => s.test_s7().await,
            "s8" => s.test_s8().await,
            "s9" => s.test_s9().await,
            "s10" => s.test_s10().await,
            other => Err(anyhow::anyhow!("unknown test '{other}' (s1..s10)")),
        };
        if let Err(e) = result {
            eprintln!("[ai] test {test} aborted: {e:#}");
            s.inconclusive(
                &test,
                &test.to_uppercase(),
                0,
                format!("test aborted: {e:#}"),
            );
        }
    }

    let spawned = s.spawned.clone();
    if !opts.keep_npcs {
        for obj in spawned.into_iter().rev() {
            s.despawn_npc(obj).await;
        }
    }
    let _ = s.gm("godmode false", Duration::from_millis(300)).await;
    let _ = s.gm("heal", Duration::from_millis(300)).await;

    let report = AiReport { lines: s.lines };
    if let Some(path) = &opts.out {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
        for line in &report.lines {
            serde_json::to_writer(&mut f, line)?;
            f.write_all(b"\n")?;
        }
        f.flush()?;
        println!(
            "[ai] wrote {} lines to {}",
            report.lines.len(),
            path.display()
        );
    }
    if opts.summary {
        print!("{}", report.render_summary());
    }
    Ok(report)
}

// -------------------------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_and_thresholds() {
        assert_eq!(classify_distance(12.6, 12.75, 2.5), "matches");
        assert_eq!(classify_distance(8.0, 12.75, 2.5), "suspect");
        let samples = [(4.0, false), (9.0, true), (10.5, true), (12.0, false)];
        assert_eq!(threshold(&samples), Some(10.5));
        assert_eq!(threshold(&[]), None);
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![10.0, 2.0, 6.0]), Some(6.0));
        assert_eq!(median(vec![10.0, 2.0, 6.0, 8.0]), Some(7.0));
    }

    #[test]
    fn movement_direction_dots() {
        let start = (0.0, 0.0, 0.0);
        let threat = (0.0, -4.0, 0.0);
        // Moving +Y is away from a threat at -Y.
        assert!((away_dot((0.0, 5.0, 0.0), start, threat) - 1.0).abs() < 1e-5);
        // Moving -Y is towards it.
        assert!((away_dot((0.0, -5.0, 0.0), start, threat) + 1.0).abs() < 1e-5);
        let home = (0.0, 10.0, 0.0);
        assert!((toward_dot((0.0, 5.0, 0.0), start, home) - 1.0).abs() < 1e-5);
        assert!((away_dot((0.0, 0.0, 0.0), start, threat)).abs() < 1e-5);
    }

    #[test]
    fn jump_and_spawn_behavior_parsing() {
        let samples = [
            (0, (0.0, 0.0, 0.0)),
            (1000, (0.5, 0.0, 0.0)),
            (2000, (18.0, 0.0, 0.0)),
        ];
        assert!((biggest_jump(&samples) - 17.5).abs() < 1e-4);
        assert_eq!(
            parse_behavior(
                "Using AI: HoldPosition, CurrentBehavior: v2.Behaviors.Common.SpawningBehavior"
            ),
            Some("Spawning".to_string())
        );
        assert_eq!(parse_behavior("nothing"), None);
    }

    #[test]
    fn spawn_point_parser_handles_jsonc() {
        let text = r#"[
            {
                "UnitId": 10008, // Bloodmist watcher
                "Position": {
                    "X": 11428.989,
                    "Y": 13988.21,
                    "Z": 431.27222
                }
            },
            {
                "UnitId": 528,
                "Position": { "X": 10548.683, "Y": 11752.29, "Z": 162.23685 }
            },
            {
                "UnitId": 529,
                "Position": {
                    "X": 10528.7,
                    "Y": 11672.99
                }
            }
        ]"#;
        let points = parse_spawn_points(text);
        assert_eq!(
            points.get(&10008),
            Some(&vec![(11428.989, 13988.21, 431.27222)])
        );
        assert_eq!(points.get(&528).map(Vec::len), Some(1));
        assert_eq!(points.get(&529), Some(&vec![(10528.7, 11672.99, 0.0)]));
    }

    #[test]
    fn param_parsing() {
        let p = "alertDuration = 10,\r\nalertToAttack = 3,\r\ncombatSkills = { melee = { 2 }, makeAGap = { 12340 } }, preferedCombatDist = 18, maxMakeAGapCount = 3,";
        assert_eq!(param_number(p, "alertDuration"), Some(10.0));
        assert_eq!(param_number(p, "preferedCombatDist"), Some(18.0));
        assert_eq!(preferred_combat_dist(p), Some(18.0));
        assert_eq!(
            spurt_skill("onSpurtSkill = { { skillType = 14048, healthCondition = 50 }, },"),
            Some(14048)
        );
        assert_eq!(
            spurt_health_condition(
                "onSpurtSkill = { { skillType = 14048, healthCondition = 50 }, },"
            ),
            Some(50.0)
        );
        assert_eq!(spurt_skill("minAPS = 20"), None);
    }

    #[test]
    fn target_changed_packet_body_is_read() {
        use crate::game::{Packet, LEVEL_GAME};
        let p = Packet {
            level: LEVEL_GAME,
            opcode: sc::SCTargetChangedPacket,
            body: vec![0x4f, 0x9d, 0x00, 0xed, 0x5f, 0x00],
        };
        match parse_event(&p) {
            Some(Ev::TargetChanged { npc, target }) => {
                assert_eq!(npc, 40271);
                assert_eq!(target, 24557);
            }
            other => panic!("unexpected event {other:?}"),
        }
        let p = Packet {
            level: LEVEL_GAME,
            opcode: sc::SCAggroTargetChanged,
            body: vec![0x4f, 0x9d, 0x00, 0xed, 0x5f, 0x00],
        };
        match parse_event(&p) {
            Some(Ev::Aggro { npc, target }) => {
                assert_eq!(npc, 40271);
                assert_eq!(target, 24557);
            }
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[test]
    fn event_windows() {
        let mut obs = Obs::new();
        obs.events = vec![
            ObsEvent {
                t_ms: 100,
                kind: Ev::TargetChanged { npc: 7, target: 3 },
            },
            ObsEvent {
                t_ms: 150,
                kind: Ev::SkillStarted {
                    skill: 2,
                    caster: 7,
                    target: 3,
                },
            },
            ObsEvent {
                t_ms: 200,
                kind: Ev::TargetChanged { npc: 7, target: 0 },
            },
            ObsEvent {
                t_ms: 300,
                kind: Ev::TargetChanged { npc: 8, target: 3 },
            },
        ];
        assert_eq!(obs.alert_onset(7, 3, 0, 1000), Some(100));
        assert_eq!(obs.alert_cleared(7, 100, 1000), Some(200));
        assert_eq!(
            obs.hostile_onset(7, 3, 0, 1000),
            Some((150, "skill_started".to_string()))
        );
        assert_eq!(obs.skill_starts(7, 0, 1000), vec![(150, 2)]);
        assert_eq!(obs.alert_onset(7, 3, 101, 1000), None);
    }
}
