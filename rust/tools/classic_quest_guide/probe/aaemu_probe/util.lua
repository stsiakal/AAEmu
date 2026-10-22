-- Shared helpers for the AAEmu probe. Lua 5.1, no globals beyond the sandbox.

local U = {}

function U.truncate_utf8(text, limit)
  if type(text) ~= "string" then return "" end
  if #text <= limit then return text end
  local cut = limit
  while cut > 0 do
    local byte = string.byte(text, cut + 1)
    if not byte or byte < 0x80 or byte > 0xBF then break end
    cut = cut - 1
  end
  if cut <= 0 then return "" end
  return text:sub(1, cut) .. "~"
end

function U.finite(value)
  return type(value) == "number" and value == value
    and value ~= math.huge and value ~= -math.huge
end

-- Bounded deep copy: depth, keys per table, string length and one shared node
-- budget per value. Truncation is marked in the copy, never silent.
function U.sanitize(value, limits, depth, seen, budget)
  limits = limits or {}
  local max_depth = limits.max_depth or 3
  local max_keys = limits.max_keys or 40
  local max_string = limits.max_string or 300
  depth = depth or 0
  seen = seen or {}
  budget = budget or {nodes = limits.max_nodes or 600}
  if value == nil then return nil end
  if budget.nodes <= 0 then return "<budget>" end
  budget.nodes = budget.nodes - 1
  local kind = type(value)
  if kind == "number" then
    if not U.finite(value) then return tostring(value) end
    return value
  end
  if kind == "string" then
    return U.truncate_utf8(value, max_string)
  end
  if kind == "boolean" then return value end
  if kind ~= "table" then return "<" .. kind .. ">" end
  if seen[value] then return "<cycle>" end
  if depth >= max_depth then
    return "<max-depth>"
  end
  seen[value] = true
  local keys = {}
  local total = 0
  local ok = pcall(function()
    for key in pairs(value) do
      total = total + 1
      if type(key) == "string" or type(key) == "number" then
        if #keys < max_keys then keys[#keys + 1] = key end
      end
    end
  end)
  table.sort(keys, function(a, b)
    local ta, tb = type(a), type(b)
    if ta ~= tb then return ta < tb end
    return a < b
  end)
  local out = {}
  for _, key in ipairs(keys) do
    local clean_key = key
    if type(key) == "string" then clean_key = U.truncate_utf8(key, 80) end
    local item
    local got = pcall(function() item = value[key] end)
    if got then
      out[clean_key] = U.sanitize(item, limits, depth + 1, seen, budget)
    else
      out[clean_key] = "<unreadable>"
    end
  end
  if total > #keys then out._more_keys = total - #keys end
  seen[value] = nil
  return out
end

-- Calls fn and returns {ok=..., n=..., values={...}} with sanitized results.
function U.call(limits, fn, ...)
  if type(fn) ~= "function" then
    return {ok = false, error = "missing function"}
  end
  local packed = {pcall(fn, ...)}
  local ok = table.remove(packed, 1)
  if not ok then
    return {ok = false, error = U.truncate_utf8(tostring(packed[1]), 240)}
  end
  local n = #packed
  -- #packed can stop at an internal nil; recover the real count.
  for index = 1, 12 do
    if packed[index] ~= nil and index > n then n = index end
  end
  local values = {}
  for index = 1, n do values[index] = U.sanitize(packed[index], limits) end
  return {ok = true, n = n, values = values}
end

function U.call_method(limits, object, name, ...)
  if type(object) ~= "table" and type(object) ~= "userdata" then
    return {ok = false, error = "missing library"}
  end
  local fn
  local got = pcall(function() fn = object[name] end)
  if not got or type(fn) ~= "function" then
    return {ok = false, error = "missing method " .. tostring(name)}
  end
  return U.call(limits, fn, object, ...)
end

-- Cheap deterministic signature for change detection.
function U.signature(value, depth)
  depth = depth or 0
  local kind = type(value)
  if kind ~= "table" then return tostring(value) end
  if depth > 4 then return "{..}" end
  local keys = {}
  for key in pairs(value) do keys[#keys + 1] = key end
  table.sort(keys, function(a, b)
    local ta, tb = type(a), type(b)
    if ta ~= tb then return ta < tb end
    return a < b
  end)
  local parts = {}
  for _, key in ipairs(keys) do
    parts[#parts + 1] = tostring(key) .. "=" .. U.signature(value[key], depth + 1)
  end
  return "{" .. table.concat(parts, ",") .. "}"
end

function U.first(result)
  if type(result) == "table" and result.ok and result.values then
    return result.values[1]
  end
  return nil
end

return U
