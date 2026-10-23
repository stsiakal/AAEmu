local api = require("api")

local GLOBAL_KEY = "AAEMU_QUEST_GUIDE_ACTIVE"
local MODULES = {
  "aaemu_quest_guide/core",
  "aaemu_quest_guide/ui",
  "aaemu_quest_guide/pages",
  "aaemu_quest_guide/nav",
  "aaemu_quest_guide/progress",
  "aaemu_quest_guide/checklist",
  "aaemu_quest_guide/recorder",
  "aaemu_quest_guide/catalog",
  "aaemu_quest_guide/util",
}

local instance_state = nil

local function global_env()
  return _G or getfenv(0)
end

local function active_state()
  local env = global_env()
  local state = env[GLOBAL_KEY]
  if state and state.alive then return state end
  return nil
end

local function documented_helpers()
  local env = global_env()
  return {
    w_ctrl = env.W_CTRL,
    x2util = env.X2Util,
    button_basic = env.BUTTON_BASIC,
  }
end

local function clear_module_cache()
  if type(package) ~= "table" or type(package.loaded) ~= "table" then return end
  for _, name in ipairs(MODULES) do
    package.loaded[name] = nil
  end
end

local function Load()
  local previous = active_state()
  if previous and previous.destroy then
    pcall(previous.destroy, previous)
  end
  clear_module_cache()
  local ok, core = pcall(require, "aaemu_quest_guide/core")
  if not ok then
    pcall(function()
      api.Log:Err("AAEmu Quest Guide failed to load: " .. tostring(core))
    end)
    return nil
  end
  local created, state = pcall(core.new, api, documented_helpers())
  if not created or not state then
    pcall(function()
      api.Log:Err("AAEmu Quest Guide failed to start: " .. tostring(state))
    end)
    return nil
  end
  instance_state = state
  global_env()[GLOBAL_KEY] = state
  pcall(function()
    api.Log:Info("AAEmu Quest Guide 0.2.0 loaded. Expected data 1.2; observations are Classic.")
  end)
  return state
end

local function Unload()
  local state = instance_state
  instance_state = nil
  if state and state.alive and state.destroy then
    pcall(state.destroy, state)
  end
  local env = global_env()
  if env[GLOBAL_KEY] == state then env[GLOBAL_KEY] = nil end
end

local function SettingToggle()
  local state = instance_state
  if state and state.alive then
    if state.show_main then state:show_main() end
    if state.ui then state.ui:show_main() end
  end
end

local metadata = {
  name = "AAEmu 1.2 Quest Guide",
  desc = "Browse expected 1.2 quest data, observe Classic outcomes, and opt in to unverified 1.2 waypoints.",
  author = "stsiakal",
  version = "0.2.0",
  OnLoad = Load,
  OnUnload = Unload,
  OnSettingToggle = SettingToggle,
}

return metadata
