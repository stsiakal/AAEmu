-- Pure helpers for the derived records: item links, unit fingerprints and
-- quest state transitions. No API access, no state, no sandbox-only names.

local D = {}

function D.item_type_from_link(link)
  if type(link) ~= "string" then return nil end
  local digits = link:match("^|i(%d+)")
  if digits then return tonumber(digits) end
  return nil
end

local function text(value)
  if value == nil then return "-" end
  local out = tostring(value)
  if out == "" then return "-" end
  return out
end

-- Stable key for the fallback identity: the client exposes no template id, so
-- name + level + faction + max health is what is available for NPCs.
function D.fingerprint_key(fp)
  fp = fp or {}
  return text(fp.name) .. "|" .. text(fp.level) .. "|" ..
    text(fp.faction) .. "|" .. text(fp.max_health)
end

function D.empty(value)
  return value == nil or value == ""
end

local function same(a, b)
  if a == nil and b == nil then return true end
  return a == b
end

-- first sight: seen_completed / accepted / seen; later: completed, uncompleted,
-- accepted, active, changed or unchanged.
function D.quest_transition(prev, current)
  current = current or {}
  if prev == nil then
    if current.completed == true then return "seen_completed" end
    if not D.empty(current.active_title) then return "accepted" end
    return "seen"
  end
  if prev.completed ~= true and current.completed == true then return "completed" end
  if prev.completed == true and current.completed ~= true then return "uncompleted" end
  if D.empty(prev.active_title) and not D.empty(current.active_title) then
    return "accepted"
  end
  if not D.empty(prev.active_title) and not same(prev.active_title,
      current.active_title) then
    return "active"
  end
  if not same(prev.main_title, current.main_title)
     or not same(prev.body, current.body) then
    return "changed"
  end
  return "unchanged"
end

function D.position_same(a, b, epsilon)
  if type(a) ~= "table" or type(b) ~= "table" then return false end
  epsilon = epsilon or 0.5
  for index = 1, 3 do
    local left, right = tonumber(a[index]), tonumber(b[index])
    if left == nil or right == nil then return false end
    if math.abs(left - right) > epsilon then return false end
  end
  return true
end

return D
