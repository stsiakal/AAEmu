-- AAEmu probe core: passive event/snapshot recorder. It only reads state.
-- The sole writes are the addon's own observation files (api.File:Write), its
-- own settings and its own window.

local U = require("aaemu_probe/util")
local G = require("aaemu_probe/getters")
local D = require("aaemu_probe/derive")
local EVENT_NAMES = require("aaemu_probe/events")

local Probe = {}
Probe.__index = Probe

local VERSION = "0.2.0"
local SCHEMA = "aaemu_probe.records"
local FLUSH_MS = 15000
local TARGET_POLL_MS = 1000
local BAG_PERIOD_MS = 60000
local BAG_DEBOUNCE_MS = 1000
local CONTEXT_TTL_MS = 200
local MAX_BUFFER = 500
local RATE_WINDOW_MS = 10000
local RATE_MAX = 40
local MAX_BAG_SLOTS = 400
local MAX_BUFFS = 40
local EVENT_LIMITS = {max_depth = 3, max_keys = 40, max_string = 300, max_nodes = 600}
local KILL_CORRELATE_MS = 5000
local LOOT_WINDOW_MS = 3000
local OPEN_WINDOW_MS = 10000
local CONTAINER_GRACE_MS = 250
local REWARD_WINDOW_MS = 5000
local QUEST_ID_TTL_MS = 60000
local LAST_TARGET_TTL_MS = 30000
local MAX_NPC_POSITIONS = 20

-- api.On events documented in the API file (UPDATE is the frame tick).
local API_ON_EVENTS = {
  "CHAT_MESSAGE", "TEAM_MEMBERS_CHANGED", "UI_RELOADED", "UPDATE_PING_INFO",
}
local BAG_EVENTS = {BAG_UPDATE = true, ADDED_ITEM = true, REMOVED_ITEM = true}
-- Noise observed live in 0.1.1: counted in the summary, never stored raw.
local IGNORED_EVENTS = {END_QUEST_CHAT_BUBBLE = true, TARGET_OVER = true}

local function lib(api, name)
  local value
  local ok = pcall(function() value = api[name] end)
  if ok then return value end
  return nil
end

-- The addon sandbox has no setmetatable, select, _G, getfenv, error or os.
-- Objects are plain tables with the class methods copied in.
local function adopt(object, class)
  for key, value in pairs(class) do
    if object[key] == nil then object[key] = value end
  end
  return object
end

-- Number of arguments up to the last non-nil one (no select in the sandbox).
local function arg_count(args)
  local top = 0
  for key in pairs(args) do
    if type(key) == "number" and key > top then top = key end
  end
  return top
end

function Probe:_boot(step, err, extra)
  local hook = self.helpers and self.helpers.boot
  if type(hook) == "function" then pcall(hook, step, err, extra) end
end

function Probe.new(api, helpers)
  local self = adopt({
    api = api,
    helpers = helpers or {},
    alive = true,
    active = true,
    paused = false,
    buffer = {},
    seq = 0,
    chunk = 0,
    files_written = 0,
    last_path = nil,
    last_error = nil,
    events_seen = 0,
    counts = {},
    distinct = 0,
    suppressed = {},
    suppressed_total = 0,
    rate = {},
    missed_while_paused = 0,
    registered_ok = 0,
    registered_fail = 0,
    since_poll = 0,
    since_flush = 0,
    since_bag = 0,
    bag_dirty_in = nil,
    last_bag_sig = nil,
    last_target = nil,
    ctx = nil,
    ctx_ms = nil,
    in_world_inventory_done = false,
    session = "0000",
    ui = nil,
    since_delayed = 0,
    delayed_done = false,
    kind_counts = {},
    quests = {},
    last_quest_id = nil,
    last_quest_ms = nil,
    combat = false,
    combat_ms = nil,
    combat_target = nil,
    combat_units = {},
    pending_kill = nil,
    pending_exp = nil,
    last_kill = nil,
    last_open = nil,
    pending_container = nil,
    pending_reward = nil,
    last_money = nil,
    last_exp = nil,
    last_target_fp = nil,
    last_target_fp_ms = nil,
    pets = {},
    pet_absent = {},
    npcs = {},
    items = {},
  }, Probe)
  self:_open_session()
  self:_record_start()
  self:_register_events()
  self:_boot("3", self.last_error, {
    registered_ok = self.registered_ok, registered_fail = self.registered_fail})
  self:_register_api_on()
  self:_run_inventory("load")
  self:_snapshot_bag("load")
  return self
end

-- Small API wrappers ---------------------------------------------------------

function Probe:_now()
  local result = U.call_method(nil, lib(self.api, "Time"), "GetUiMsec")
  local value = U.first(result)
  if type(value) == "number" then return value end
  return 0
end

function Probe:_local_time()
  return U.first(U.call_method(EVENT_LIMITS, lib(self.api, "Time"), "GetLocalTime"))
end

function Probe:_set_error(text)
  self.last_error = U.truncate_utf8(tostring(text), 200)
end

-- Session -------------------------------------------------------------------

function Probe:_open_session()
  local number = nil
  local ok, settings = pcall(self.api.GetSettings, "aaemu_probe")
  if ok and type(settings) == "table" then
    number = (tonumber(settings.next_session) or 0) + 1
    settings.next_session = number
    local saved, err = pcall(self.api.SaveSettings)
    if not saved then
      self:_set_error("SaveSettings failed: " .. tostring(err))
    end
  end
  if number then
    self.session = string.format("%04d", number)
  else
    self.session = "t" .. tostring(self:_now())
  end
end

-- Buffer and storage --------------------------------------------------------

function Probe:_add(kind, fields, force)
  if not self.alive then return false end
  if not force and not self.active then return false end
  if #self.buffer >= MAX_BUFFER and not force then
    self.paused = true
    self.missed_while_paused = self.missed_while_paused + 1
    return false
  end
  self.seq = self.seq + 1
  local record = fields or {}
  record.k = kind
  record.seq = self.seq
  record.ms = self:_now()
  self.buffer[#self.buffer + 1] = record
  self.kind_counts[kind] = (self.kind_counts[kind] or 0) + 1
  return true
end

function Probe:_summary_record()
  local counts, suppressed, kinds = {}, {}, {}
  for name, count in pairs(self.counts) do counts[name] = count end
  for name, count in pairs(self.suppressed) do suppressed[name] = count end
  for name, count in pairs(self.kind_counts) do kinds[name] = count end
  return {
    events_seen = self.events_seen, distinct = self.distinct,
    counts = counts, suppressed = suppressed, kinds = kinds,
    suppressed_total = self.suppressed_total,
    missed_while_paused = self.missed_while_paused,
    registered_ok = self.registered_ok, registered_fail = self.registered_fail,
  }
end

-- Writes the whole buffer as one chunk. On failure the buffer is kept and
-- retried; nothing is dropped.
function Probe:flush(reason)
  if #self.buffer == 0 then return true end
  local chunk = self.chunk + 1
  local summary = self:_summary_record()
  summary.k = "summary"
  self.seq = self.seq + 1
  summary.seq = self.seq
  summary.ms = self:_now()
  summary.reason = reason
  local records = {}
  for index, record in ipairs(self.buffer) do records[index] = record end
  records[#records + 1] = summary
  local path = string.format("aaemu_probe/observations/probe_%s_%03d.lua",
    self.session, chunk)
  local payload = {
    schema = SCHEMA, version = 1, probe_version = VERSION,
    session = self.session, chunk = chunk, count = #records, records = records,
  }
  local ok, result = pcall(self.api.File.Write, self.api.File, path, payload)
  if not ok or (result ~= nil and result ~= true) then
    self:_set_error("write failed (" .. path .. "): " .. tostring(result))
    return false
  end
  self.chunk = chunk
  self.files_written = self.files_written + 1
  self.last_path = path
  self.buffer = {}
  self.last_error = nil
  if self.paused then
    self.paused = false
    self:_add("resume", {missed = self.missed_while_paused,
      note = "buffer flushed; logging resumed"}, true)
  end
  return true
end

function Probe:save()
  return self:flush("save")
end

-- Context ---------------------------------------------------------------------

function Probe:_context()
  local now = self:_now()
  if self.ctx and self.ctx_ms and now - self.ctx_ms < CONTEXT_TTL_MS then
    return self.ctx
  end
  local api = self.api
  local unit = lib(api, "Unit")
  local ctx = {}
  local pos = U.call_method(EVENT_LIMITS, unit, "UnitWorldPosition", "player")
  if pos.ok then ctx.pos = pos.values else ctx.pos_error = pos.error end
  local zone = U.call_method(EVENT_LIMITS, unit, "GetCurrentZoneGroup")
  if zone.ok then ctx.zone = zone.values[1] else ctx.zone_error = zone.error end
  local id = U.first(U.call_method(EVENT_LIMITS, unit, "GetUnitId", "target"))
  if id ~= nil then
    ctx.target = {
      id = id,
      name = U.first(U.call_method(EVENT_LIMITS, unit, "UnitName", "target")),
      info = U.first(U.call_method(EVENT_LIMITS, unit, "UnitInfo", "target")),
    }
  end
  ctx.local_time = self:_local_time()
  self.ctx = ctx
  self.ctx_ms = now
  return ctx
end

-- Events ----------------------------------------------------------------------

function Probe:_note_event(name)
  self.events_seen = self.events_seen + 1
  local count = self.counts[name]
  if not count then
    self.distinct = self.distinct + 1
    count = 0
  end
  self.counts[name] = count + 1
end

function Probe:_rate_ok(name, now)
  local slot = self.rate[name]
  if not slot or now - slot.start >= RATE_WINDOW_MS then
    slot = {start = now, count = 0}
    self.rate[name] = slot
  end
  slot.count = slot.count + 1
  if slot.count > RATE_MAX then
    self.suppressed[name] = (self.suppressed[name] or 0) + 1
    self.suppressed_total = self.suppressed_total + 1
    return false
  end
  return true
end

function Probe:on_event(source, name, ...)
  if not self.alive or not self.active then return end
  if type(name) ~= "string" then name = tostring(name) end
  self:_note_event(name)
  if BAG_EVENTS[name] then self.bag_dirty_in = BAG_DEBOUNCE_MS end
  if name == "ENTERED_WORLD" and not self.in_world_inventory_done then
    self.in_world_inventory_done = true
    self:_run_inventory("entered_world")
  end
  local now = self:_now()
  local raw = {...}
  if IGNORED_EVENTS[name] then return end
  -- Derived records are computed before the per-event rate limit so a burst of
  -- raw events cannot hide a kill, loot or quest transition.
  local derived_ok, derived_err = pcall(self._handle_derived, self, name, raw, now)
  if not derived_ok then
    self:_set_error("derive " .. name .. ": " .. tostring(derived_err))
  end
  if not self:_rate_ok(name, now) then return end
  local count = arg_count(raw)
  local budget = {nodes = EVENT_LIMITS.max_nodes}
  local args = {}
  for index = 1, count do
    args[index] = U.sanitize(raw[index], EVENT_LIMITS, 0, nil, budget)
  end
  local ctx = self:_context()
  self:_add("event", {
    e = name, src = source, n = count, args = args,
    ui_ms = now, lt = ctx.local_time, pos = ctx.pos, pos_error = ctx.pos_error,
    zone = ctx.zone, target = ctx.target,
  })
end

-- Derived records -------------------------------------------------------------

local function combat_active(value)
  if type(value) == "boolean" then return value end
  if type(value) == "number" then return value ~= 0 end
  if type(value) == "string" then
    if value == "true" then return true end
    if value == "false" then return false end
    local number = tonumber(value)
    if number ~= nil then return number ~= 0 end
  end
  return nil
end

-- Unit ids arrive as hex strings in events but may be numbers in getters;
-- compare them as lowercase hex text.
local function id_key(value)
  if value == nil then return nil end
  if type(value) == "number" then return string.format("%x", value) end
  if type(value) == "string" then
    local number = tonumber(value)
    if number ~= nil then return string.format("%x", number) end
    return value:lower()
  end
  return tostring(value)
end

function Probe:_unit_position(token, id)
  local unit = lib(self.api, "Unit")
  local result = U.call_method(EVENT_LIMITS, unit, "UnitWorldPosition", token)
  if (not result.ok or #(result.values or {}) < 3) and id ~= nil then
    result = U.call_method(EVENT_LIMITS, unit, "GetUnitWorldPosition", id)
  end
  if result.ok and type(result.values) == "table" and #result.values >= 3 then
    return result.values
  end
  return nil
end

local function info_filled(info)
  return type(info) == "table"
    and (info.name ~= nil or info.level ~= nil or info.faction ~= nil
      or info.type ~= nil)
end

-- No template id is exposed for units, so this is the best stable identity:
-- name + level + faction + max health. Positions are attached for catalogues.
function Probe:_fingerprint(token, id)
  local unit = lib(self.api, "Unit")
  local info = U.first(U.call_method(EVENT_LIMITS, unit, "UnitInfo", token))
  -- UnitInfo is empty for NPCs; the by-id getter still has level/faction.
  if not info_filled(info) and id ~= nil then
    local by_id = U.first(U.call_method(EVENT_LIMITS, unit, "GetUnitInfoById", id))
    if info_filled(by_id) or type(info) ~= "table" then info = by_id end
  end
  local name = U.first(U.call_method(EVENT_LIMITS, unit, "UnitName", token))
  if name == nil and id ~= nil then
    name = U.first(U.call_method(EVENT_LIMITS, unit, "GetUnitNameById", id))
  end
  local level = U.first(U.call_method(EVENT_LIMITS, unit, "UnitLevel", token))
  local faction = U.first(U.call_method(EVENT_LIMITS, unit, "GetFactionName", token))
  local max_health = U.first(U.call_method(EVENT_LIMITS, unit, "UnitMaxHealth", token))
  if type(info) == "table" then
    if name == nil then name = info.name end
    if level == nil then level = info.level end
    if faction == nil then faction = info.faction end
    if max_health == nil then max_health = info.maxHealth or info.max_health end
  end
  local fp = {
    id = id, name = name, level = level, faction = faction,
    max_health = max_health,
    unit_type = type(info) == "table" and info.type or nil,
    position = self:_unit_position(token, id),
  }
  fp.key = D.fingerprint_key(fp)
  return fp
end

function Probe:_snapshot_fingerprint(now)
  local unit = lib(self.api, "Unit")
  local id = U.first(U.call_method(nil, unit, "GetUnitId", "target"))
  if id ~= nil then return self:_fingerprint("target", id) end
  if self.last_target_fp and now - (self.last_target_fp_ms or 0)
     <= LAST_TARGET_TTL_MS then
    return self.last_target_fp
  end
  return nil
end

function Probe:_observe_quest(qid, event, now)
  qid = tonumber(qid)
  if qid == nil or qid <= 0 then return end
  local quest = lib(self.api, "Quest")
  local readings, errors = {}, {}
  for _, name in ipairs(G.QUEST_GETTERS) do
    local result = U.call_method(EVENT_LIMITS, quest, name, qid)
    readings[name] = U.first(result)
    if not result.ok then errors[name] = result.error end
  end
  local state = {
    completed = readings.IsCompleted,
    active_title = readings.GetActiveQuestTitle,
    main_title = readings.GetQuestContextMainTitle,
    body = readings.GetQuestContextBody,
  }
  local entry = self.quests[qid]
  local transition = D.quest_transition(entry and entry.state or nil, state)
  local observations = (entry and entry.observations or 0) + 1
  self.quests[qid] = {
    state = state, ms = now, observations = observations,
    transition = transition,
  }
  self.last_quest_id = qid
  self.last_quest_ms = now
  self:_add("quest_state", {
    qid = qid, event = event, n = observations,
    changed = transition ~= "unchanged", transition = transition,
    state = state,
    prev = (transition ~= "unchanged") and entry and entry.state or nil,
    getter_errors = errors,
  })
end

function Probe:_handle_derived(name, raw, now)
  if name == "CHAT_MSG_QUEST" then
    self:_observe_quest(raw[9], name, now)
  elseif name == "QUEST_LEFT_TIME_UPDATED" then
    self:_observe_quest(raw[1], name, now)
  elseif name == "UNIT_COMBAT_STATE_CHANGED" then
    self:_on_combat_state(raw, now)
  elseif name == "EXP_CHANGED" then
    self:_on_exp(raw, now)
  elseif name == "ADDED_ITEM" then
    self:_on_added_item(raw, now)
  elseif name == "PLAYER_MONEY" then
    self:_on_money(raw, now)
  elseif name == "REMOVED_ITEM" then
    self:_on_removed_item(raw, now)
  elseif name == "SPELLCAST_START" then
    self:_on_cast(raw, now)
  elseif name == "DYNAMIC_ACTION_BAR_SHOW" then
    self:_on_action_bar(raw, now)
  end
end

function Probe:_on_combat_state(raw, now)
  local active = combat_active(raw[1])
  if active == nil then return end
  local key = id_key(raw[2])
  if active then
    if key ~= nil then self.combat_units[key] = now end
    self.combat = true
    self.combat_ms = now
    self.combat_target = self:_snapshot_fingerprint(now)
    return
  end
  -- Only a unit we saw enter combat, the player, or the current target can
  -- end a fight; unrelated unit updates must not invent kills.
  local unit = lib(self.api, "Unit")
  local player_key = id_key(U.first(U.call_method(nil, unit, "GetUnitId", "player")))
  local target_key = id_key(self.last_target)
  local known = key ~= nil and self.combat_units[key] ~= nil
  if not known and (key == nil or (key ~= player_key and key ~= target_key)) then
    return
  end
  if key ~= nil then self.combat_units[key] = nil end
  self.combat = false
  local fp = self:_snapshot_fingerprint(now)
  self.pending_kill = {
    ms = now, unit = raw[2], fp = fp,
    pos = (type(fp) == "table" and fp.position) or self:_context().pos,
  }
  self.combat_target = nil
  if type(fp) == "table" then self:_catalogue_npc(fp, now, "combat_end") end
  if self.pending_exp and now - self.pending_exp.ms <= KILL_CORRELATE_MS then
    self.pending_kill.exp = self.pending_exp.amount
    self.pending_kill.exp_ms = self.pending_exp.ms
    self.pending_exp = nil
    self:_emit_kill(now)
  end
end

function Probe:_emit_kill(now)
  local pending = self.pending_kill
  if not pending then return end
  local fp = pending.fp
  local position = (type(fp) == "table" and fp.position) or pending.pos
  self.last_kill = {
    ms = now, fp = fp, pos = position, exp = pending.exp,
  }
  self.pending_kill = nil
  self:_add("kill", {
    exp = pending.exp, exp_ms = pending.exp_ms,
    combat_unit = pending.unit, elapsed_ms = now - pending.ms,
    target_id = type(fp) == "table" and fp.id or nil,
    npc_key = type(fp) == "table" and fp.key or nil,
    fingerprint = fp, position = position, pos = self:_context().pos,
  })
end

function Probe:_on_exp(raw, now)
  local amount = tonumber(raw[2])
  if amount ~= nil then self.last_exp = {ms = now, amount = amount} end
  if self.pending_reward and now - self.pending_reward.ms <= REWARD_WINDOW_MS then
    self.pending_reward.exp = amount
    self:_maybe_finish_reward(now)
  end
  if amount == nil or amount <= 0 then return end
  if self.pending_kill and now - self.pending_kill.ms <= KILL_CORRELATE_MS then
    self.pending_kill.exp = amount
    self.pending_kill.exp_ms = now
    self:_emit_kill(now)
  elseif self.combat == true then
    -- EXP may arrive before the combat-end event; keep it for that window.
    self.pending_exp = {
      ms = now, amount = amount, unit = raw[1],
      fp = self:_snapshot_fingerprint(now), pos = self:_context().pos,
    }
  end
end

function Probe:_find_bag_item(item_type)
  local bag = lib(self.api, "Bag")
  local capacity = U.first(U.call_method(EVENT_LIMITS, bag, "Capacity"))
  local limit = type(capacity) == "number" and math.min(capacity, MAX_BAG_SLOTS) or 0
  for slot = 1, limit do
    local result = U.call_method(EVENT_LIMITS, bag, "GetBagItemInfo", 1, slot)
    if result.ok then
      local info = result.values[1]
      if type(info) == "table" then
        local found = tonumber(info.itemType or info.itemTypeId or info.type
          or info.item_type_id)
        if found == item_type then return info, slot end
      end
    end
  end
  return nil
end

function Probe:_catalogue_item(item_type, now)
  if item_type == nil then return end
  local entry = self.items[item_type]
  if entry and entry.name ~= nil then return end
  local info, slot = self:_find_bag_item(item_type)
  if info == nil or info.name == nil then
    if entry == nil then self.items[item_type] = {first_ms = now} end
    return
  end
  entry = {
    item_type_id = item_type, name = info.name, grade = info.grade,
    category = info.category, stack = info.stack, cost = info.cost,
    slot = slot, first_ms = entry and entry.first_ms or now,
  }
  self.items[item_type] = entry
  self:_add("item_catalogue", entry)
end

-- Every bag snapshot also feeds the item catalogue, so an item that was
-- added before its bag update is filled in on the next snapshot.
function Probe:_catalogue_bag_items(items)
  if type(items) ~= "table" then return end
  local now = self:_now()
  for slot, info in pairs(items) do
    if type(info) == "table" then
      local item_type = tonumber(info.itemType or info.itemTypeId or info.type
        or info.item_type_id)
      if item_type ~= nil then
        local entry = self.items[item_type]
        if entry == nil then
          entry = {
            item_type_id = item_type, name = info.name, grade = info.grade,
            category = info.category, stack = info.stack, cost = info.cost,
            slot = slot, first_ms = now,
          }
          self.items[item_type] = entry
          self:_add("item_catalogue", entry)
        elseif entry.name == nil and info.name ~= nil then
          entry.name = info.name
          entry.grade = info.grade
          entry.category = info.category
          entry.stack = info.stack
          entry.cost = info.cost
          entry.slot = slot
          self:_add("item_catalogue", entry)
        end
      end
    end
  end
end

function Probe:_on_added_item(raw, now)
  local link = raw[1]
  local item_type = D.item_type_from_link(link)
  local count = tonumber(raw[2])
  local source = tonumber(raw[3])
  self:_catalogue_item(item_type, now)
  local ctx
  if source == 10 and self.last_kill
     and now - self.last_kill.ms <= LOOT_WINDOW_MS then
    ctx = self:_context()
    self:_add("loot", {
      item_type_id = item_type, count = count, link = link,
      npc_key = type(self.last_kill.fp) == "table" and self.last_kill.fp.key or nil,
      npc = self.last_kill.fp, kill_ms = self.last_kill.ms,
      kill_pos = self.last_kill.pos, pos = ctx.pos, zone = ctx.zone,
      elapsed_ms = now - self.last_kill.ms,
    })
  end
  if source == 35 then
    if not self.pending_reward then
      ctx = ctx or self:_context()
      local qid = self.last_quest_id
      if qid ~= nil and self.last_quest_ms ~= nil
         and now - self.last_quest_ms > QUEST_ID_TTL_MS then
        qid = nil
      end
      self.pending_reward = {
        ms = now, quest_id = qid, items = {}, pos = ctx.pos, zone = ctx.zone,
        money = nil, exp = nil,
      }
      -- Money and exp can arrive in the same tick just before the item.
      if self.last_money and now - self.last_money.ms <= 1000 then
        self.pending_reward.money = self.last_money.delta
      end
      if self.last_exp and now - self.last_exp.ms <= 1000 then
        self.pending_reward.exp = self.last_exp.amount
      end
    end
    self.pending_reward.items[#self.pending_reward.items + 1] = {
      item_type_id = item_type, count = count, link = link,
    }
    self:_maybe_finish_reward(now)
  end
  if self.pending_container and now == self.pending_container.consume_ms then
    self.pending_container.adds[#self.pending_container.adds + 1] = {
      item_type_id = item_type, count = count, link = link, source = source,
    }
  end
end

function Probe:_on_money(raw, now)
  local delta = tonumber(raw[1])
  if delta ~= nil then self.last_money = {ms = now, delta = delta} end
  if not self.pending_reward then return end
  if now - self.pending_reward.ms > REWARD_WINDOW_MS then return end
  self.pending_reward.money = delta
  self:_maybe_finish_reward(now)
end

function Probe:_maybe_finish_reward(now)
  local pending = self.pending_reward
  if not pending then return end
  if pending.money ~= nil and pending.exp ~= nil then
    self:_emit_reward(now)
  end
end

function Probe:_emit_reward(now)
  local pending = self.pending_reward
  if not pending then return end
  self.pending_reward = nil
  local quest_entry = pending.quest_id and self.quests[pending.quest_id]
  self:_add("quest_reward", {
    quest_id = pending.quest_id, items = pending.items,
    money_delta = pending.money, exp_delta = pending.exp,
    elapsed_ms = now - pending.ms, pos = pending.pos, zone = pending.zone,
    quest = quest_entry and quest_entry.state or nil,
  })
end

function Probe:_on_cast(raw, now)
  local name = raw[1]
  if type(name) ~= "string" or name:lower() ~= "open" then return end
  local ctx = self:_context()
  self.last_open = {
    ms = now, cast = {name = name, args = {raw[2], raw[3], raw[4]}},
    pos = ctx.pos, zone = ctx.zone,
  }
end

function Probe:_on_removed_item(raw, now)
  local reason = raw[3]
  if type(reason) ~= "string" or reason:lower() ~= "consume" then return end
  local open = self.last_open
  if not open or now - open.ms > OPEN_WINDOW_MS then return end
  self.last_open = nil
  local ctx = self:_context()
  self.pending_container = {
    open_ms = open.ms, open_cast = open.cast, open_pos = open.pos,
    consume_ms = now, consumed = {
      item_type_id = D.item_type_from_link(raw[1]),
      count = tonumber(raw[2]), link = raw[1], source = raw[4],
    },
    adds = {}, pos = open.pos or ctx.pos, zone = open.zone or ctx.zone,
  }
end

function Probe:_on_action_bar(raw, now)
  local fp = self:_snapshot_fingerprint(now)
  local ctx = self:_context()
  local target_id = type(fp) == "table" and fp.id
    or (ctx.target and ctx.target.id) or nil
  self:_add("action_bar", {
    kind = raw[1], n = raw[2], target_id = target_id,
    npc_key = type(fp) == "table" and fp.key or nil,
    fingerprint = fp, pos = ctx.pos, zone = ctx.zone,
  })
  if type(fp) == "table" then
    self:_catalogue_npc(fp, now, "action_bar", raw[2])
  end
end

function Probe:_catalogue_npc(fp, now, origin, action_bar)
  if type(fp) ~= "table" or fp.key == nil then return end
  if fp.name == nil and fp.max_health == nil then return end
  local key = fp.key
  local entry = self.npcs[key]
  local created = entry == nil
  local changed = created
  if created then
    entry = {
      first_ms = now, positions = {}, action_bars = {}, ids = {}, sightings = 0,
      fingerprint = {
        name = fp.name, level = fp.level, faction = fp.faction,
        max_health = fp.max_health, unit_type = fp.unit_type,
      },
    }
    self.npcs[key] = entry
  end
  entry.sightings = entry.sightings + 1
  entry.last_ms = now
  entry.last_id = fp.id
  if fp.id ~= nil then
    local tag = tostring(fp.id)
    local known_id = false
    for _, saved in ipairs(entry.ids) do
      if tostring(saved) == tag then known_id = true; break end
    end
    if not known_id and #entry.ids < MAX_NPC_POSITIONS then
      entry.ids[#entry.ids + 1] = fp.id
      changed = true
    end
  end
  if type(fp.position) == "table" then
    local seen = false
    for _, saved in ipairs(entry.positions) do
      if D.position_same(saved.pos, fp.position) then seen = true; break end
    end
    if not seen and #entry.positions < MAX_NPC_POSITIONS then
      entry.positions[#entry.positions + 1] = {
        pos = fp.position, ms = now, origin = origin,
      }
      changed = true
    end
  end
  if action_bar ~= nil then
    local tag = tostring(action_bar)
    local found = false
    for _, value in ipairs(entry.action_bars) do
      if tostring(value) == tag then found = true; break end
    end
    if not found then
      entry.action_bars[#entry.action_bars + 1] = action_bar
      changed = true
    end
  end
  if changed then
    self:_add("npc_catalogue", {
      key = key, fingerprint = entry.fingerprint, positions = entry.positions,
      action_bars = entry.action_bars, ids = entry.ids,
      sightings = entry.sightings, first_ms = entry.first_ms,
      last_ms = entry.last_ms, origin = origin, last_id = entry.last_id,
    })
  end
end

-- Flushes records that need a short window to collect their parts (container
-- adds, quest reward money and exp, stale pending correlations).
function Probe:_flush_derived(now)
  local container = self.pending_container
  if container and now - container.consume_ms > CONTAINER_GRACE_MS then
    self.pending_container = nil
    self:_add("container_open", {
      open_cast = container.open_cast, open_ms = container.open_ms,
      consume_ms = container.consume_ms, consumed = container.consumed,
      adds = container.adds, add_count = #container.adds,
      elapsed_ms = container.consume_ms - container.open_ms,
      pos = container.pos, zone = container.zone,
    })
  end
  if self.pending_reward and now - self.pending_reward.ms > REWARD_WINDOW_MS then
    self:_emit_reward(now)
  end
  if self.pending_exp and now - self.pending_exp.ms > KILL_CORRELATE_MS then
    self.pending_exp = nil
  end
  if self.pending_kill and now - self.pending_kill.ms > KILL_CORRELATE_MS then
    self.pending_kill = nil
  end
  if self.last_kill and now - self.last_kill.ms > LOOT_WINDOW_MS * 4 then
    self.last_kill = nil
  end
  if self.last_open and now - self.last_open.ms > OPEN_WINDOW_MS then
    self.last_open = nil
  end
  for key, since in pairs(self.combat_units) do
    if now - since > 300000 then self.combat_units[key] = nil end
  end
end

local function make_widget_handler(self)
  return function(a, b, ...)
    -- Handlers receive (widget, event, ...); tolerate (event, ...) too.
    if type(a) == "string" then
      self:on_event("widget", a, b, ...)
    else
      self:on_event("widget", b, ...)
    end
  end
end

function Probe:_register_events()
  local api = self.api
  local iface = lib(api, "Interface")
  local made, window = false, nil
  if iface then
    made, window = pcall(iface.CreateEmptyWindow, iface,
      "aaemu_probe_events", "UIParent")
  end
  if not made or type(window) ~= "table" then
    self:_set_error("event window unavailable: " .. tostring(window))
    self:_add("registrations", {error = "no event window"}, true)
    return
  end
  self.event_window = window
  -- Same shape as AACstats: 1x1 hidden sink anchored at the screen corner.
  pcall(window.SetExtent, window, 1, 1)
  pcall(window.AddAnchor, window, "TOPLEFT", "UIParent", 0, 0)
  local names, seen = {}, {}
  local runtime = self.helpers.ui_events
  local runtime_count = 0
  if type(runtime) == "table" then
    for key, value in pairs(runtime) do
      runtime_count = runtime_count + 1
      local name = type(value) == "string" and value or key
      if type(name) == "string" and not seen[name] then
        seen[name] = true
        names[#names + 1] = name
      end
    end
  end
  for _, name in ipairs(EVENT_NAMES) do
    if not seen[name] then
      seen[name] = true
      names[#names + 1] = name
    end
  end
  table.sort(names)
  local handler = make_widget_handler(self)
  local handler_ok, handler_err = pcall(window.SetHandler, window, "OnEvent", handler)
  local ok_names, failed = {}, {}
  for _, name in ipairs(names) do
    local ok, ret = pcall(window.RegisterEvent, window, name)
    if ok then
      ok_names[#ok_names + 1] = name
      self.registered_ok = self.registered_ok + 1
    else
      failed[name] = U.truncate_utf8(tostring(ret), 160)
      self.registered_fail = self.registered_fail + 1
    end
  end
  pcall(window.Show, window, false)
  self:_add("registrations", {
    mechanism = "widget RegisterEvent + SetHandler(OnEvent)",
    handler_ok = handler_ok,
    handler_error = (not handler_ok) and tostring(handler_err) or nil,
    requested = #names, runtime_UI_EVENTS_count = runtime_count,
    ok_count = #ok_names, fail_count = self.registered_fail,
    registered = ok_names, failed = failed,
    note = "RegisterEvent does not report loader blocks; blocked events " ..
      "return normally and never fire; compare registered with counts.",
  }, true)
end

function Probe:_register_api_on()
  local api = self.api
  local results = {}
  for _, name in ipairs(API_ON_EVENTS) do
    local event = name
    local ok, err = pcall(api.On, event, function(...)
      self:on_event("api.On", event, ...)
    end)
    results[event] = ok and "ok" or U.truncate_utf8(tostring(err), 160)
  end
  local ok, err = pcall(api.On, "UPDATE", function(dt) self:update(dt) end)
  results.UPDATE = ok and "ok" or U.truncate_utf8(tostring(err), 160)
  if not ok then self:_set_error("api.On UPDATE failed: " .. tostring(err)) end
  self:_add("registrations_api_on", {results = results}, true)
end

-- Inventory ---------------------------------------------------------------------

local function entry_args(entry, extra)
  if extra ~= nil then return {extra} end
  local args = {}
  for index = 3, #entry do args[#args + 1] = entry[index] end
  return args
end

function Probe:_call_entry(entry, extra)
  local name = entry[2]
  if not G.is_read_only(name) then
    return {ok = false, error = "refused: not a read-only name"}
  end
  local args = entry_args(entry, extra)
  return U.call_method(EVENT_LIMITS, lib(self.api, entry[1]), name, unpack(args))
end

function Probe:_surface_records()
  local api = self.api
  local libs, names = {}, {}
  for _, libname in ipairs(G.SURFACE_LIBS) do
    local target = lib(api, libname)
    local entry = {present = target ~= nil, kind = type(target)}
    if type(target) == "table" then
      local methods = {}
      pcall(function()
        for key, value in pairs(target) do
          if type(key) == "string" then methods[key] = type(value) end
        end
      end)
      entry.members = methods
    end
    libs[libname] = entry
  end
  pcall(function()
    for key, value in pairs(api) do
      if type(key) == "string" then names[key] = type(value) end
    end
  end)
  local globals = {}
  for key, value in pairs(self.helpers.globals or {}) do globals[key] = value end
  return libs, names, globals
end

-- The sandbox has no `next`, so libraries cannot be enumerated; probe the
-- fixed documented/known names instead and record which of them exist.
function Probe:_member_records()
  local api = self.api
  local present, members, missing = {}, {}, {}
  for libname, names in pairs(G.SURFACE_NAMES) do
    local target = lib(api, libname)
    present[libname] = type(target)
    local found, absent = {}, {}
    if target ~= nil then
      for _, name in ipairs(names) do
        local ok, value = pcall(function() return target[name] end)
        if ok and value ~= nil then
          found[name] = type(value)
        else
          absent[#absent + 1] = name
        end
      end
    else
      for _, name in ipairs(names) do absent[#absent + 1] = name end
    end
    members[libname] = found
    missing[libname] = absent
  end
  return present, members, missing
end

function Probe:_run_inventory(phase)
  local libs, names, globals = self:_surface_records()
  self:_add("api_surface", {phase = phase, libs = libs, top_level = names}, true)
  self:_add("api_globals", {phase = phase, globals = globals}, true)
  local present, members, missing = self:_member_records()
  local found_count, missing_count = 0, 0
  for _, found in pairs(members) do
    for _ in pairs(found) do found_count = found_count + 1 end
  end
  for _, absent in pairs(missing) do missing_count = missing_count + #absent end
  self:_add("api_members", {
    phase = phase, present = present, members = members, missing = missing,
    found_count = found_count, missing_count = missing_count,
  }, true)
  local calls = {}
  local function push(entry, extra)
    local result = self:_call_entry(entry, extra)
    calls[#calls + 1] = {
      lib = entry[1], fn = entry[2], args = entry_args(entry, extra), r = result,
    }
  end
  for _, entry in ipairs(G.INVENTORY) do push(entry) end
  for slot = 1, G.EQUIP_SLOTS do
    push({"Equipment", "GetEquippedItemTooltipInfo"}, slot)
  end
  local zone = U.first(U.call_method(
    EVENT_LIMITS, lib(self.api, "Unit"), "GetCurrentZoneGroup"))
  if zone ~= nil then
    for _, entry in ipairs(G.ZONE_CALLS) do push(entry, zone) end
  end
  self:_add("api_inventory", {phase = phase, count = #calls, calls = calls}, true)
end

-- Bag snapshot ------------------------------------------------------------------

function Probe:_snapshot_bag(reason)
  if not self.alive or not self.active then return end
  local api = self.api
  local bag = lib(api, "Bag")
  local capacity = U.first(U.call_method(EVENT_LIMITS, bag, "Capacity"))
  local count = U.first(U.call_method(EVENT_LIMITS, bag, "CountItems"))
  local currency = U.call_method(EVENT_LIMITS, bag, "GetCurrency")
  local bank_currency = U.call_method(EVENT_LIMITS, lib(api, "Bank"), "GetCurrency")
  local items, errors, filled = {}, 0, 0
  local limit = type(capacity) == "number" and math.min(capacity, MAX_BAG_SLOTS) or 0
  local first_error
  for slot = 1, limit do
    local result = U.call_method(EVENT_LIMITS, bag, "GetBagItemInfo", 1, slot)
    if result.ok then
      local info = result.values[1]
      if info ~= nil then
        items[slot] = info
        filled = filled + 1
      end
    else
      errors = errors + 1
      first_error = first_error or result.error
    end
  end
  self:_catalogue_bag_items(items)
  local sig = U.signature({items, currency.values, bank_currency.values})
  local record = {
    reason = reason, capacity = capacity, count = count, filled = filled,
    currency = currency.ok and currency.values[1] or nil,
    currency_error = currency.error,
    bank_currency = bank_currency.ok and bank_currency.values[1] or nil,
    slot_errors = errors, first_error = first_error,
  }
  if self.last_bag_sig == sig then
    record.unchanged = true
  else
    record.items = items
    self.last_bag_sig = sig
  end
  self:_add("bag", record)
end

-- Target ------------------------------------------------------------------------

local function scan_hints(value, path, out, depth)
  if type(value) ~= "table" or depth > 3 then return end
  for key, item in pairs(value) do
    local label = path .. "." .. tostring(key)
    if type(key) == "string" then
      local lowered = key:lower()
      if lowered:find("id", 1, true) or lowered:find("type", 1, true)
         or lowered:find("templ", 1, true) or lowered:find("kind", 1, true)
         or lowered:find("grade", 1, true) then
        if type(item) ~= "table" then out[label] = item end
      end
    end
    scan_hints(item, label, out, depth + 1)
  end
end

function Probe:_buff_list(count_name, fn_name, token)
  token = token or "target"
  local unit = lib(self.api, "Unit")
  local count = U.first(U.call_method(EVENT_LIMITS, unit, count_name, token))
  local list = {}
  if type(count) == "number" then
    for index = 1, math.min(count, MAX_BUFFS) do
      list[index] = U.call_method(EVENT_LIMITS, unit, fn_name, token, index)
    end
  end
  return {count = count, list = list}
end

function Probe:_dump_unit(kind, token, id, extra)
  local calls, by_id = {}, {}
  for _, entry in ipairs(G.TARGET) do
    calls[entry[1] .. "." .. entry[2]] = self:_call_entry({entry[1], entry[2], token})
  end
  for _, entry in ipairs(G.TARGET_BY_ID) do
    by_id[entry[1] .. "." .. entry[2]] = self:_call_entry({entry[1], entry[2]}, id)
  end
  local hints = {}
  for key, result in pairs(calls) do
    if result.ok and result.values then scan_hints(result.values, key, hints, 0) end
  end
  for key, result in pairs(by_id) do
    if result.ok and result.values then scan_hints(result.values, key, hints, 0) end
  end
  local ctx = self:_context()
  local record = {
    id = id, token = token, calls = calls, by_id = by_id,
    buffs = self:_buff_list("UnitBuffCount", "UnitBuff", token),
    debuffs = self:_buff_list("UnitDeBuffCount", "UnitDeBuff", token),
    id_like_fields = hints, pos = ctx.pos, zone = ctx.zone,
    fingerprint = self:_fingerprint(token, id),
  }
  if extra then
    for key, value in pairs(extra) do record[key] = value end
  end
  self:_add(kind, record)
  return record
end

function Probe:_dump_target(id)
  local own_pet = nil
  for _, which in ipairs(G.PET_TOKENS) do
    if self.pets[which] == id then own_pet = which end
  end
  local now = self:_now()
  local record = self:_dump_unit("target", "target", id, {own_pet = own_pet})
  local fp = record.fingerprint
  self.last_target_fp = fp
  self.last_target_fp_ms = now
  if type(fp) == "table" then
    self:_catalogue_npc(fp, now, "target")
  end
end

-- Own mount/pet: dumped on every target change so its identity is recorded
-- even when it is not the current target (and can be matched to a target dump).
function Probe:_dump_pet(which)
  local id = U.first(U.call_method(nil, lib(self.api, "Unit"), "GetUnitId", which))
  if id == nil then
    if not self.pet_absent[which] then
      self.pet_absent[which] = true
      self:_add("pet_absent", {token = which})
    end
    self.pets[which] = nil
    return nil
  end
  self.pet_absent[which] = nil
  self.pets[which] = id
  return self:_dump_unit("pet", which, id, {})
end

function Probe:_poll_target()
  local id = U.first(U.call_method(nil, lib(self.api, "Unit"), "GetUnitId", "target"))
  if id == self.last_target then return end
  self.last_target = id
  self.ctx_ms = nil
  for _, which in ipairs(G.PET_TOKENS) do self:_dump_pet(which) end
  if id == nil then
    self:_add("target_cleared", {})
    return
  end
  self:_dump_target(id)
end

-- Frame tick ------------------------------------------------------------------------

function Probe:update(dt)
  if not self.alive then return end
  dt = tonumber(dt) or 0
  self.since_poll = self.since_poll + dt
  self.since_flush = self.since_flush + dt
  self.since_bag = self.since_bag + dt
  if self.active then self:_flush_derived(self:_now()) end
  if not self.delayed_done then
    self.since_delayed = self.since_delayed + dt
    if self.since_delayed >= 10000 then
      -- OnLoad can run before the character is in the world: repeat once.
      self.delayed_done = true
      self:_run_inventory("delayed_10s")
      self:_snapshot_bag("delayed_10s")
    end
  end
  if self.bag_dirty_in then
    self.bag_dirty_in = self.bag_dirty_in - dt
    if self.bag_dirty_in <= 0 then
      self.bag_dirty_in = nil
      self.since_bag = 0
      self:_snapshot_bag("bag_event")
    end
  end
  if self.since_poll >= TARGET_POLL_MS then
    self.since_poll = 0
    if self.active then self:_poll_target() end
    self:refresh_ui()
  end
  if self.since_bag >= BAG_PERIOD_MS then
    self.since_bag = 0
    self:_snapshot_bag("periodic")
  end
  if self.since_flush >= FLUSH_MS then
    self.since_flush = 0
    self:flush("interval")
    self:refresh_ui()
  end
end

-- Control -------------------------------------------------------------------------------

function Probe:set_active(active)
  if active == self.active then return end
  if not active then
    self:flush("stop")
    self.active = false
  else
    self.active = true
  end
  self:refresh_ui()
end

function Probe:toggle()
  self:set_active(not self.active)
  return self.active
end

function Probe:status_lines()
  local lines = {}
  lines[1] = string.format("Events seen %d  distinct %d  suppressed %d",
    self.events_seen, self.distinct, self.suppressed_total)
  lines[2] = string.format("Records buffered %d/%d  files written %d",
    #self.buffer, MAX_BUFFER, self.files_written)
  local state
  if self.paused then
    state = string.format("PAUSED (buffer full, %d missed) - press Save",
      self.missed_while_paused)
  elseif self.active then
    state = "Recording, session " .. self.session
  else
    state = "Stopped"
  end
  lines[3] = state
  lines[4] = "Last error: " .. (self.last_error or "none")
  return lines
end

function Probe:refresh_ui()
  if self.ui and self.ui.refresh then self.ui:refresh(self) end
end

function Probe:_record_start()
  self:_add("meta", {
    probe_version = VERSION, session = self.session,
    ui_ms = self:_now(), lt = self:_local_time(),
    has_global_UI_EVENTS = self.helpers.ui_events ~= nil,
    sandbox_globals = self.helpers.sandbox_globals,
    flush_ms = FLUSH_MS, max_buffer = MAX_BUFFER, rate_max = RATE_MAX,
    rate_window_ms = RATE_WINDOW_MS, context_ttl_ms = CONTEXT_TTL_MS,
    quest_count_available = false,
    quest_note = "API exposes only per-questTypeId getters; no active quest list. " ..
      "Quest getters run on first sight and on every later event for the id.",
    ignored_events = {"END_QUEST_CHAT_BUBBLE", "TARGET_OVER"},
    derive_note = "kill = combat end + EXP_CHANGED; loot = ADDED_ITEM source 10 " ..
      "within 3 s of a kill; container = Open cast + consume + same-ms adds; " ..
      "quest reward = ADDED_ITEM source 35 + PLAYER_MONEY + EXP_CHANGED.",
  }, true)
end

function Probe:destroy()
  if not self.alive then return end
  self:flush("unload")
  self.alive = false
  if self.ui and self.ui.destroy then pcall(self.ui.destroy, self.ui) end
  local window = self.event_window
  if window then
    pcall(window.ReleaseHandler, window, "OnEvent")
    local iface = lib(self.api, "Interface")
    if iface then pcall(iface.Free, iface, window) end
  end
end

Probe.VERSION = VERSION
Probe.MAX_BUFFER = MAX_BUFFER
return Probe
