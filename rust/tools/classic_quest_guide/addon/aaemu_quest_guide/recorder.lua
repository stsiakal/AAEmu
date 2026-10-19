-- Observation recorder. All reads are documented API calls guarded by pcall.
-- Records are explicitly "ClassicObserved" and never become 1.2 truth.

local U = require("aaemu_quest_guide/util")

local Recorder = {}
Recorder.__index = Recorder

local SCHEMA = "aaemu_quest_guide.observations"
local VERSION = 1
local SAMPLE_MS = 1000
local FLUSH_MS = 30000
local MAX_RECORDS = 128
local MAX_BUFFS = 32
local MAX_ITEMS = 8
local PENDING_KEY = "AAEMU_QUEST_GUIDE_PENDING_WRITES"
local REASONS = {
  ["Blocked"] = "Blocked",
  ["Missing NPC"] = "Missing NPC",
  ["Missing doodad"] = "Missing doodad",
  ["Wrong outcome"] = "Wrong outcome",
}

local function pending_store()
  local env = _G or getfenv(0)
  env[PENDING_KEY] = env[PENDING_KEY] or {}
  return env[PENDING_KEY]
end

function Recorder.new(api, state)
  local pending = pending_store()
  local pending_count = 0
  for _ in pairs(pending) do pending_count = pending_count + 1 end
  local self = setmetatable({
    api = api,
    state = state,
    pending = pending,
    enabled = false,
    paused = false,
    destroyed = false,
    buffer = {},
    session = nil,
    chunk = 0,
    last_sample = 0,
    last_flush = 0,
    last_identity_check = 0,
    last_path = nil,
    last_error = nil,
    status = pending_count > 0
      and ("Recording off. " .. tostring(pending_count) ..
        " failed write chunk(s) from a previous session retained for retry.")
      or "Recording off.",
  }, Recorder)
  return self
end

function Recorder:_set_status(text)
  self.status = U.truncate(text, 220)
  if self.state and self.state.set_rec_status then
    self.state:set_rec_status(self.status)
  end
end

function Recorder:_settings()
  return self.state.settings or {}
end

function Recorder:identity()
  local ok, id, error_message = U.safe_method(self.api.Unit, "GetUnitId", "player")
  if not ok then
    return nil, "player id unavailable: " .. tostring(error_message)
  end
  local valid_number = U.finite(id) and id > 0
  local valid_string = type(id) == "string" and U.trim(id) ~= ""
  if not valid_number and not valid_string then
    return nil, "player id is nil/zero; refusing to record an unknown identity"
  end
  -- Preserve the opaque id exactly as returned: numeric or string. It is not a
  -- static template id and must not be coerced.

  local ok_name, name, name_error = U.safe_method(
    self.api.Unit, "GetUnitNameById", id)
  if not ok_name then
    return nil, "player name unavailable: " .. tostring(name_error)
  end
  if type(name) ~= "string" or U.trim(name) == "" then
    return nil, "player name is empty; refusing to record an unknown identity"
  end
  return {id = id, name = U.truncate(name, 80)}, nil
end

function Recorder:_context_key(form)
  form = form or {}
  return table.concat({
    U.trim(form.server or ""),
    U.trim(form.context or ""),
    U.trim(form.build or ""),
  }, "|")
end

function Recorder:_session_key()
  local identity = self.session and self.session.player or {}
  local meta = self.state:meta() or {}
  local db = ((meta.inputs or {}).db or {})
  return table.concat({
    U.trim(self.session and self.session.server or ""),
    U.trim(self.session and self.session.context or ""),
    tostring(identity.name or ""),
    tostring(db.sha256 or "unknown"),
  }, "|")
end

function Recorder:start()
  if self.enabled then
    self:_set_status("Recording is already on.")
    return true
  end
  if self.destroyed then return false end
  local pending_count = self:_pending_count()
  if #self.buffer > 0 or pending_count > 0 then
    self:_set_status("Cannot start a new session: " ..
      tostring(#self.buffer + pending_count) ..
      " unsaved record chunk(s) remain from the last session. Retry Save/Export " ..
      "until the write succeeds first.")
    return false
  end
  local identity, identity_error = self:identity()
  if not identity then
    self:_set_status("Cannot start recording: " .. identity_error)
    return false
  end
  local form = self.state:get_form() or {}
  local settings = self:_settings()
  local sequence = tonumber(settings.next_session) or 0
  sequence = sequence + 1
  settings.next_session = sequence
  local saved, save_error = self.state:save_settings("session sequence")
  if not saved then
    self:_set_status("Cannot start: session sequence could not be persisted: " ..
      tostring(save_error))
    return false
  end
  local meta = self.state:meta() or {}
  local db = ((meta.inputs or {}).db or {})
  local worlds = ((meta.inputs or {}).worlds or {})
  local spheres = ((meta.inputs or {}).spheres or {})
  local generator = meta.generator or {}
  self.session = {
    id = string.format("%06d", sequence),
    sequence = sequence,
    started_ui_ms = self.state:now(),
    player = identity,
    server = U.trim(form.server or ""),
    context = U.trim(form.context or ""),
    build = U.trim(form.build or ""),
    world = U.trim(form.world or ""),
    context_key = self:_context_key(form),
    catalog_sha256 = tostring(db.sha256 or "unknown"),
    expected_data_version = tostring(meta.data_version or "1.2"),
    catalog = {
      database_sha256 = tostring(db.sha256 or "unknown"),
      worlds_sha256 = tostring(worlds.sha256 or "unknown"),
      spheres_sha256 = tostring(spheres.sha256 or "unknown"),
      generator = {
        name = tostring(generator.name or "unknown"),
        version = tostring(generator.version or "unknown"),
        code_commit = tostring(generator.code_commit or "unknown"),
        code_dirty = generator.code_dirty,
      },
    },
  }
  self.enabled = true
  self.paused = false
  self.buffer = {}
  self.chunk = 0
  self.last_sample = self.state:now()
  self.last_flush = self.last_sample
  self.last_identity_check = self.last_sample
  self.last_path = nil
  self.last_error = nil
  local pending_count = 0
  for _ in pairs(self.pending) do pending_count = pending_count + 1 end
  self:_set_status("Recording started (session " .. self.session.id ..
    ", ClassicObserved)." ..
    (pending_count > 0 and (" " .. tostring(pending_count) ..
      " earlier failed write chunk(s) still await retry.") or ""))
  return true
end

function Recorder:is_recording()
  return self.enabled
end

function Recorder:buffer_count()
  return #self.buffer
end

function Recorder:_unit_snapshot(unit)
  local snapshot = {unit = unit}
  local ok_id, id, id_error = U.safe_method(self.api.Unit, "GetUnitId", unit)
  snapshot.opaque_unit_id = U.safe_value(ok_id, id, id_error)
  if ok_id and id ~= nil then
    local ok_name, name, name_error = U.safe_method(
      self.api.Unit, "GetUnitNameById", id)
    snapshot.name = U.safe_value(ok_name, name, name_error)
  end
  local ok_info, info, info_error = U.safe_method(self.api.Unit, "UnitInfo", unit)
  snapshot.unit_info = U.safe_value(ok_info, info, info_error,
    {max_depth = 5, max_keys = 40, max_items = 32, max_string = 200})
  local ok_pos, values, pos_error = U.safe_method_multi(
    self.api.Unit, "UnitWorldPosition", unit)
  if ok_pos and U.finite(values[1]) and U.finite(values[2]) and U.finite(values[3]) then
    snapshot.position = {x = values[1], y = values[2], z = values[3]}
  elseif ok_pos then
    snapshot.position = {nil_value = true}
  else
    snapshot.position = {ok = false, error = U.truncate(tostring(pos_error), 160)}
  end
  for _, key in ipairs({"UnitHealth", "UnitMaxHealth", "UnitMana", "UnitMaxMana"}) do
    local ok, value, error_message = U.safe_method(self.api.Unit, key, unit)
    snapshot[key:sub(1, 1):lower() .. key:sub(2)] =
      U.safe_value(ok, value, error_message)
  end
  for _, spec in ipairs({{"buff", "UnitBuffCount", "UnitBuff"},
                         {"debuff", "UnitDeBuffCount", "UnitDeBuff"}}) do
    local label, count_method, item_method = spec[1], spec[2], spec[3]
    local ok, count, error_message = U.safe_method(
      self.api.Unit, count_method, unit)
    local out = {count = U.safe_value(ok, count, error_message), items = {}}
    if ok and U.finite(count) and count > 0 then
      out.truncated = count > MAX_BUFFS
      for index = 1, math.min(count, MAX_BUFFS) do
        local item_ok, item, item_error = U.safe_method(
          self.api.Unit, item_method, unit, index)
        if not item_ok then
          out.item_errors = (out.item_errors or 0) + 1
        end
        out.items[index] = U.safe_value(item_ok, item, item_error,
          {max_depth = 3, max_keys = 20, max_items = 12, max_string = 160})
      end
    elseif not ok then
      out.error = U.truncate(tostring(error_message), 160)
    end
    snapshot[label .. "s"] = out
  end
  return snapshot
end

function Recorder:_selected()
  local info = self.state:selected_info() or {}
  return {
    quest_id = info.quest_id,
    step_index = info.step_index,
    component_id = info.component_id,
    act_id = info.act_id,
    manually_acknowledged = info.manually_acknowledged,
  }
end

function Recorder:_quest_text()
  local qid = self.state.quest and self.state.quest.id or nil
  if not qid then return {} end
  local out = {}
  local ok_completed, completed, completed_error = U.safe_method(
    self.api.Quest, "IsCompleted", qid)
  out.completed = U.safe_value(ok_completed, completed, completed_error)
  local ok_title, title, title_error = U.safe_method(
    self.api.Quest, "GetQuestContextMainTitle", qid)
  if not ok_title or title == nil or title == "" then
    local ok_active, active, active_error = U.safe_method(
      self.api.Quest, "GetActiveQuestTitle", qid)
    if ok_active and active ~= nil and active ~= "" then
      out.title = {ok = true, value = tostring(active)}
    else
      out.title = U.safe_value(ok_title, title, title_error)
      if not ok_title then out.title.error = tostring(active_error or title_error) end
    end
  else
    out.title = {ok = true, value = tostring(title)}
  end
  local ok_body, body, body_error = U.safe_method(
    self.api.Quest, "GetQuestContextBody", qid)
  out.body = U.safe_value(ok_body, body, body_error)
  return out
end

function Recorder:_times()
  local out = {}
  local ok_ui, ui, ui_error = U.safe_method(self.api.Time, "GetUiMsec")
  out.ui_msec = U.safe_value(ok_ui, ui, ui_error)
  for _, key in ipairs({"GetLocalTime", "GetGameTime"}) do
    local ok, value, error_message = U.safe_method(self.api.Time, key)
    out[key == "GetLocalTime" and "local_time" or "game_time"] =
      U.safe_value(ok, value, error_message,
        {max_depth = 3, max_keys = 20, max_items = 20, max_string = 80})
  end
  return out
end

function Recorder:_bag()
  local out = {}
  for _, key in ipairs({"GetCurrency", "CountItems", "Capacity"}) do
    local ok, value, error_message = U.safe_method(self.api.Bag, key)
    out[key] = U.safe_value(ok, value, error_message)
  end
  local step = self.state:current_step()
  local target = step and step.target or {}
  if target.kind == "item" and type(target.ids) == "table" then
    out.item_type_counts = {}
    for index = 1, math.min(#target.ids, MAX_ITEMS) do
      local id = target.ids[index]
      local ok, value, error_message = U.safe_method(
        self.api.Bag, "CountBagItemByItemType", id)
      out.item_type_counts[#out.item_type_counts + 1] = {
        item_type = id,
        count = U.safe_value(ok, value, error_message),
      }
    end
  end
  return out
end

function Recorder:_record(kind, extra)
  local meta = self.state:meta() or {}
  local form = self.state:get_form() or {}
  local record = {
    schema = SCHEMA,
    version = VERSION,
    kind = kind,
    source = "ClassicObserved",
    expected_data_version = tostring(meta.data_version or "1.2"),
    recorded_ui_msec = self.state:now(),
    session = self.session.id,
    player = self.session.player,
    context = {
      server = U.trim(form.server or ""),
      context = U.trim(form.context or ""),
      build = U.trim(form.build or ""),
      world = U.trim(form.world or ""),
      user_declared = true,
      api_current_world_known = false,
      world_note = "User-declared context; the Classic API does not expose the current world.",
    },
    selected = self:_selected(),
    expected_provenance = {
      database_sha256 = self.session.catalog_sha256,
      worlds_sha256 = self.session.catalog.worlds_sha256,
      spheres_sha256 = self.session.catalog.spheres_sha256,
      generator = self.session.catalog.generator,
      note = "Expected 1.2 static catalog provenance; observations are Classic, not 1.2 truth.",
    },
    player_snapshot = self:_unit_snapshot("player"),
    target_snapshot = self:_unit_snapshot("target"),
    quest_text = self:_quest_text(),
    times = self:_times(),
    bag = self:_bag(),
  }
  if extra then
    for key, value in pairs(extra) do record[key] = value end
  end
  return record
end

function Recorder:_add(record)
  if not self.enabled then
    self:_set_status("Start recording before saving observations.")
    return false
  end
  if self.paused or #self.buffer >= MAX_RECORDS then
    self.paused = true
    self:_set_status("Buffer full (" .. tostring(#self.buffer) ..
      "/" .. tostring(MAX_RECORDS) .. "); recording paused. Flush failed records " ..
      "before retrying. Unsaved data may be lost if the game exits.")
    return false
  end
  self.buffer[#self.buffer + 1] = record
  return true
end

function Recorder:_pending_count()
  local count = 0
  for _ in pairs(self.pending or {}) do count = count + 1 end
  return count
end

function Recorder:_write(path, payload)
  local ok, result, error_message = U.safe_method(
    self.api.File, "Write", path, payload)
  if ok and (result == nil or result == true) then return true, nil end
  return false, tostring(error_message or result or "write failed")
end

function Recorder:_flush_pending()
  local paths = {}
  for path in pairs(self.pending or {}) do paths[#paths + 1] = path end
  table.sort(paths)
  for _, path in ipairs(paths) do
    local payload = self.pending[path]
    local ok, error_message = self:_write(path, payload)
    if not ok then
      self.last_error = error_message
      self:_set_status("Pending write failed for " .. path .. ": " ..
        tostring(error_message) .. ". " .. tostring(#(payload.records or {})) ..
        " record(s) retained. Unsaved data may be lost if the game exits.")
      return false
    end
    self.pending[path] = nil
    self.last_path = path
  end
  return true
end

function Recorder:_validate_current(reason)
  local identity, identity_error = self:identity()
  if not identity then
    self:_isolate(nil, reason or "identity lost", identity_error)
    return false
  end
  local same = self.session and self.session.player
    and self.session.player.id == identity.id
    and self.session.player.name == identity.name
  if not same then
    self:_isolate(identity, "identity change", nil)
    return false
  end
  local form = self.state:get_form() or {}
  if self:_context_key(form) ~= self.session.context_key then
    self:_isolate(nil, "context change", nil)
    return false
  end
  return true
end

function Recorder:_isolate(identity, reason, detail)
  local ok = self:flush(reason or "session isolation")
  self.enabled = false
  self.paused = false
  self:_set_status("Recording stopped (" .. tostring(reason or "isolation") ..
    (detail and (": " .. tostring(detail)) or "") .. "). " ..
    (ok and "Records were flushed." or
      "Unsaved records are retained for retry; they may be lost if the game exits."))
  if identity ~= nil then
    if self.state.notify_identity_changed then
      self.state:notify_identity_changed(identity)
    end
  elseif reason == "context change" then
    if self.state.notify_context_changed then
      self.state:notify_context_changed()
    end
  else
    if self.state.notify_identity_changed then
      self.state:notify_identity_changed(nil)
    end
  end
end

function Recorder:sample()
  if not self.enabled or self.paused then return false end
  if not self:_validate_current("sample") then return false end
  return self:_add(self:_record("periodic"))
end

function Recorder:snapshot(unit)
  if not self.enabled then
    self:_set_status("Start recording before taking a saved snapshot.")
    return false
  end
  if not self:_validate_current("snapshot") then return false end
  return self:_add(self:_record("snapshot", {
    snapshot_unit = unit,
    user_annotation = false,
    confirmed_event = false,
  }))
end

function Recorder:annotate(reason, note)
  if not self.enabled then
    self:_set_status("Start recording before saving a report annotation.")
    return false
  end
  if not self:_validate_current("annotation") then return false end
  reason = REASONS[reason] or "Blocked"
  note = U.truncate(U.trim(note or ""), 200)
  local ok = self:_add(self:_record("annotation", {
    reason = reason,
    note = note,
    user_annotation = true,
    confirmed_event = false,
    note_text = "User annotation, not a confirmed game event.",
  }))
  if ok then
    self:_set_status("Annotation saved to buffer (" .. reason .. ").")
  end
  return ok
end

function Recorder:flush(reason)
  local pending_ok = self:_flush_pending()
  if not pending_ok then return false end
  if #self.buffer == 0 then
    if self.paused and self:_pending_count() == 0 then self.paused = false end
    self.last_error = nil
    self:_set_status(self.enabled and "Recording on; no unsaved records."
      or "Recording off; no unsaved records.")
    return true
  end
  local path = string.format("aaemu_quest_guide/observations/session_%s_chunk_%03d.lua",
    self.session.id, self.chunk + 1)
  local payload = {
    schema = SCHEMA,
    version = VERSION,
    source = "ClassicObserved",
    expected_data_version = self.session.expected_data_version,
    flushed_reason = tostring(reason or "manual"),
    session = self.session,
    records = self.buffer,
  }
  local count = #self.buffer
  local success, error_message = self:_write(path, payload)
  if not success then
    self.last_error = tostring(error_message or "write failed")
    -- Retain the whole chunk in the process-local pending queue and pause new
    -- samples until a retry succeeds. Do not keep a duplicate copy in buffer.
    self.pending[path] = payload
    self.buffer = {}
    self.paused = true
    self:_set_status("Write failed for " .. path .. ": " .. self.last_error ..
      ". " .. tostring(count) .. " record(s) retained for retry; recording paused. " ..
      "Unsaved data may be lost if the game exits.")
    local ok_log, _, _ = U.safe_method(self.api.Log, "Err",
      "AAEmu Quest Guide: observation write failed: " .. self.last_error)
    return false
  end
  self.buffer = {}
  self.chunk = self.chunk + 1
  self.last_flush = self.state:now()
  self.last_error = nil
  self.last_path = path
  if self.paused then self.paused = false end
  self:_set_status("Saved " .. tostring(count) .. " record(s) to " .. path ..
    (self.enabled and "." or " (recording off)."))
  return true
end

function Recorder:retry_flush()
  return self:flush("retry")
end

function Recorder:stop(reason)
  if not self.enabled then
    if #self.buffer > 0 then return self:flush(reason or "retry") end
    self:_set_status("Recording is already off.")
    return true
  end
  local ok = self:flush(reason or "stop")
  self.enabled = false
  self.paused = false
  if ok then
    self:_set_status("Recording stopped. " .. self.status)
  else
    self:_set_status("Recording stopped, but unsaved records remain. " .. self.status)
  end
  return ok
end

function Recorder:tick(now)
  if self.destroyed or not self.enabled then return end
  now = now or self.state:now()
  if not self:_validate_current("periodic") then return end
  if self.paused then return end
  if now - self.last_sample >= SAMPLE_MS then
    self.last_sample = now
    self:sample()
  end
  if now - self.last_flush >= FLUSH_MS then
    self.last_flush = now
    self:flush("periodic")
  end
end
function Recorder:destroy()
  if self.destroyed then return end
  if self.enabled or #self.buffer > 0 then
    local ok, error_message = self:flush("unload")
    if not ok then
      U.safe_method(self.api.Log, "Err",
        "AAEmu Quest Guide: unload flush failed: " .. tostring(error_message or self.last_error))
    end
  end
  self.enabled = false
  self.paused = false
  self.destroyed = true
end

function Recorder:status_text()
  return self.status
end

return Recorder
