-- Harness for real main.lua under the strict mock. Paths are configured by the
-- Python tests through package.path; no machine-specific path is embedded here.

local MockApi = require("mock_api")

local Harness = {}
Harness.__index = Harness

function Harness.new(opts)
  opts = opts or {}
  local api, mock = MockApi.new(opts)
  package.preload["api"] = function() return api end
  local self = setmetatable({
    api = api,
    mock = mock,
    main_path = opts.main_path,
    metadata = nil,
    loads = 0,
  }, Harness)
  self:_load_metadata()
  return self
end

function Harness:_load_metadata()
  local chunk = assert(loadfile(self.main_path))
  self.metadata = chunk()
  return self.metadata
end

function Harness:load()
  self.loads = self.loads + 1
  local result = self.metadata.OnLoad()
  return result
end

function Harness:unload()
  return self.metadata.OnUnload()
end

function Harness:setting()
  if self.metadata.OnSettingToggle then return self.metadata.OnSettingToggle() end
end

-- Simulate an addon reload that asks for a new main.lua metadata object.
function Harness:reload()
  self:_load_metadata()
  return self:load()
end

function Harness:update(ms)
  self.mock:update(ms or 1000)
end

function Harness:advance(ms, count)
  self.mock:advance(ms or 1000, count or 1)
end

function Harness:click(id)
  return self.mock:click(id)
end

function Harness:exists(id)
  return self.mock:exists(id)
end

function Harness:shown(id)
  return self.mock:shown(id)
end

function Harness:text(id)
  return self.mock:text(id)
end

function Harness:set_text(id, text)
  self.mock:set_text(id, text)
end

function Harness:enter(id)
  return self.mock:enter(id)
end

function Harness:active_state()
  return _G.AAEMU_QUEST_GUIDE_ACTIVE
end

function Harness:reset_pending()
  self.mock:reset_pending()
end

return Harness
