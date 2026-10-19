-- Shared helpers for the AAEmu 1.2 Quest Guide addon.
-- Lua 5.1, no external dependencies.

local U = {}

function U.trim(text)
  if type(text) ~= "string" then return "" end
  return (text:gsub("^%s+", ""):gsub("%s+$", ""))
end

function U.finite(value)
  return type(value) == "number" and value == value
    and value ~= math.huge and value ~= -math.huge
end

function U.number(value, fallback)
  if U.finite(value) then return value end
  return fallback
end

function U.clamp(value, low, high)
  if value < low then return low end
  if value > high then return high end
  return value
end

function U.truncate(text, limit)
  if type(text) ~= "string" then text = tostring(text or "") end
  if #text <= limit then return text end
  return text:sub(1, math.max(0, limit - 1)) .. "~"
end

function U.safe_call(fn, ...)
  if type(fn) ~= "function" then
    return false, nil, "missing function"
  end
  local ok, result = pcall(fn, ...)
  if not ok then
    return false, nil, tostring(result)
  end
  return true, result, nil
end

function U.safe_method(object, name, ...)
  if type(object) ~= "table" or type(object[name]) ~= "function" then
    return false, nil, "missing method " .. tostring(name)
  end
  return U.safe_call(object[name], object, ...)
end

-- Like safe_call/safe_method, but preserves multiple return values (positions
-- and projections return x,y,z). The result table is a plain array; errors are
-- reported separately and never converted to numeric zero.
function U.safe_multi(fn, ...)
  if type(fn) ~= "function" then
    return false, {}, "missing function"
  end
  local packed = {pcall(fn, ...)}
  local ok = table.remove(packed, 1)
  if not ok then
    return false, {}, tostring(packed[1])
  end
  return true, packed, nil
end

function U.safe_method_multi(object, name, ...)
  if type(object) ~= "table" or type(object[name]) ~= "function" then
    return false, {}, "missing method " .. tostring(name)
  end
  return U.safe_multi(object[name], object, ...)
end

function U.safe_func(object, name, ...)
  if type(object) ~= "table" or type(object[name]) ~= "function" then
    return false, nil, "missing function " .. tostring(name)
  end
  return U.safe_call(object[name], ...)
end

function U.safe_value(ok, value, error_message, limits)
  if not ok then
    return {ok = false, error = U.truncate(tostring(error_message or "error"), 180)}
  end
  if value == nil then
    return {ok = true, nil_value = true}
  end
  return {ok = true, value = U.sanitize(value, limits)}
end

function U.truncate_utf8(text, limit)
  if type(text) ~= "string" then return "" end
  if limit <= 0 then return "" end
  if #text <= limit then return text end
  local cut = limit
  while cut > 0 do
    local byte = string.byte(text, cut + 1)
    if not byte or byte < 0x80 or byte > 0xBF then break end
    cut = cut - 1
  end
  if cut <= 0 then return "" end
  return text:sub(1, cut - 1) .. "~"
end

-- Bounded copy of arbitrary API tables: depth, per-node limits and one shared
-- total budget for the whole snapshot. nil stays distinct from 0.
function U.sanitize(value, limits, depth, seen, budget)
  limits = limits or {}
  local max_depth = limits.max_depth or 5
  local max_keys = limits.max_keys or 40
  local max_items = limits.max_items or 32
  local max_string = limits.max_string or 240
  depth = depth or 0
  seen = seen or {}
  budget = budget or {
    nodes = limits.max_nodes or 2000,
    chars = limits.max_chars or 24000,
  }
  if budget.nodes <= 0 then return "<budget>" end
  budget.nodes = budget.nodes - 1
  local kind = type(value)
  if value == nil then return nil end
  if kind == "number" then
    if not U.finite(value) then return tostring(value) end
    return value
  end
  if kind == "string" then
    if budget.chars <= 0 then return "<budget>" end
    local allowed = math.min(max_string, budget.chars)
    local out = U.truncate_utf8(value, allowed)
    budget.chars = budget.chars - #out
    return out
  end
  if kind == "boolean" then return value end
  if kind ~= "table" then return "<" .. kind .. ">" end
  if seen[value] then return "<cycle>" end
  if depth >= max_depth then return "<max-depth>" end
  seen[value] = true
  local out = {}
  local keys = {}
  for key in pairs(value) do
    if type(key) == "string" or type(key) == "number" then
      keys[#keys + 1] = key
      if #keys >= max_keys then break end
    end
  end
  table.sort(keys, function(a, b)
    local ta, tb = type(a), type(b)
    if ta ~= tb then return ta < tb end
    if ta == "number" then return a < b end
    return tostring(a) < tostring(b)
  end)
  local count = 0
  for _, key in ipairs(keys) do
    if count >= max_items or budget.nodes <= 0 then
      out._truncated = true
      break
    end
    local clean_key = key
    if type(key) == "string" then
      clean_key = U.truncate_utf8(key, 80)
    end
    out[clean_key] = U.sanitize(value[key], limits, depth + 1, seen, budget)
    count = count + 1
  end
  if #keys >= max_items then out._truncated = true end
  seen[value] = nil
  return out
end
function U.wrap(text, width)
  width = width or 96
  local lines = {}
  if type(text) ~= "string" then return lines end
  for raw in (text .. "\n"):gmatch("(.-)\n") do
    raw = raw:gsub("\r", "")
    if #raw == 0 then
      lines[#lines + 1] = ""
    else
      while #raw > width do
        local cut = raw:sub(1, width):match("^.*()%s")
        if not cut or cut < math.floor(width * 0.55) then cut = width + 1 end
        lines[#lines + 1] = U.trim(raw:sub(1, cut - 1))
        raw = U.trim(raw:sub(cut))
      end
      if #raw > 0 then lines[#lines + 1] = raw end
    end
  end
  return lines
end

function U.lines(text, width, limit)
  local result = U.wrap(text, width or 96)
  if limit and #result > limit then
    result = {unpack(result, 1, limit)}
    result[#result + 1] = "~ (more text omitted)"
  end
  return result
end

function U.join_ids(ids, limit)
  if type(ids) ~= "table" then return "" end
  limit = limit or 12
  local parts = {}
  for index = 1, math.min(#ids, limit) do
    parts[#parts + 1] = tostring(ids[index])
  end
  if #ids > limit then parts[#parts + 1] = "+" .. tostring(#ids - limit) end
  return table.concat(parts, ",")
end

function U.words(text)
  local result = {}
  for word in tostring(text or ""):gmatch("%S+") do result[#result + 1] = word end
  return result
end

function U.contains(text, needle)
  return tostring(text or ""):lower():find(tostring(needle or ""):lower(), 1, true) ~= nil
end

function U.key(value)
  return tostring(value or "")
end

function U.set_from_list(list)
  local set = {}
  if type(list) == "table" then
    for _, value in ipairs(list) do set[tostring(value)] = true end
  end
  return set
end

function U.list_from_set(set, limit)
  local list = {}
  if type(set) ~= "table" then return list end
  for key in pairs(set) do
    list[#list + 1] = key
    if limit and #list >= limit then break end
  end
  table.sort(list)
  return list
end

return U
