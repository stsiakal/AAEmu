-- Loads the real main.lua the way the Classic loader does: main.lua and every
-- required module run in an environment that holds ONLY the globals the real
-- sandbox (sandbox.lua CreateAddonEnv) provides. Anything else is nil.

local MockApi = require("mock_api")

local Harness = {}
Harness.__index = Harness

-- Exact allowlist, from the extracted sandbox.alb (L858-L1221).
Harness.SANDBOX_GLOBALS = {
  "api", "print", "string", "table", "math", "pairs", "ipairs", "tonumber",
  "tostring", "type", "pcall", "xpcall", "unpack", "baseDir", "getmetatable",
  "ADDON", "BUTTON_BASIC", "UIC", "ALIGN", "W_CTRL", "W_ICON", "W_UNIT",
  "W_ETC", "W_BAR", "W_MONEY", "W_BTN", "EQUIP_SLOT", "CHAT_MESSAGE_FILTERS",
  "UI_EVENTS", "require", "X2Util", "X2Hotkey", "X2Player",
}

local function event_names(base_dir)
  local chunk = assert(loadfile(base_dir .. "/aaemu_probe/events.lua"))
  local names = chunk()
  local out = {}
  for _, name in ipairs(names) do out[name] = name end
  return out
end

function Harness.new(opts)
  opts = opts or {}
  local api, mock = MockApi.new(opts)
  local self = setmetatable({
    api = api, mock = mock, main_path = opts.main_path,
    base_dir = opts.base_dir,
  }, Harness)
  self.loaded = {}
  self.sandbox = {
    api = api, print = print, string = string, table = table, math = math,
    pairs = pairs, ipairs = ipairs, tonumber = tonumber, tostring = tostring,
    type = type, pcall = pcall, xpcall = xpcall, unpack = unpack,
    getmetatable = getmetatable, baseDir = opts.base_dir,
    BUTTON_BASIC = {DEFAULT = {}}, UIC = {}, ALIGN = {}, ADDON = {},
    EQUIP_SLOT = {}, CHAT_MESSAGE_FILTERS = {},
    UI_EVENTS = event_names(opts.base_dir),
  }
  self:_load_metadata()
  return self
end

-- Mirrors sandbox.lua L1233-1253: "api" returns the api; other names are
-- loadfile()d from <base>/<name>.lua into a fresh env that falls back to the
-- addon env; results are cached.
function Harness:_make_env()
  local env = {}
  local sandbox = self.sandbox
  local base_dir = self.base_dir
  local loaded = self.loaded
  env.require = function(name)
    if name == "api" then return sandbox.api end
    if type(name) ~= "string" or name:find("..", 1, true) then
      error("Invalid module name")
    end
    if loaded[name] == nil then
      local chunk, err = loadfile(base_dir .. "/" .. name .. ".lua")
      if not chunk then error("Error loading file " .. tostring(err)) end
      local module_env = setmetatable({}, {__index = env, __metatable = false})
      setfenv(chunk, module_env)
      loaded[name] = chunk()
    end
    return loaded[name]
  end
  return setmetatable(env, {__index = sandbox, __metatable = false})
end

function Harness:_load_metadata()
  self.loaded = {}
  local chunk = assert(loadfile(self.main_path))
  setfenv(chunk, self:_make_env())
  self.metadata = chunk()
  return self.metadata
end

function Harness:load()
  self.instance = self.metadata.OnLoad()
  return self.instance
end

function Harness:unload() return self.metadata.OnUnload() end

-- The loader calls OnUnload on the old addon before loading main.lua again.
function Harness:reload()
  self:unload()
  self:_load_metadata()
  return self:load()
end

function Harness:probe() return self.instance end

return Harness
