-- Loads the real main.lua metadata against the strict mock.

local MockApi = require("mock_api")

local Harness = {}
Harness.__index = Harness

function Harness.new(opts)
  opts = opts or {}
  local api, mock = MockApi.new(opts)
  package.preload["api"] = function() return api end
  local self = setmetatable({api = api, mock = mock, main_path = opts.main_path}, Harness)
  self:_load_metadata()
  return self
end

function Harness:_load_metadata()
  local chunk = assert(loadfile(self.main_path))
  self.metadata = chunk()
  return self.metadata
end

function Harness:load() return self.metadata.OnLoad() end
function Harness:unload() return self.metadata.OnUnload() end
function Harness:reload()
  self:_load_metadata()
  return self:load()
end
function Harness:probe() return _G.AAEMU_PROBE_ACTIVE end

return Harness
