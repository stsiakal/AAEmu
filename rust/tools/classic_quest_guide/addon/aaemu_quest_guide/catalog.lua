-- Lazy catalog access. Only the selected chunk is required, and at most a
-- small bounded number of chunks stay in memory.

local Catalog = {}
Catalog.__index = Catalog

local MAX_CHUNKS = 6

local function load_module(name)
  local ok, module = pcall(require, name)
  if not ok then return nil, tostring(module) end
  return module, nil
end

function Catalog.new()
  local self = setmetatable({}, Catalog)
  local index, index_error = load_module("aaemu_quest_guide/data/index")
  if not index then
    return nil, "catalog index unavailable: " .. tostring(index_error)
  end
  local meta = load_module("aaemu_quest_guide/data/meta") or {}
  local route = load_module("aaemu_quest_guide/data/route") or {}
  self.index = index
  self.meta = meta
  self.route = route
  self.by_id = {}
  self.chunks = {}
  self.chunk_order = {}
  self.chunk_errors = {}
  for _, entry in ipairs(index.quests or {}) do
    if entry and entry.id then self.by_id[entry.id] = entry end
  end
  return self
end

function Catalog:size()
  return tonumber(self.index.quest_count) or #(self.index.quests or {})
end

function Catalog:entry(id)
  return self.by_id[tonumber(id)]
end

function Catalog:chunk_name(chunk)
  return string.format("aaemu_quest_guide/data/chunk_%04d", tonumber(chunk) or 0)
end

function Catalog:load_chunk(chunk)
  if self.chunks[chunk] then return self.chunks[chunk], nil end
  local module, error_message = load_module(self:chunk_name(chunk))
  if not module or type(module.quests) ~= "table" then
    self.chunk_errors[chunk] = error_message or "chunk has no quest table"
    return nil, self.chunk_errors[chunk]
  end
  self.chunks[chunk] = module
  self.chunk_order[#self.chunk_order + 1] = chunk
  while #self.chunk_order > MAX_CHUNKS do
    local evicted = table.remove(self.chunk_order, 1)
    self.chunks[evicted] = nil
    -- Drop our lazy chunk module too when the host exposes package.loaded,
    -- so the cache bound is real rather than only a private table bound.
    if type(package) == "table" and type(package.loaded) == "table" then
      package.loaded[self:chunk_name(evicted)] = nil
    end
  end
  return module, nil
end

function Catalog:quest(id)
  id = tonumber(id)
  if not id then return nil, "quest id is not a number" end
  local entry = self.by_id[id]
  if not entry then return nil, "quest " .. tostring(id) .. " is not in the catalog" end
  local module, error_message = self:load_chunk(entry.chunk)
  if not module then return nil, error_message end
  local quest = module.quests[id]
  if not quest then return nil, "quest " .. tostring(id) .. " is missing from its chunk" end
  return quest, nil
end

local function matches(entry, tokens, index)
  local name = tostring(entry.name or "")
  local status = tostring(entry.status or "")
  local category = tostring(entry.category_id or "")
  local level = tostring(entry.level or "")
  for _, token in ipairs(tokens) do
    local kind, value = token:match("^(%a+)=(.*)$")
    if kind == "status" then
      if status ~= value then return false end
    elseif kind == "level" then
      if level ~= value then return false end
    elseif kind == "cat" then
      if category ~= value then return false end
    elseif kind == "name" then
      if not tostring(name):lower():find(value:lower(), 1, true) then return false end
    else
      local haystack = tostring(entry.id) .. " " .. name .. " " .. status ..
        " " .. tostring(entry.start_npc_name or "")
      local category_name = tostring((index.categories or {})[entry.category_id] or "")
      if not haystack:lower():find(token:lower(), 1, true)
        and not category_name:lower():find(token:lower(), 1, true) then
        return false
      end
    end
  end
  return true
end

local function tokens_for(text)
  local tokens = {}
  local trimmed = tostring(text or ""):gsub("^%s+", ""):gsub("%s+$", "")
  if #trimmed == 0 then return tokens end
  for word in trimmed:gmatch("%S+") do tokens[#tokens + 1] = word end
  return tokens
end

-- Shared by Find and the checklist text filter.
function Catalog:matches_entry(entry, text)
  local tokens = tokens_for(text)
  if #tokens == 0 then return true end
  return matches(entry, tokens, self.index)
end

-- Scan once per explicit search; never called from UPDATE.
function Catalog:search(text, limit)
  limit = limit or 200
  local tokens = tokens_for(text)
  if #tokens == 0 then return nil, 0 end
  local ids = {}
  for _, entry in ipairs(self.index.quests or {}) do
    if matches(entry, tokens, self.index) then
      ids[#ids + 1] = entry.id
      if #ids >= limit then break end
    end
  end
  return ids, #ids
end

function Catalog:route_ids()
  local ids = {}
  for _, id in ipairs((self.route or {}).quest_ids or {}) do
    ids[#ids + 1] = tonumber(id)
  end
  if #ids == 0 then ids = {330, 6198} end
  return ids
end

function Catalog:database_hash()
  local input = (self.meta or {}).inputs or {}
  local db = input.db or {}
  return tostring(db.sha256 or "unknown")
end

function Catalog:data_version()
  return tostring((self.meta or {}).data_version or "unknown")
end

return Catalog
