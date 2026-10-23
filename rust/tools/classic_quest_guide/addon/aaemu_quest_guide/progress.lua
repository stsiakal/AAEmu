-- Quest progress state: persisted turn-in/reward observations, manual marks and
-- live client reads. Nothing here executes a game action or invents completion.

local U = require("aaemu_quest_guide/util")

local Progress = {}
Progress.__index = Progress

local QUEST_ID_TTL_MS = 60000
local SOURCE_QUEST_REWARD = 35
local MAX_IDS = 6000

function Progress.new(api, state)
  return setmetatable({
    api = api,
    state = state,
    observed = {},
    manual = {},
    live_cache = {},
    active_cache = {},
    last_quest_id = nil,
    last_quest_ms = nil,
    scope = nil,
    status = "Quest progress not loaded.",
  }, Progress)
end

function Progress:_now()
  local ok, ms = U.safe_method(self.api.Time, "GetUiMsec")
  if ok and U.finite(ms) then return ms end
  return self.state:now()
end

function Progress:key()
  if self.state.storage_key then return self.state:storage_key() end
  return nil
end

function Progress:reload()
  self.observed = {}
  self.manual = {}
  self.live_cache = {}
  self.active_cache = {}
  local key = self:key()
  self.scope = key
  if not key then
    self.status = "Progress storage unavailable: player/context unknown."
    return false
  end
  local settings = self.state.settings or {}
  local completion = settings.completion or {}
  local manual = settings.manual_done or {}
  self.observed = U.set_from_list(completion[key])
  self.manual = U.set_from_list(manual[key])
  self.status = "Progress loaded for this character/context/catalog."
  return true
end

function Progress:save()
  local key = self:key()
  if not key then
    self.status = "Progress not saved: player identity is unknown."
    return false, self.status
  end
  local settings = self.state.settings or {}
  self.state.settings = settings
  settings.completion = settings.completion or {}
  settings.manual_done = settings.manual_done or {}
  settings.completion[key] = U.list_from_set(self.observed, MAX_IDS)
  settings.manual_done[key] = U.list_from_set(self.manual, MAX_IDS)
  local ok, error_message = self.state:save_settings("progress")
  if ok then
    self.status = "Progress saved for this character/context/catalog."
    return true, nil
  end
  self.status = "Progress saved in memory only: " ..
    tostring(error_message or "SaveSettings failed")
  return false, self.status
end

function Progress:refresh_live()
  self.live_cache = {}
  self.active_cache = {}
end

function Progress:_live(id)
  id = tonumber(id)
  if not id then return nil end
  local cached = self.live_cache[id]
  if cached then return cached end
  local ok, value, error_message = U.safe_method(self.api.Quest, "IsCompleted", id)
  cached = {ok = ok, value = value, error = error_message}
  self.live_cache[id] = cached
  return cached
end

function Progress:live_completed(id)
  local record = self:_live(id)
  return record ~= nil and record.ok and record.value == true
end

function Progress:_active(id)
  id = tonumber(id)
  if not id then return nil end
  local cached = self.active_cache[id]
  if cached then return cached end
  local ok, value, error_message = U.safe_method(
    self.api.Quest, "GetActiveQuestTitle", id)
  cached = {ok = ok, value = value, error = error_message}
  self.active_cache[id] = cached
  return cached
end

function Progress:is_active(id)
  local record = self:_active(id)
  return record ~= nil and record.ok and type(record.value) == "string"
    and U.trim(record.value) ~= ""
end

function Progress:is_observed(id)
  return self.observed[tostring(tonumber(id) or "")] == true
end

function Progress:is_manual(id)
  return self.manual[tostring(tonumber(id) or "")] == true
end

function Progress:is_done(id)
  if self:is_observed(id) or self:is_manual(id) then return true end
  return self:live_completed(id)
end

function Progress:status_for(id)
  if self:is_observed(id) or self:live_completed(id) then return "completed" end
  if self:is_manual(id) then return "manual" end
  if self:is_active(id) then return "progress" end
  return "open"
end

function Progress:set_manual(id, enabled)
  id = tonumber(id)
  if not id or id <= 0 or id ~= math.floor(id) then
    return false, "Quest ID must be a positive integer."
  end
  local key = tostring(id)
  if enabled then
    self.manual[key] = true
  else
    self.manual[key] = nil
  end
  local ok, message = self:save()
  if not ok then return false, message end
  return true, enabled and ("Marked quest #" .. key .. " done manually ([m]).")
    or ("Removed the manual done mark from quest #" .. key .. ".")
end

function Progress:_mark_observed(quest_id, source)
  quest_id = tonumber(quest_id)
  if not quest_id or quest_id <= 0 then return false end
  if not (self.state.catalog and self.state.catalog:entry(quest_id)) then
    return false
  end
  local key = tostring(quest_id)
  if self.observed[key] then return false end
  self.observed[key] = true
  self:save()
  self.status = "Quest #" .. key .. " completed (" .. tostring(source) ..
    "); recorded for this character/context/catalog."
  return true, self.status
end

function Progress:_turn_in(source)
  local quest_id = self.last_quest_id
  local seen = self.last_quest_ms
  local now = self:_now()
  if not quest_id or not seen or now - seen > QUEST_ID_TTL_MS then
    return false
  end
  local changed, message = self:_mark_observed(quest_id, source)
  if changed then
    self.last_quest_id = nil
    self.last_quest_ms = nil
    return true, message, quest_id
  end
  return false
end

-- Widget event sink. CHAT_MSG_QUEST arg 9 carries the quest type id; a quest
-- reward item (ADDED_ITEM source 35) or turn-in removal (REMOVED_ITEM source
-- 35) within one minute completes that quest. This is the only automatic
-- completion source besides api.Quest:IsCompleted.
function Progress:handle_event(name, ...)
  local args = {...}
  if name == "CHAT_MSG_QUEST" then
    local quest_id = tonumber(args[9])
    if quest_id and quest_id > 0 and quest_id == math.floor(quest_id) then
      self.last_quest_id = quest_id
      self.last_quest_ms = self:_now()
    end
    return false
  end
  if name == "ADDED_ITEM" and tonumber(args[3]) == SOURCE_QUEST_REWARD then
    return self:_turn_in("reward item")
  end
  if name == "REMOVED_ITEM" and tonumber(args[4]) == SOURCE_QUEST_REWARD then
    return self:_turn_in("turn-in item")
  end
  return false
end

return Progress
