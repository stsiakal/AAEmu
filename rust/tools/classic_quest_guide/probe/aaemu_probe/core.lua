-- AAEmu probe core: passive event/snapshot recorder. It only reads state.
-- The sole writes are the addon's own observation files (api.File:Write), its
-- own settings and its own window.

local U = require("aaemu_probe/util")
local G = require("aaemu_probe/getters")
local EVENT_NAMES = require("aaemu_probe/events")

local Probe = {}
Probe.__index = Probe

local VERSION = "0.1.1"
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

-- api.On events documented in the API file (UPDATE is the frame tick).
local API_ON_EVENTS = {
  "CHAT_MESSAGE", "TEAM_MEMBERS_CHANGED", "UI_RELOADED", "UPDATE_PING_INFO",
}
local BAG_EVENTS = {BAG_UPDATE = true, ADDED_ITEM = true, REMOVED_ITEM = true}

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
  return true
end

function Probe:_summary_record()
  local counts, suppressed = {}, {}
  for name, count in pairs(self.counts) do counts[name] = count end
  for name, count in pairs(self.suppressed) do suppressed[name] = count end
  return {
    events_seen = self.events_seen, distinct = self.distinct,
    counts = counts, suppressed = suppressed,
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
  if not self:_rate_ok(name, now) then return end
  local raw = {...}
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

function Probe:_run_inventory(phase)
  local libs, names, globals = self:_surface_records()
  self:_add("api_surface", {phase = phase, libs = libs, top_level = names}, true)
  self:_add("api_globals", {phase = phase, globals = globals}, true)
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

function Probe:_buff_list(count_name, fn_name)
  local unit = lib(self.api, "Unit")
  local count = U.first(U.call_method(EVENT_LIMITS, unit, count_name, "target"))
  local list = {}
  if type(count) == "number" then
    for index = 1, math.min(count, MAX_BUFFS) do
      list[index] = U.call_method(EVENT_LIMITS, unit, fn_name, "target", index)
    end
  end
  return {count = count, list = list}
end

function Probe:_dump_target(id)
  local calls, by_id = {}, {}
  for _, entry in ipairs(G.TARGET) do
    calls[entry[1] .. "." .. entry[2]] = self:_call_entry({entry[1], entry[2], "target"})
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
  self:_add("target", {
    id = id, calls = calls, by_id = by_id,
    buffs = self:_buff_list("UnitBuffCount", "UnitBuff"),
    debuffs = self:_buff_list("UnitDeBuffCount", "UnitDeBuff"),
    id_like_fields = hints, pos = ctx.pos, zone = ctx.zone,
  })
end

function Probe:_poll_target()
  local id = U.first(U.call_method(nil, lib(self.api, "Unit"), "GetUnitId", "target"))
  if id == self.last_target then return end
  self.last_target = id
  self.ctx_ms = nil
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
    quest_note = "API exposes only per-questTypeId getters; no active quest list.",
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
