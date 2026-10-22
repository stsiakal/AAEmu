-- Tiny status window: four status lines, a hint, Save and Stop/Start. Widget
-- use follows the patterns of the quest guide addon; every call is guarded.

local U = require("aaemu_probe/util")

local UI = {}

local HINT_1 = "Play about 10 minutes: take and turn in a quest, kill and loot,"
local HINT_2 = "use a doodad, open a merchant, mount. Then press Save."

local function safe(object, name, ...)
  if type(object) ~= "table" then return false end
  local fn
  local got = pcall(function() fn = object[name] end)
  if not got or type(fn) ~= "function" then return false end
  return pcall(fn, object, ...)
end

function UI.new(api, probe)
  local self = {api = api, probe = probe, labels = {}}
  for key, value in pairs(UI) do self[key] = value end
  self:build()
  return self
end

function UI:_child(parent, kind, id)
  local ok, widget = safe(parent, "CreateChildWidget", kind, id, 0, true)
  if ok and widget then return widget end
  local ok2, widget2 = safe(self.api.Interface, "CreateWidget", kind, id, parent)
  if ok2 and widget2 then return widget2 end
  return nil
end

function UI:_label(id, text, y)
  local label = self:_child(self.main, "label", id)
  if not label then return nil end
  safe(label, "SetExtent", 400, 16)
  safe(label, "AddAnchor", "TOPLEFT", self.main, 12, y)
  safe(label, "SetText", text)
  safe(label, "Show", true)
  return label
end

function UI:_button(id, text, x, y, handler)
  local button = self:_child(self.main, "button", id)
  if not button then return nil end
  safe(button, "SetExtent", 90, 22)
  safe(button, "AddAnchor", "TOPLEFT", self.main, x, y)
  safe(button, "SetText", text)
  local basic = self.probe.helpers.button_basic
  local skinned = false
  if type(basic) == "table" and basic.DEFAULT ~= nil then
    skinned = safe(self.api.Interface, "ApplyButtonSkin", button, basic.DEFAULT)
  end
  if not skinned then safe(button, "SetStyle", "text_default") end
  safe(button, "SetHandler", "OnClick", handler)
  safe(button, "Show", true)
  return button
end

function UI:build()
  local api = self.api
  local ok, main = safe(api.Interface, "CreateWindow", "aaemu_probe_main",
    "AAEmu probe", 0, 0)
  if not ok or not main then
    local ok2, fallback = safe(api.Interface, "CreateEmptyWindow", "aaemu_probe_main")
    if ok2 then main = fallback end
  end
  if not main then return end
  self.main = main
  safe(main, "SetExtent", 440, 190)
  safe(main, "AddAnchor", "TOPLEFT", "UIParent", 40, 160)
  safe(main, "SetTitle", "AAEmu probe")
  safe(main, "SetTitleText", "AAEmu probe")
  if safe(main, "EnableDrag", true) then
    safe(main, "SetHandler", "OnDragStart", function(widget)
      safe(widget, "StartMoving")
    end)
    safe(main, "SetHandler", "OnDragStop", function(widget)
      safe(widget, "StopMovingOrSizing")
    end)
  end
  local probe = self.probe
  for index = 1, 4 do
    self.labels[index] = self:_label("aep_status" .. index, "", 28 + (index - 1) * 18)
  end
  self.hint1 = self:_label("aep_hint1", HINT_1, 106)
  self.hint2 = self:_label("aep_hint2", HINT_2, 124)
  self.save_button = self:_button("aep_save", "Save", 12, 152, function()
    probe:save()
    probe:refresh_ui()
  end)
  self.toggle_button = self:_button("aep_toggle", "Stop", 112, 152, function()
    probe:toggle()
  end)
  safe(main, "Show", true)
end

function UI:refresh(probe)
  if not self.main then return end
  local lines = probe:status_lines()
  for index, label in ipairs(self.labels) do
    safe(label, "SetText", U.truncate_utf8(lines[index] or "", 90))
  end
  safe(self.toggle_button, "SetText", probe.active and "Stop" or "Start")
end

function UI:show()
  safe(self.main, "Show", true)
end

function UI:destroy()
  if not self.main then return end
  safe(self.main, "Show", false)
  safe(self.api.Interface, "Free", self.main)
  self.main = nil
end

return UI
