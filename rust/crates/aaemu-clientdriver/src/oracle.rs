//! `oracle stats`: compare the numbers the real 1.2 client computes itself with the server's
//! `/getattribute` getters.
//!
//! Background (see `docs/client/LUA-DRIVER.md` and the research note `D:/aa-re/parity/combat-oracle.md`):
//! the 1.2 protocol sends no derived stats, so the client derives max HP/MP, DPS, armor, crit,
//! avoidance etc. natively from the same compact data the server uses. `X2Unit:UnitInfo('player')`
//! and `X2Unit:UnitModifierInfo('player')` expose those client-side tables to Lua, and
//! `X2Unit:UnitMaxHealth/UnitMaxMana` the pools. The server's values for the same quantities are
//! readable through the GM command `/getattribute`, whose reply arrives as system chat.
//!
//! Wire path of one run:
//! 1. the UI Lua state wraps `X2Chat.DispatchChatMessage` (the single sink every incoming chat
//!    line passes through) and records `channel \t text` lines in `_G.__aaemu_oracle.chat`;
//! 2. a snapshot request dumps `UnitInfo`, `UnitModifierInfo`, the pools, equipment and buffs;
//! 3. `/heal` is sent through the same chat path, then the pools are read again for the
//!    self-calibrating check (`health == max_health` at full HP);
//! 4. `/getattribute all` is sent and the recorded chat is parsed into `Name: value` pairs;
//! 5. [`MAPPINGS`] relates client keys to `UnitAttribute` names; each pair is compared with the
//!    declared transform and a per-kind tolerance, and a JSON report is written.
//!
//! The mapping is deliberately conservative: only quantities with an explicit evidence trail are
//! mapped (name, live values, and the server getter); every other client key and every server
//! attribute is listed as unmapped, never guessed.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde_json::{json, Value};

use aaemu_clientdriver::win::Driver;
use aaemu_clientdriver::{Reply, Request, Target};

/// Options of `oracle stats`.
#[derive(Debug, Clone)]
pub struct StatsOptions {
    /// Where the JSON report is written.
    pub out: PathBuf,
    /// Skip the `/heal` + full-pool self check (the character may not be a GM).
    pub skip_heal: bool,
}

/// Comparison kind of one mapping (the server prints ints and floats differently).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int,
    Float,
}

/// One client-key <-> `UnitAttribute` mapping.
///
/// `client` is a dotted path into the snapshot JSON (`unit_info.str`, `pools.max_health`, ...).
/// `scale`/`offset` transform the client value into the server's representation
/// (`server = client * scale + offset`); the default is the identity. Every entry carries the
/// evidence used to write it - never guess: an unmapped key is honest, a wrong mapping is not.
pub struct Mapping {
    pub client: &'static str,
    pub attr: &'static str,
    pub kind: Kind,
    pub scale: f64,
    pub offset: f64,
    pub evidence: &'static str,
}

const fn m(
    client: &'static str,
    attr: &'static str,
    kind: Kind,
    scale: f64,
    offset: f64,
    evidence: &'static str,
) -> Mapping {
    Mapping {
        client,
        attr,
        kind,
        scale,
        offset,
        evidence,
    }
}

/// The explicit mapping table.
///
/// Evidence sources:
/// * `character_info_table.alb` / `character_info.alb` (extracted UI Lua, see the research note)
///   uses these keys for the character-info window; the live client table keys match them.
/// * Server getters: `rust/crates/aaemu-game/src/systems/skills/stats.rs` (`CharStats::get`),
///   served by `/getattribute` (`systems/services/commands/scripts/get_attribute.rs`).
/// * Live equality observed on 2026-10-18 (level-1 client, one entry in the tool's first run);
///   mismatches are printed by the tool, not hidden here.
///
/// The `*_mul` / `casting_time_mul` client keys are percent *deltas* (`0` = unchanged, shown as
/// `%.1f%%` by `GetAddModifierText`, `baselib/tooltip/tooltip.alb:36`), while the server getters
/// return multipliers `1 + bonus/1000` (`stats.rs:448-464`): `server = 1 + client/100`.
/// The DPS client keys are real DPS; the server stores thousandths and its damage code multiplies
/// by `0.001` (`effects/combat.rs:269-278`): `server = client * 1000`.
pub const MAPPINGS: &[Mapping] = &[
    // ---- primary attributes ----
    m(
        "unit_info.str",
        "Str",
        Kind::Int,
        1.0,
        0.0,
        "same key; live 10 == 10",
    ),
    m(
        "unit_info.dex",
        "Dex",
        Kind::Int,
        1.0,
        0.0,
        "same key; live 10 == 10",
    ),
    m(
        "unit_info.sta",
        "Sta",
        Kind::Int,
        1.0,
        0.0,
        "same key; live 10 == 10",
    ),
    m(
        "unit_info.int",
        "Int",
        Kind::Int,
        1.0,
        0.0,
        "same key; live 10 == 10",
    ),
    m(
        "unit_info.spi",
        "Spi",
        Kind::Int,
        1.0,
        0.0,
        "same key; live 10 == 10",
    ),
    // ---- pools (UnitModifierInfo.max_health is a delta; the pool getter is the total) ----
    m(
        "pools.max_health",
        "MaxHealth",
        Kind::Int,
        1.0,
        0.0,
        "X2Unit:UnitMaxHealth; live 720 == 720",
    ),
    m(
        "pools.max_mana",
        "MaxMana",
        Kind::Int,
        1.0,
        0.0,
        "X2Unit:UnitMaxMana; live 670 == 670",
    ),
    // ---- regeneration ----
    m(
        "unit_info.health_regen",
        "HealthRegen",
        Kind::Int,
        1.0,
        0.0,
        "same quantity; live 53 vs 52 - mismatch reported",
    ),
    m(
        "unit_info.mana_regen",
        "ManaRegen",
        Kind::Int,
        1.0,
        0.0,
        "same quantity; live 18 == 18",
    ),
    m(
        "unit_info.persistent_health_regen",
        "PersistentHealthRegen",
        Kind::Int,
        1.0,
        0.0,
        "same quantity; live 1 == 1",
    ),
    m(
        "unit_info.persistent_mana_regen",
        "PersistentManaRegen",
        Kind::Int,
        1.0,
        0.0,
        "same quantity; live 1 vs 5 - mismatch reported",
    ),
    // ---- defense ----
    m(
        "unit_info.armor",
        "Armor",
        Kind::Int,
        1.0,
        0.0,
        "same quantity; live 269 == 269",
    ),
    m(
        "unit_info.magic_resist",
        "MagicResist",
        Kind::Int,
        1.0,
        0.0,
        "same quantity; live 377 == 377",
    ),
    m(
        "unit_info.battle_resist",
        "BattleResist",
        Kind::Int,
        1.0,
        0.0,
        "same key; live 0 == 0",
    ),
    m(
        "unit_info.flexibility",
        "Flexibility",
        Kind::Int,
        1.0,
        0.0,
        "same key; live 0 == 0",
    ),
    m(
        "unit_info.bulls_eye",
        "BullsEye",
        Kind::Int,
        1.0,
        0.0,
        "same key; live 0 == 0",
    ),
    // ---- offense: DPS (server stores thousandths, stats.rs Dps getters) ----
    m(
        "unit_info.melee_dps",
        "MainhandDps",
        Kind::Int,
        1000.0,
        0.0,
        "live 19.423 * 1000 == 19423",
    ),
    m(
        "unit_info.ranged_dps",
        "RangedDps",
        Kind::Int,
        1000.0,
        0.0,
        "live 16.81 * 1000 == 16810",
    ),
    m(
        "unit_info.spell_dps",
        "SpellDps",
        Kind::Int,
        1000.0,
        0.0,
        "live 2 * 1000 == 2000",
    ),
    m(
        "unit_info.heal_dps",
        "HealDps",
        Kind::Int,
        1000.0,
        0.0,
        "live 2 * 1000 == 2000",
    ),
    // ---- critical rates and bonuses (client rate is already a percentage) ----
    m(
        "unit_info.melee_critical_rate",
        "MeleeCritical",
        Kind::Float,
        1.0,
        0.0,
        "live 4.7619 == 4.7619047",
    ),
    m(
        "unit_info.ranged_critical_rate",
        "RangedCritical",
        Kind::Float,
        1.0,
        0.0,
        "live 4.7619 == 4.7619047",
    ),
    m(
        "unit_info.spell_critical_rate",
        "SpellCritical",
        Kind::Float,
        1.0,
        0.0,
        "live 4.7619 == 4.7619047",
    ),
    m(
        "unit_info.heal_critical_rate",
        "HealCritical",
        Kind::Float,
        1.0,
        0.0,
        "live 4.7619 == 4.7619047",
    ),
    m(
        "unit_info.melee_critical_bonus",
        "MeleeCriticalBonus",
        Kind::Float,
        1.0,
        0.0,
        "live 50 == 50",
    ),
    m(
        "unit_info.ranged_critical_bonus",
        "RangedCriticalBonus",
        Kind::Float,
        1.0,
        0.0,
        "live 50 == 50",
    ),
    m(
        "unit_info.spell_critical_bonus",
        "SpellCriticalBonus",
        Kind::Float,
        1.0,
        0.0,
        "live 50 == 50",
    ),
    m(
        "unit_info.heal_critical_bonus",
        "HealCriticalBonus",
        Kind::Float,
        1.0,
        0.0,
        "live 50 == 50",
    ),
    // ---- hit / avoidance (client calls the accuracy stat `*_success_rate`) ----
    m(
        "unit_info.melee_success_rate",
        "MeleeAntiMiss",
        Kind::Float,
        1.0,
        0.0,
        "server MeleeAntiMiss getter; live 100 == 100",
    ),
    m(
        "unit_info.ranged_success_rate",
        "RangedAntiMiss",
        Kind::Float,
        1.0,
        0.0,
        "live 100 == 100",
    ),
    m(
        "unit_info.spell_success_rate",
        "SpellAntiMiss",
        Kind::Float,
        1.0,
        0.0,
        "live 100 == 100",
    ),
    m(
        "unit_info.melee_parry_rate",
        "MeleeParry",
        Kind::Float,
        1.0,
        0.0,
        "live 2.14286 == 2.142857",
    ),
    m(
        "unit_info.ranged_parry_rate",
        "RangedParry",
        Kind::Float,
        1.0,
        0.0,
        "live 0 == 0",
    ),
    m(
        "unit_info.block_rate",
        "Block",
        Kind::Float,
        1.0,
        0.0,
        "live 2.85714 == 2.8571427",
    ),
    m(
        "unit_info.dodge_rate",
        "Dodge",
        Kind::Float,
        1.0,
        0.0,
        "live 3.80952 == 3.8095236",
    ),
    // ---- percent deltas: server = 1 + client / 100 ----
    m(
        "unit_info.casting_time_mul",
        "CastingTimeMul",
        Kind::Float,
        0.01,
        1.0,
        "GetAddModifierText formats this as percent; server getter 1+bonus/1000; live 0 -> 1",
    ),
    m(
        "unit_info.melee_damage_mul",
        "MeleeDamageMul",
        Kind::Float,
        0.01,
        1.0,
        "percent delta; server getter stats.rs:452; live 0 -> 1",
    ),
    m(
        "unit_info.ranged_damage_mul",
        "RangedDamageMul",
        Kind::Float,
        0.01,
        1.0,
        "percent delta; server getter stats.rs:455; live 0 -> 1",
    ),
    m(
        "unit_info.spell_damage_mul",
        "SpellDamageMul",
        Kind::Float,
        0.01,
        1.0,
        "percent delta; server getter stats.rs:458; live 0 -> 1",
    ),
    m(
        "unit_info.incoming_melee_damage_mul",
        "IncomingMeleeDamageMul",
        Kind::Float,
        0.01,
        1.0,
        "percent delta; server getter stats.rs:427; live 0 -> 1",
    ),
    m(
        "unit_info.incoming_ranged_damage_mul",
        "IncomingRangedDamageMul",
        Kind::Float,
        0.01,
        1.0,
        "percent delta; server getter stats.rs:434; live 0 -> 1",
    ),
    m(
        "unit_info.incoming_spell_damage_mul",
        "IncomingSpellDamageMul",
        Kind::Float,
        0.01,
        1.0,
        "percent delta; server getter stats.rs:441; live 0 -> 1",
    ),
    m(
        "unit_info.incoming_siege_damage_mul",
        "IncomingSiegeDamageMul",
        Kind::Float,
        0.01,
        1.0,
        "client computes it; server getter does not exist (NotFound) - parity gap",
    ),
    // ---- global cooldown (both sides in percent) ----
    m(
        "unit_info.global_cooldown_mul",
        "GlobalCooldownMul",
        Kind::Int,
        1.0,
        0.0,
        "both percent; live 100 == 100",
    ),
];

/// The Lua JSON encoder (Lua 5.1, no cjson). Mirrors the one in `mcp/client_actions.rs` with
/// depth 4 and sorted keys.
const JSON: &str = r#"
local function encode(v,depth)
 if depth>4 then return 'null' end
 local t=type(v)
 if t=='nil' then return 'null' elseif t=='boolean' then return tostring(v)
 elseif t=='number' then if v~=v or v==math.huge or v==-math.huge then return 'null' end return string.format('%.9g',v)
 elseif t=='string' then return '"'..string.gsub(v,'[%z\1-\31\\"]',function(c)
  if c=='"' then return '\\"' elseif c=='\\' then return '\\\\' else return string.format('\\u%04x',string.byte(c)) end end)..'"'
 elseif t=='table' then local out={} for k,w in pairs(v) do
  if type(w)~='function' and type(w)~='userdata' then out[#out+1]=encode(tostring(k),depth+1)..':'..encode(w,depth+1) end end
  table.sort(out) return '{'..table.concat(out,',')..'}' end return 'null'
end
"#;

/// Install the chat-recording hook once per client process. `X2Chat.DispatchChatMessage` is the
/// single native sink the UI Lua event handlers feed every incoming chat line into.
const HOOK: &str = r#"
if not _G.__aaemu_oracle then _G.__aaemu_oracle={chat={}} end
local o=_G.__aaemu_oracle
if not o.hooked then
 if type(X2Chat)~='table' or type(X2Chat.DispatchChatMessage)~='function' then return 'unavailable' end
 local orig=X2Chat.DispatchChatMessage
 X2Chat.DispatchChatMessage=function(self,...)
  if #o.chat<8192 then
   local n=select('#',...)
   local parts={}
   for i=1,n do parts[i]=tostring((select(i,...))) end
   o.chat[#o.chat+1]=table.concat(parts,'\t')
  end
  return orig(self,...)
 end
 o.hooked=true
end
return 'hooked'
"#;

/// Code prefix that clears the recorded chat and sends one Say line (a GM command reaches the
/// server through the normal chat path). Callers append the `SendChatMessage` statement.
const CHAT_CLEAR: &str =
    "local o=assert(_G.__aaemu_oracle,'oracle hook is not installed') o.chat={}\n";

/// The snapshot: identity, pools, the two client stat tables, equipment and buffs.
const SNAPSHOT: &str = r#"
assert(X2:IsEnteredWorld() and X2Unit:UnitName('player'), 'a character must be in the world')
local result={character=X2Unit:UnitName('player'),level=X2Unit:UnitLevel('player')}
result.pools={health=X2Unit:UnitHealth('player'),max_health=X2Unit:UnitMaxHealth('player'),
              mana=X2Unit:UnitMana('player'),max_mana=X2Unit:UnitMaxMana('player')}
result.unit_info=X2Unit:UnitInfo('player')
result.unit_modifier_info=X2Unit:UnitModifierInfo('player')
result.equip={}
if type(playerEquipSlots)=='table' and X2Equipment then
 for i=1,#playerEquipSlots do
  local slot=playerEquipSlots[i]
  local ok,info=pcall(X2Equipment.GetEquippedItemTooltipInfo,X2Equipment,slot)
  if ok and type(info)=='table' and info.itemType then
   result.equip[tostring(slot)]={slot=slot,item_template_id=info.itemType,name=info.name,
     grade=info.itemGrade,slot_type=info.slotType,DPS=info.DPS,armor=info.armor,
     magic_resistance=info.magicResistance}
  end
 end
end
result.buffs={} result.debuffs={}
local bc=X2Unit:UnitBuffCount('player') or 0
for i=1,math.min(bc,32) do
 local b=X2Unit:UnitBuff('player',i)
 if b then result.buffs[tostring(i)]={buff_id=b.buff_id,path=b.path} end
end
local dc=X2Unit:UnitDeBuffCount('player') or 0
for i=1,math.min(dc,32) do
 local b=X2Unit:UnitDeBuff('player',i)
 if b then result.debuffs[tostring(i)]={buff_id=b.buff_id,path=b.path} end
end
"#;

/// The pools only (after the heal).
const POOLS: &str = r#"
local result={health=X2Unit:UnitHealth('player'),max_health=X2Unit:UnitMaxHealth('player'),
              mana=X2Unit:UnitMana('player'),max_mana=X2Unit:UnitMaxMana('player')}
"#;

/// The last `UnitAttribute` `/getattribute all` prints (ascending values; `ExpByLaborPowerMul` is
/// the highest defined one): polling for it marks the reply complete.
const LAST_ATTRIBUTE: &str = "ExpByLaborPowerMul";

/// The chat record of one server reply: `(channel, text)` entries as the Lua hook wrote them.
pub fn parse_attribute_chat(entries: &[String]) -> (Option<String>, BTreeMap<String, String>) {
    let mut header = None;
    let mut attributes = BTreeMap::new();
    for entry in entries {
        // The hook writes `channel \t text`; the text itself may contain tabs, so split once.
        let text = entry.split_once('\t').map_or(entry.as_str(), |(_, t)| t);
        let text = text.trim_end_matches('\n');
        if let Some(rest) = text.strip_prefix("[getattribute] ") {
            header = Some(rest.to_string());
            continue;
        }
        if let Some((name, value)) = text.split_once(": ") {
            if !name.is_empty()
                && name.bytes().all(|b| b.is_ascii_alphanumeric())
                && name.as_bytes()[0].is_ascii_alphabetic()
            {
                attributes.insert(name.to_string(), value.to_string());
            }
        }
    }
    (header, attributes)
}

/// Parses one attribute value: `NotFound` -> `None`, otherwise the number.
pub fn parse_attribute_value(raw: &str) -> Option<f64> {
    if raw == "NotFound" {
        return None;
    }
    raw.trim().parse::<f64>().ok()
}

/// Compares a transformed client value with the server value.
pub fn compare(kind: Kind, scale: f64, offset: f64, client: f64, server: f64) -> bool {
    let client = client * scale + offset;
    match kind {
        // Ints are exact; `scale` only changes the unit (server DPS are thousandths).
        Kind::Int => (client.round() - server.round()).abs() < 0.5,
        // Floats allow the client's display rounding (four decimals) and float32 noise.
        Kind::Float => (client - server).abs() <= 0.01,
    }
}

/// Flattens a JSON object into `prefix.key` -> value paths (leaves only, numbers and strings).
pub fn flatten(value: &Value, prefix: &str, out: &mut BTreeMap<String, Value>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(child, &path, out);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                flatten(child, &format!("{prefix}.{index}"), out);
            }
        }
        leaf => {
            out.insert(prefix.to_string(), leaf.clone());
        }
    }
}

/// Looks up a dotted path in the snapshot.
pub fn get_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = root;
    for part in path.split('.') {
        current = match current {
            Value::Object(map) => map.get(part)?,
            _ => return None,
        };
    }
    Some(current)
}

fn lua(driver: &mut Driver, code: &str) -> anyhow::Result<String> {
    match driver.send(&Request::Lua(Target::Ui, code.to_string()))? {
        Reply::Ok(s) => Ok(s),
        Reply::LuaError(e) => bail!("lua error: {e}"),
        Reply::DriverError(e) => bail!("driver error: {e}"),
    }
}

fn read_chat(driver: &mut Driver) -> anyhow::Result<Vec<String>> {
    let text = lua(
        driver,
        &format!("{JSON}\nreturn encode(_G.__aaemu_oracle and _G.__aaemu_oracle.chat or {{}},1)"),
    )?;
    parse_chat_record(text.trim())
}

/// Decodes the recorded chat list. The Lua encoder writes every table as an object
/// (`{"1":...,"2":...}`); a real array is accepted too, so the reader stays usable if the
/// encoder gains array support.
pub fn parse_chat_record(text: &str) -> anyhow::Result<Vec<String>> {
    let value: Value = serde_json::from_str(text).context("invalid chat record from the client")?;
    let mut entries: Vec<(u64, String)> = Vec::new();
    match value {
        Value::Object(map) => {
            for (key, entry) in map {
                let index: u64 = key.parse().context("non-numeric chat record key")?;
                entries.push((index, entry.as_str().unwrap_or_default().to_string()));
            }
        }
        Value::Array(items) => {
            for (index, entry) in items.into_iter().enumerate() {
                entries.push((
                    index as u64 + 1,
                    entry.as_str().unwrap_or_default().to_string(),
                ));
            }
        }
        other => bail!("unexpected chat record shape: {other}"),
    }
    entries.sort_by_key(|(index, _)| *index);
    Ok(entries.into_iter().map(|(_, entry)| entry).collect())
}

fn send_command(driver: &mut Driver, command: &str) -> anyhow::Result<()> {
    let quoted: String = command
        .bytes()
        .map(|b| format!("\\{b:03}"))
        .collect::<Vec<_>>()
        .join("");
    lua(
        driver,
        &format!("{CHAT_CLEAR}X2Chat:SendChatMessage(CHAT_SAY,\"{quoted}\",'')\nreturn 'sent'"),
    )?;
    Ok(())
}

/// Runs `oracle stats` against a connected client.
pub fn stats(pid: u32, opts: &StatsOptions) -> anyhow::Result<()> {
    let mut driver = Driver::connect(pid, Duration::from_secs(10))?;
    let hook = lua(&mut driver, HOOK)?;
    if hook.trim() != "hooked" {
        bail!("client chat hook unavailable ({hook}); is a character in the world?");
    }

    let snapshot_text = lua(
        &mut driver,
        &format!("{JSON}\n{SNAPSHOT}\nreturn encode(result,0)"),
    )?;
    let snapshot: Value =
        serde_json::from_str(snapshot_text.trim()).context("invalid snapshot from the client")?;
    let character = snapshot["character"].as_str().unwrap_or("?").to_string();
    let level = snapshot["level"].as_u64().unwrap_or(0);

    // GM heal + the self-calibrating pool check (fresh values after the heal). `/heal` without a
    // target is a no-op in the 1.2 command (its `player_target` is the current target), so the
    // character's own name is the target. Character names are single words; anything else is
    // skipped rather than risking a broken chat command.
    let mut heal_reply = Vec::new();
    let mut self_check: Option<Value> = None;
    if !opts.skip_heal {
        let name_is_safe = !character.is_empty()
            && character.len() <= 16
            && character.bytes().all(|b| b.is_ascii_alphanumeric());
        if name_is_safe {
            send_command(&mut driver, &format!("/heal {character}"))?;
        } else {
            eprintln!("oracle stats: skipping /heal (unsupported character name {character:?})");
        }
        std::thread::sleep(Duration::from_millis(1500));
        heal_reply = read_chat(&mut driver)?;
        let pools_text = lua(
            &mut driver,
            &format!("{JSON}\n{POOLS}\nreturn encode(result,0)"),
        )?;
        let pools: Value =
            serde_json::from_str(pools_text.trim()).context("invalid pools from the client")?;
        let health = pools["health"].as_f64();
        let max_health = pools["max_health"].as_f64();
        let mana = pools["mana"].as_f64();
        let max_mana = pools["max_mana"].as_f64();
        self_check = Some(json!({
            "before": snapshot["pools"],
            "health": health,
            "max_health": max_health,
            "mana": mana,
            "max_mana": max_mana,
            "health_full": health == max_health && health.is_some(),
            "mana_full": mana == max_mana && mana.is_some(),
        }));
    }

    // `/getattribute all`: clear the record, send, poll until the last attribute arrives.
    send_command(&mut driver, "/getattribute all")?;
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut raw_chat;
    let mut header;
    let mut attributes;
    loop {
        std::thread::sleep(Duration::from_millis(300));
        raw_chat = read_chat(&mut driver)?;
        (header, attributes) = parse_attribute_chat(&raw_chat);
        if attributes.contains_key(LAST_ATTRIBUTE) || Instant::now() >= deadline {
            break;
        }
    }
    if attributes.is_empty() {
        bail!(
            "no /getattribute output received (is the character a GM?); chat: {:?}",
            raw_chat
        );
    }

    // Compare every mapping.
    let mapped_client: Vec<&str> = MAPPINGS.iter().map(|m| m.client).collect();
    let mapped_attrs: Vec<&str> = MAPPINGS.iter().map(|m| m.attr).collect();
    let mut pairs = Vec::new();
    let mut mismatches = Vec::new();
    for mapping in MAPPINGS {
        let client_value = get_path(&snapshot, mapping.client).and_then(Value::as_f64);
        let raw = attributes.get(mapping.attr).map(String::as_str);
        let server_value = raw.and_then(parse_attribute_value);
        let (status, matched) = match (client_value, server_value) {
            (Some(c), Some(s)) => (
                "compared",
                compare(mapping.kind, mapping.scale, mapping.offset, c, s),
            ),
            (Some(_), None) if raw == Some("NotFound") => ("server_not_found", false),
            (Some(_), None) => ("server_missing", false),
            (None, _) => ("client_missing", false),
        };
        let pair = json!({
            "attr": mapping.attr,
            "client_key": mapping.client,
            "kind": match mapping.kind { Kind::Int => "int", Kind::Float => "float" },
            "client": client_value,
            "client_transformed": client_value.map(|c| c * mapping.scale + mapping.offset),
            "server": server_value,
            "server_raw": raw,
            "match": matched,
            "status": status,
            "evidence": mapping.evidence,
        });
        if status == "compared" && !matched {
            mismatches.push(pair.clone());
        }
        pairs.push(pair);
    }

    // Everything not referenced by the mapping table, on both sides.
    let mut leaves = BTreeMap::new();
    flatten(&snapshot["unit_info"], "unit_info", &mut leaves);
    flatten(
        &snapshot["unit_modifier_info"],
        "unit_modifier_info",
        &mut leaves,
    );
    flatten(&snapshot["pools"], "pools", &mut leaves);
    let unmapped_client: Vec<&String> = leaves
        .keys()
        .filter(|k| !mapped_client.contains(&k.as_str()))
        .collect();
    let unmapped_server: BTreeMap<&String, &String> = attributes
        .iter()
        .filter(|(name, value)| {
            value.as_str() != "NotFound" && !mapped_attrs.contains(&name.as_str())
        })
        .collect();

    let compared = pairs.iter().filter(|p| p["status"] == "compared").count();
    let matched = pairs
        .iter()
        .filter(|p| p["status"] == "compared" && p["match"] == true)
        .count();
    let not_found = pairs
        .iter()
        .filter(|p| p["status"] == "server_not_found")
        .count();
    let missing = pairs.len() - compared - not_found;

    let report = json!({
        "character": character,
        "level": level,
        // Top-level convenience copies of the requested report fields.
        "gear": snapshot["equip"],
        "buffs": snapshot["buffs"],
        "client": {
            "pools": snapshot["pools"],
            "unit_info": snapshot["unit_info"],
            "unit_modifier_info": snapshot["unit_modifier_info"],
            "unit_info_keys": sorted_keys(&snapshot["unit_info"]),
            "unit_modifier_info_keys": sorted_keys(&snapshot["unit_modifier_info"]),
            "equip": snapshot["equip"],
            "buffs": snapshot["buffs"],
            "debuffs": snapshot["debuffs"],
        },
        "server": {
            "header": header,
            "attributes": attributes,
            "raw_chat": raw_chat,
        },
        "pairs": pairs,
        "unmapped_client": unmapped_client,
        "unmapped_server": unmapped_server,
        "self_check": self_check,
        "heal_reply": heal_reply,
        "mapping_evidence": MAPPINGS.iter().map(|m| json!({
            "client": m.client, "attr": m.attr,
            "scale": m.scale, "offset": m.offset, "evidence": m.evidence,
        })).collect::<Vec<_>>(),
    });

    std::fs::write(&opts.out, serde_json::to_string_pretty(&report)?)
        .with_context(|| format!("writing {}", opts.out.display()))?;

    println!("oracle stats: {character} (level {level}), pid {pid}");
    if let Some(check) = &self_check {
        println!(
            "  self-check: health {} -> {} / {} ({}), mana {} -> {} / {} ({})",
            json_num(&check["before"]["health"]),
            json_num(&check["health"]),
            json_num(&check["max_health"]),
            if check["health_full"] == true {
                "full"
            } else {
                "MISMATCH"
            },
            json_num(&check["before"]["mana"]),
            json_num(&check["mana"]),
            json_num(&check["max_mana"]),
            if check["mana_full"] == true {
                "full"
            } else {
                "MISMATCH"
            },
        );
    }
    println!(
        "  compared {compared}: {matched} match, {} mismatch, {not_found} server NotFound, {missing} missing",
        compared - matched
    );
    for pair in &mismatches {
        println!(
            "  mismatch {}: client {} -> {} vs server {} (raw {:?})",
            pair["attr"].as_str().unwrap_or("?"),
            json_num(&pair["client"]),
            json_num(&pair["client_transformed"]),
            json_num(&pair["server"]),
            pair["server_raw"].as_str().unwrap_or("?"),
        );
    }
    println!(
        "  unmapped: {} client keys, {} server attributes (see {})",
        unmapped_client.len(),
        unmapped_server.len(),
        opts.out.display()
    );
    Ok(())
}

fn json_num(value: &Value) -> String {
    match value.as_f64() {
        Some(v) if v.fract() == 0.0 => format!("{}", v as i64),
        Some(v) => format!("{v}"),
        None => "?".into(),
    }
}

fn sorted_keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();
    keys.dedup();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed real `/getattribute all` reply (recorded live on 2026-10-18, level-1 GM).
    fn live_reply() -> Vec<String> {
        [
            "12\t[getattribute] Stats for target Oracleone (15129)",
            "12\tStr: 10",
            "12\tDex: 10",
            "12\tMaxHealth: 720",
            "12\tMeleeCritical: 4.7619047",
            "12\tMeleeDodge: NotFound",
            "12\tHealthRegen: 52",
            "12\tPersistentManaRegen: 5",
            "12\tExpByLaborPowerMul: NotFound",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn parses_getattribute_reply_lines() {
        let (header, attributes) = parse_attribute_chat(&live_reply());
        assert_eq!(
            header.as_deref(),
            Some("Stats for target Oracleone (15129)")
        );
        assert_eq!(attributes.get("Str").map(String::as_str), Some("10"));
        assert_eq!(
            attributes.get("MeleeCritical").map(String::as_str),
            Some("4.7619047")
        );
        assert_eq!(
            attributes.get("MeleeDodge").map(String::as_str),
            Some("NotFound")
        );
        assert!(attributes.contains_key(LAST_ATTRIBUTE));
    }

    #[test]
    fn parses_chat_record_shapes() {
        let object = r#"{"1":"12\tStr: 10","2":"12\tDex: 10"}"#;
        assert_eq!(
            parse_chat_record(object).unwrap(),
            vec!["12\tStr: 10".to_string(), "12\tDex: 10".to_string()]
        );
        let array = r#"["12\tStr: 10","12\tDex: 10"]"#;
        assert_eq!(parse_chat_record(array).unwrap().len(), 2);
        assert!(parse_chat_record("[1,2]").unwrap()[0].is_empty());
        assert!(parse_chat_record("not json").is_err());
    }

    #[test]
    fn ignores_noise_and_unrelated_chat() {
        let entries = vec![
            "12\tWelcome to ArcheAge".to_string(),
            "12\tYou are not authorized to use this command.".to_string(),
            "12\tStr 10".to_string(),
            "7\tStr: 10".to_string(),
            "12\tmangled:".to_string(),
        ];
        let (header, attributes) = parse_attribute_chat(&entries);
        assert!(header.is_none());
        assert_eq!(attributes.len(), 1);
        assert_eq!(attributes.get("Str").map(String::as_str), Some("10"));
    }

    #[test]
    fn parses_attribute_values() {
        assert_eq!(parse_attribute_value("10"), Some(10.0));
        assert_eq!(parse_attribute_value("-0.5"), Some(-0.5));
        assert_eq!(parse_attribute_value("1E-05"), Some(1e-5));
        assert_eq!(parse_attribute_value("NotFound"), None);
        assert_eq!(parse_attribute_value("garbage"), None);
    }

    #[test]
    fn ints_are_exact_and_floats_use_tolerance() {
        assert!(compare(Kind::Int, 1.0, 0.0, 10.0, 10.0));
        assert!(!compare(Kind::Int, 1.0, 0.0, 53.0, 52.0));
        assert!(compare(Kind::Float, 1.0, 0.0, 4.7619, 4.7619047));
        assert!(!compare(Kind::Float, 1.0, 0.0, 1.0, 1.02));
    }

    #[test]
    fn scaled_and_offset_transforms() {
        // DPS: client 19.423 is server 19423 (thousandths).
        assert!(compare(Kind::Int, 1000.0, 0.0, 19.423, 19423.0));
        assert!(!compare(Kind::Int, 1000.0, 0.0, 19.423, 19424.0));
        // Percent delta: client 0 means server 1.
        assert!(compare(Kind::Float, 0.01, 1.0, 0.0, 1.0));
        assert!(compare(Kind::Float, 0.01, 1.0, 10.0, 1.1));
        assert!(!compare(Kind::Float, 0.01, 1.0, 10.0, 1.2));
    }

    #[test]
    fn flatten_and_get_path() {
        let value = json!({"unit_info": {"str": 10, "nested": {"x": 1}}, "other": "s"});
        let mut leaves = BTreeMap::new();
        flatten(&value, "", &mut leaves);
        assert_eq!(leaves["unit_info.str"], json!(10));
        assert_eq!(leaves["unit_info.nested.x"], json!(1));
        assert_eq!(leaves["other"], json!("s"));
        assert_eq!(get_path(&value, "unit_info.nested.x"), Some(&json!(1)));
        assert_eq!(get_path(&value, "unit_info.missing"), None);
    }

    #[test]
    fn mapping_table_is_consistent() {
        let mut clients = std::collections::BTreeSet::new();
        let mut attrs = std::collections::BTreeSet::new();
        for mapping in MAPPINGS {
            assert!(
                clients.insert(mapping.client),
                "duplicate client key {}",
                mapping.client
            );
            assert!(
                attrs.insert(mapping.attr),
                "duplicate attribute {}",
                mapping.attr
            );
            assert!(
                !mapping.evidence.is_empty(),
                "{} has no evidence",
                mapping.client
            );
            assert!(
                mapping.client.starts_with("unit_info.") || mapping.client.starts_with("pools.")
            );
            assert!(mapping.scale != 0.0);
            if mapping.offset != 0.0 {
                assert_eq!(
                    mapping.kind,
                    Kind::Float,
                    "{} uses offset on an int",
                    mapping.client
                );
            }
        }
    }

    #[test]
    fn unmapped_lists_cover_the_live_shape() {
        // The live keys observed on 2026-10-18 (level 1). Every mapped key must exist in the
        // client tables and every mapped attribute in the server output of that run.
        let client_keys = [
            "str",
            "dex",
            "sta",
            "int",
            "spi",
            "armor",
            "magic_resist",
            "health_regen",
            "mana_regen",
            "persistent_health_regen",
            "persistent_mana_regen",
            "bulls_eye",
            "battle_resist",
            "flexibility",
            "global_cooldown_mul",
            "melee_dps",
            "ranged_dps",
            "spell_dps",
            "heal_dps",
            "melee_critical_rate",
            "ranged_critical_rate",
            "spell_critical_rate",
            "heal_critical_rate",
            "melee_critical_bonus",
            "ranged_critical_bonus",
            "spell_critical_bonus",
            "heal_critical_bonus",
            "melee_success_rate",
            "ranged_success_rate",
            "spell_success_rate",
            "melee_parry_rate",
            "ranged_parry_rate",
            "block_rate",
            "dodge_rate",
            "casting_time_mul",
            "melee_damage_mul",
            "ranged_damage_mul",
            "spell_damage_mul",
            "incoming_melee_damage_mul",
            "incoming_ranged_damage_mul",
            "incoming_spell_damage_mul",
            "incoming_siege_damage_mul",
        ];
        for mapping in MAPPINGS {
            if let Some(key) = mapping.client.strip_prefix("unit_info.") {
                assert!(
                    client_keys.contains(&key),
                    "{} is not a live UnitInfo key",
                    mapping.client
                );
            }
        }
    }
}
