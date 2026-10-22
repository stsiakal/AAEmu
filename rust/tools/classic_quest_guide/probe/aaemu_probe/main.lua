local api = require("api")

local GLOBAL_KEY = "AAEMU_PROBE_ACTIVE"
local MODULES = {
  "aaemu_probe/core", "aaemu_probe/ui", "aaemu_probe/util",
  "aaemu_probe/getters", "aaemu_probe/events",
}

local instance = nil

local function global_env()
  return _G or getfenv(0)
end

local function clear_module_cache()
  if type(package) ~= "table" or type(package.loaded) ~= "table" then return end
  for _, name in ipairs(MODULES) do package.loaded[name] = nil end
end

local function Load()
  local env = global_env()
  local previous = env[GLOBAL_KEY]
  if previous and previous.alive then pcall(previous.destroy, previous) end
  clear_module_cache()
  local ok, core = pcall(require, "aaemu_probe/core")
  if not ok then
    pcall(function() api.Log:Err("AAEmu probe failed to load: " .. tostring(core)) end)
    return nil
  end
  local helpers = {button_basic = env.BUTTON_BASIC}
  local created, probe = pcall(core.new, api, helpers)
  if not created or not probe then
    pcall(function() api.Log:Err("AAEmu probe failed to start: " .. tostring(probe)) end)
    return nil
  end
  local ok_ui, ui_module = pcall(require, "aaemu_probe/ui")
  if ok_ui then
    local built, ui = pcall(ui_module.new, api, probe)
    if built then
      probe.ui = ui
      probe:refresh_ui()
    else
      probe.last_error = "window failed: " .. tostring(ui)
    end
  else
    probe.last_error = "ui module failed: " .. tostring(ui_module)
  end
  instance = probe
  env[GLOBAL_KEY] = probe
  pcall(function()
    api.Log:Info("AAEmu probe " .. core.VERSION .. " loaded (passive, session " ..
      tostring(probe.session) .. ").")
  end)
  return probe
end

local function Unload()
  local probe = instance
  instance = nil
  if probe and probe.alive then pcall(probe.destroy, probe) end
  local env = global_env()
  if env[GLOBAL_KEY] == probe then env[GLOBAL_KEY] = nil end
end

local function SettingToggle()
  if instance and instance.alive and instance.ui then instance.ui:show() end
end

return {
  name = "AAEmu probe",
  desc = "Passive recorder that logs which events and API values the client exposes.",
  author = "stsiakal",
  version = "0.1.0",
  OnLoad = Load,
  OnUnload = Unload,
  OnSettingToggle = SettingToggle,
}
