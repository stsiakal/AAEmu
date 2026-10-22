-- AAEmu probe entry point.
--
-- The Classic addon sandbox (sandbox.lua CreateAddonEnv) gives an addon only:
--   api, print, string, table, math, pairs, ipairs, tonumber, tostring, type,
--   pcall, xpcall, unpack, getmetatable, require, baseDir, and the game
--   constants (ADDON, UIC, ALIGN, W_*, BUTTON_BASIC, UI_EVENTS, EQUIP_SLOT, ...).
-- It has NO _G, getfenv, setfenv, setmetatable, select, error, os, io or
-- package. Nothing in this addon may touch those names.

local VERSION = "0.1.1"
local BOOT_DIR = "aaemu_probe/observations/"

local api = nil
local instance = nil

-- Boot trail: each step writes its own tiny file so a failure can never be
-- silent. Never raises.
local function boot(step, err, extra)
  pcall(function()
    if not api then return end
    local record = {
      step = step, version = VERSION, ok = err == nil,
      err = err ~= nil and tostring(err) or nil, extra = extra,
    }
    api.File:Write(BOOT_DIR .. "boot_" .. tostring(step) .. ".lua", record)
  end)
end

local function chat(text, is_error)
  pcall(function()
    if is_error then
      api.Log:Err(text)
    else
      api.Log:Info(text)
    end
  end)
end

-- Existence/type of names the sandbox may or may not provide.
local function global_types()
  return {
    UI_EVENTS = type(UI_EVENTS), ADDON = type(ADDON), UIC = type(UIC),
    UIParent = type(UIParent), ALIGN = type(ALIGN),
    BUTTON_BASIC = type(BUTTON_BASIC), EQUIP_SLOT = type(EQUIP_SLOT),
    CHAT_MESSAGE_FILTERS = type(CHAT_MESSAGE_FILTERS),
    X2Util = type(X2Util), X2Player = type(X2Player), X2Unit = type(X2Unit),
    X2Quest = type(X2Quest), X2Chat = type(X2Chat), X2Bag = type(X2Bag),
    X2Time = type(X2Time), X2House = type(X2House), X2Map = type(X2Map),
    X2Hotkey = type(X2Hotkey),
    W_CTRL = type(W_CTRL), W_ICON = type(W_ICON), W_UNIT = type(W_UNIT),
    W_ETC = type(W_ETC), W_BAR = type(W_BAR), W_MONEY = type(W_MONEY),
    W_BTN = type(W_BTN),
  }
end

local function sandbox_types()
  return {
    pcall = type(pcall), xpcall = type(xpcall), unpack = type(unpack),
    getmetatable = type(getmetatable), setmetatable = type(setmetatable),
    select = type(select), error = type(error), assert = type(assert),
    rawget = type(rawget), next = type(next), loadstring = type(loadstring),
    loadfile = type(loadfile), getfenv = type(getfenv), setfenv = type(setfenv),
    _G = type(_G), os = type(os), io = type(io), debug = type(debug),
    package = type(package), baseDir = type(baseDir), require = type(require),
  }
end

-- File scope: first statement that does anything.
do
  local ok, result = pcall(require, "api")
  if ok then api = result end
  boot("0", (not ok) and result or nil, {
    api_type = type(api),
    globals = global_types(), sandbox = sandbox_types(),
  })
  chat("AAEmu probe " .. VERSION .. ": main.lua read (boot_0).")
end

local function Load()
  boot("1", nil, {note = "OnLoad entered"})
  if not api then
    boot("1", "api unavailable")
    return nil
  end
  local ok_core, core = pcall(require, "aaemu_probe/core")
  if not ok_core then
    boot("err", "core require failed: " .. tostring(core))
    chat("AAEmu probe failed to load: " .. tostring(core), true)
    return nil
  end
  local helpers = {
    button_basic = BUTTON_BASIC,
    ui_events = UI_EVENTS,
    globals = global_types(),
    sandbox_globals = sandbox_types(),
    boot = boot,
  }
  local created, probe = pcall(core.new, api, helpers)
  if not created or not probe then
    boot("err", "core.new failed: " .. tostring(probe))
    chat("AAEmu probe failed to start: " .. tostring(probe), true)
    return nil
  end
  local ok_ui, ui_module = pcall(require, "aaemu_probe/ui")
  if ok_ui then
    local built, ui = pcall(ui_module.new, api, probe)
    if built and ui then
      probe.ui = ui
      boot("2", ui.main == nil and "window not created" or nil,
        {window = ui.main ~= nil})
      probe:refresh_ui()
    else
      probe.last_error = "window failed: " .. tostring(ui)
      boot("2", probe.last_error)
    end
  else
    probe.last_error = "ui module failed: " .. tostring(ui_module)
    boot("2", probe.last_error)
  end
  instance = probe
  boot("4", nil, {session = probe.session, last_error = probe.last_error})
  chat("AAEmu probe " .. VERSION .. " loaded (passive, session " ..
    tostring(probe.session) .. ").")
  return probe
end

local function Unload()
  local probe = instance
  instance = nil
  if probe and probe.alive then pcall(probe.destroy, probe) end
end

local function SettingToggle()
  if instance and instance.alive and instance.ui then instance.ui:show() end
end

return {
  name = "AAEmu probe",
  desc = "Passive recorder that logs which events and API values the client exposes.",
  author = "stsiakal",
  version = VERSION,
  OnLoad = Load,
  OnUnload = Unload,
  OnSettingToggle = SettingToggle,
}
