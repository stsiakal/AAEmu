-- Waypoint projection. Only documented position/projection calls are used.
-- Candidate 1.2 coordinates are never navigation truth without explicit opt-in.

local U = require("aaemu_quest_guide/util")

local Nav = {}
Nav.__index = Nav

local UPDATE_MS = 250
local LABEL_WIDTH = 250
local LABEL_HEIGHT = 44
local MARGIN = 56

function Nav.new(api, state)
  local self = setmetatable({
    api = api,
    state = state,
    mode = "off",
    point = nil,
    status = "Navigation off. 1.2 candidate placement is opt-in.",
    next_update = 0,
    marker = nil,
    label = nil,
    last_status = nil,
  }, Nav)
  return self
end

function Nav:_ensure_marker()
  if self.marker then return self.marker, self.label end
  if self.marker_failed then return nil, nil end
  local ok, marker = U.safe_method(self.api.Interface, "CreateEmptyWindow",
                                  "aqg_waypoint")
  if not ok or not marker then
    self.status = "Waypoint UI unavailable: " .. tostring(marker)
    return nil, nil
  end
  local ok_label, label = U.safe_method(marker, "CreateChildWidget", "label",
                                        "aqg_waypoint_label", 0, true)
  if not ok_label or not label then
    self.status = "Waypoint label unavailable: " .. tostring(label)
    U.safe_method(marker, "Show", false)
    U.safe_method(self.api.Interface, "Free", marker)
    self.marker_failed = true
    return nil, nil
  end
  U.safe_method(label, "SetExtent", LABEL_WIDTH, LABEL_HEIGHT)
  U.safe_method(label, "SetText", "")
  U.safe_method(label, "AddAnchor", "TOPLEFT", marker, "TOPLEFT", 0, 0)
  U.safe_method(marker, "Show", false)
  self.marker = marker
  self.label = label
  return marker, label
end

function Nav:_tag()
  if self.mode == "candidate" then
    return "unverified 1.2 placement"
  elseif self.mode == "observed" then
    return "observed Classic waypoint"
  end
  return ""
end

function Nav:is_active()
  return self.mode ~= "off" and self.point ~= nil
end

function Nav:is_candidate()
  return self.mode == "candidate"
end

function Nav:status_text()
  return self.status
end

function Nav:banner_text()
  if self.mode == "candidate" then
    return "unverified 1.2 placement - confirm the Classic world yourself"
  end
  return ""
end

function Nav:_set_status(text)
  text = U.truncate(text, 190)
  self.status = text
  if text ~= self.last_status and self.state and self.state.set_nav_status then
    self.last_status = text
    self.state:set_nav_status(text)
  end
end

function Nav:clear(reason)
  self.mode = "off"
  self.point = nil
  if self.marker then U.safe_method(self.marker, "Show", false) end
  self:_set_status(reason or "Navigation cleared.")
end

function Nav:_current_location()
  local step = self.state:current_step()
  if not step or type(step.locations) ~= "table" or #step.locations == 0 then
    return nil, "The selected step has no candidate point (status " ..
      tostring(step and step.status or "?") .. ")."
  end
  local location = step.locations[1]
  if type(location) ~= "table" or not U.finite(location.x)
      or not U.finite(location.y) or not U.finite(location.z) then
    return nil, "The selected candidate location has no finite coordinates."
  end
  return location, nil
end

function Nav:activate_candidate()
  local location, error_message = self:_current_location()
  if not location then
    self:clear(error_message)
    return false, error_message
  end
  local form = self.state:get_form() or {}
  local declared = U.trim(form.world or "")
  if declared == "" then
    local message = "Enter and confirm the Classic world label before using a 1.2 candidate."
    self:clear(message)
    return false, message
  end
  if tostring(location.world or "") ~= declared then
    local message = "World mismatch: candidate is in '" ..
      tostring(location.world or "?") .. "', declared '" .. declared ..
      "'. Navigation stays off."
    self:clear(message)
    return false, message
  end
  self.mode = "candidate"
  self.point = {
    x = location.x, y = location.y, z = location.z,
    world = location.world, conditional = location.schedule ~= nil,
  }
  self.next_update = 0
  local suffix = location.schedule and " (conditional schedule)" or ""
  self:_set_status("Using unverified 1.2 placement in " .. declared .. suffix ..
    ". Current Classic world is unknown to the API.")
  return true, self.status
end

function Nav:_capture_unit(unit)
  local ok, values, error_message = U.safe_method_multi(
    self.api.Unit, "UnitWorldPosition", unit)
  if not ok then
    return nil, "Position unavailable for " .. unit .. ": " .. tostring(error_message)
  end
  local x, y, z = values[1], values[2], values[3]
  if not (U.finite(x) and U.finite(y) and U.finite(z)) then
    return nil, "Position unavailable for " .. unit .. " (nil/non-finite; no origin fallback)."
  end
  return {x = x, y = y, z = z}, nil
end

function Nav:capture(unit)
  local point, error_message = self:_capture_unit(unit)
  if not point then
    self:clear(error_message)
    return false, error_message
  end
  local form = self.state:get_form() or {}
  point.world = U.trim(form.world or "")
  self.mode = "observed"
  self.point = point
  self.next_update = 0
  self:_set_status("Captured observed Classic " .. tostring(unit) ..
    " waypoint at current client coordinates. This is separate from 1.2 candidates.")
  return true, self.status
end

function Nav:observed_points()
  return self.mode == "observed" and self.point or nil
end

function Nav:_player_position()
  local ok, values, error_message = U.safe_method_multi(
    self.api.Unit, "UnitWorldPosition", "player")
  if not ok then return nil, tostring(error_message) end
  local x, y, z = values[1], values[2], values[3]
  if not (U.finite(x) and U.finite(y) and U.finite(z)) then
    return nil, "player position is nil/non-finite"
  end
  return {x = x, y = y, z = z}, nil
end

local function distance(a, b)
  local dx, dy, dz = b.x - a.x, b.y - a.y, b.z - a.z
  return math.sqrt(dx * dx + dy * dy + dz * dz)
end

function Nav:_screen()
  local ok_w, width = U.safe_method(self.api.Interface, "GetScreenWidth")
  local ok_h, height = U.safe_method(self.api.Interface, "GetScreenHeight")
  if not ok_w or not ok_h or not U.finite(width) or not U.finite(height)
      or width < 200 or height < 200 then
    return nil, "screen size unavailable"
  end
  return width, height, nil
end

local function arrow_for(dx, dy)
  if math.abs(dx) >= math.abs(dy) then
    if dx >= 0 then return ">" else return "<" end
  end
  if dy >= 0 then return "v" else return "^" end
end

function Nav:update(now)
  if self.mode == "off" or not self.point then
    if self.marker then U.safe_method(self.marker, "Show", false) end
    return
  end
  if now and now < self.next_update then return end
  self.next_update = (now or 0) + UPDATE_MS
  local marker, label = self:_ensure_marker()
  if not marker or not label then
    self:_set_status(self.status)
    return
  end
  local player, player_error = self:_player_position()
  if not player then
    U.safe_method(marker, "Show", false)
    self:_set_status("Waypoint unavailable: " .. player_error)
    return
  end
  local ok, values, error_message = U.safe_method_multi(
    (self.state.helpers or {}).x2util, "ConvertWorldToScreen",
    self.point.x, self.point.y, self.point.z)
  if not ok then
    U.safe_method(marker, "Show", false)
    self:_set_status("Waypoint projection unavailable: " .. tostring(error_message))
    return
  end
  local screen_x, screen_y, projected = values[1], values[2], values[3]
  if not (U.finite(screen_x) and U.finite(screen_y) and U.finite(projected)) then
    U.safe_method(marker, "Show", false)
    self:_set_status("Waypoint projection unavailable (nil/non-finite values).")
    return
  end
  local meters = distance(player, self.point)
  local tag = self:_tag()
  if projected < 0 then
    U.safe_method(label, "SetText", "Turn to find waypoint\n" .. tag)
    local width, height = self:_screen()
    if width and height then
      U.safe_method(marker, "AddAnchor", "CENTER", "UIParent", "CENTER", 0, -80)
    else
      U.safe_method(marker, "AddAnchor", "CENTER", "UIParent", "CENTER", 0, -80)
    end
    U.safe_method(marker, "Show", true)
    self:_set_status("Waypoint is off-screen/not projectable: turn to find it. " ..
      "No direction is invented for a negative projection.")
    return
  end
  local width, height, screen_error = self:_screen()
  if not width then
    U.safe_method(marker, "Show", false)
    self:_set_status("Waypoint unavailable: " .. tostring(screen_error))
    return
  end
  local clamped_x = U.clamp(screen_x, MARGIN, width - MARGIN)
  local clamped_y = U.clamp(screen_y, MARGIN, height - MARGIN)
  local clamped = clamped_x ~= screen_x or clamped_y ~= screen_y
  local prefix = clamped
    and (arrow_for(screen_x - width / 2, screen_y - height / 2) .. " ")
    or "o "
  local conditional = self.point.conditional and " [conditional schedule]" or ""
  U.safe_method(label, "SetText", prefix .. string.format("%.0fm", meters) ..
    "  " .. tag .. conditional)
  U.safe_method(marker, "AddAnchor", "CENTER", "UIParent", "TOPLEFT",
                clamped_x, clamped_y)
  U.safe_method(marker, "Show", true)
  self:_set_status((tag == "unverified 1.2 placement"
    and "Following unverified 1.2 placement in " .. tostring(self.point.world or "?")
    or "Following observed Classic waypoint") ..
    (conditional ~= "" and " (conditional schedule)." or "."))
end

function Nav:destroy()
  if self.marker then
    U.safe_method(self.marker, "Show", false)
    U.safe_method(self.api.Interface, "Free", self.marker)
  end
  self.marker = nil
  self.label = nil
  self.mode = "off"
  self.point = nil
end

return Nav
