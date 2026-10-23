-- Bounded catalog checklist: status marks, filters and one page of rows.
-- Rows are built only on explicit refresh, never from UPDATE.

local U = require("aaemu_quest_guide/util")

local Checklist = {}
Checklist.__index = Checklist

local PER_PAGE = 16
local FILTERS = {open = true, done = true, all = true}
local MARKS = {
  completed = "[x]", manual = "[m]", progress = "[~]", open = "[ ]",
}
local STATUS_FILTER = {open = "open", done = "done", all = "all"}

function Checklist.new(api, state)
  return setmetatable({
    api = api,
    state = state,
    filter = "open",
    hide_placeholders = true,
    hide_not_ref = false,
    page = 1,
    rows = {},
    total = 0,
    page_count = 1,
    done_on_page = 0,
    done_in_scope = 0,
    scope_count = 0,
  }, Checklist)
end

function Checklist:ref_available()
  local meta = self.state:meta() or {}
  return ((meta.inputs or {}).ref_db) ~= nil
end

function Checklist:set_filter(name)
  name = STATUS_FILTER[name] or name
  if not FILTERS[name] then return false end
  self.filter = name
  self.page = 1
  return true
end

function Checklist:filter_text()
  return tostring(self.state.filter or "")
end

function Checklist:_title(entry)
  local quest_id = entry.id
  local ok, title = U.safe_method(
    self.api.Quest, "GetQuestContextMainTitle", quest_id)
  if ok and type(title) == "string" and U.trim(title) ~= "" then return title end
  ok, title = U.safe_method(self.api.Quest, "GetActiveQuestTitle", quest_id)
  if ok and type(title) == "string" and U.trim(title) ~= "" then return title end
  if type(entry.name) == "string" and U.trim(entry.name) ~= "" then
    return entry.name
  end
  return "Quest#" .. tostring(quest_id)
end

function Checklist:_giver(entry)
  local kind = entry.start_kind
  if entry.start_npc_name and U.trim(entry.start_npc_name) ~= "" then
    return tostring(entry.start_npc_name)
  end
  if entry.start_npc_id then return "NPC " .. tostring(entry.start_npc_id) end
  if kind == "doodad" then return "doodad start" end
  if kind == "item" then return "item start" end
  if kind == "quest" then return "quest start" end
  if kind then return tostring(kind) .. " start" end
  return "no accept step"
end

function Checklist:_category(entry)
  local categories = (self.state.catalog.index or {}).categories or {}
  local name = categories[entry.category_id]
  if type(name) ~= "string" or U.trim(name) == "" then
    name = "category " .. tostring(entry.category_id or "?")
  end
  return name
end

function Checklist:row_text(entry)
  local status = self.state.progress:status_for(entry.id)
  local line = string.format("%s %d  %s  Lv%s  %s (zone %s)  Giver: %s",
    MARKS[status] or "[?]", entry.id, self:_title(entry),
    tostring(entry.level or "?"), self:_category(entry),
    tostring(entry.zone_id or "?"), self:_giver(entry))
  return U.truncate(line, 150), status
end

function Checklist:_include(entry)
  if self.hide_placeholders and entry.placeholder then return false end
  if self.hide_not_ref and self:ref_available() and entry.in_ref == false then
    return false
  end
  local text = self:filter_text()
  if U.trim(text) ~= ""
      and not (self.state.catalog and self.state.catalog:matches_entry(entry, text)) then
    return false
  end
  return true
end

function Checklist:build()
  local state = self.state
  local catalog = state.catalog
  self.rows = {}
  self.total = 0
  self.page_count = 1
  self.done_on_page = 0
  self.done_in_scope = 0
  self.scope_count = 0
  if not catalog then return self end

  local scope = {}
  for _, entry in ipairs(catalog.index.quests or {}) do
    if self:_include(entry) then scope[#scope + 1] = entry end
  end
  self.scope_count = #scope

  local filtered = {}
  for _, entry in ipairs(scope) do
    local done = state.progress:is_done(entry.id)
    if done then self.done_in_scope = self.done_in_scope + 1 end
    if self.filter == "all"
        or (self.filter == "done" and done)
        or (self.filter == "open" and not done) then
      filtered[#filtered + 1] = entry
    end
  end
  self.total = #filtered
  self.page_count = math.max(1, math.ceil(self.total / PER_PAGE))
  self.page = U.clamp(tonumber(self.page) or 1, 1, self.page_count)
  local first = (self.page - 1) * PER_PAGE + 1
  local last = math.min(self.total, first + PER_PAGE - 1)
  for index = first, last do
    local entry = filtered[index]
    local text, status = self:row_text(entry)
    self.rows[#self.rows + 1] = {entry = entry, id = entry.id,
                                 text = text, status = status}
    if status == "completed" or status == "manual" then
      self.done_on_page = self.done_on_page + 1
    end
  end
  return self
end

function Checklist:next_page(delta)
  local target = U.clamp(self.page + (delta or 1), 1, self.page_count)
  if target == self.page then return false end
  self.page = target
  self:build()
  return true
end

function Checklist:filter_summary()
  local parts = {}
  parts[#parts + 1] = self.filter == "open" and "not completed"
    or (self.filter == "done" and "completed" or "all")
  parts[#parts + 1] = self.hide_placeholders and "placeholders hidden"
    or "placeholders shown"
  if self:ref_available() then
    parts[#parts + 1] = self.hide_not_ref and "not in 3.0 hidden"
      or "not in 3.0 shown"
  else
    parts[#parts + 1] = "reference db unavailable"
  end
  if U.trim(self:filter_text()) ~= "" then
    parts[#parts + 1] = "text: " .. U.truncate(self:filter_text(), 60)
  end
  return table.concat(parts, " | ")
end

Checklist.PER_PAGE = PER_PAGE
Checklist.MARKS = MARKS

return Checklist
