-- Strict mock API used by the Lua 5.1 tests. Unknown widget methods raise so the
-- runtime cannot silently depend on undocumented calls. W_CTRL/X2Util are only
-- installed as documented globals, never on the api table.

local Widget = {}
Widget.__index = function(self, key)
  local method = Widget.methods[key]
  if method then return method end
  error("unknown widget API: " .. tostring(key), 2)
end

local ALLOWED_TYPES = {
  label = true, button = true, window = true, editbox = true,
  textbox = true, checkbutton = true,
}
local ALLOWED_ACTIONS = {
  OnClick = true, OnDragStart = true, OnDragStop = true,
  OnTextChanged = true, OnEnterPressed = true, OnShow = true, OnHide = true,
  OnWheelUp = true, OnWheelDown = true, OnEvent = true,
}

local Mock = {}

local function copy(value, seen)
  if type(value) ~= "table" then return value end
  seen = seen or {}
  if seen[value] then return "<cycle>" end
  seen[value] = true
  local out = {}
  for key, item in pairs(value) do out[copy(key, seen)] = copy(item, seen) end
  seen[value] = nil
  return out
end

function Mock.new(opts)
  opts = opts or {}
  local mock = {
    widgets = {}, order = {}, callbacks = {}, files = {}, writes = {},
    settings = {}, settings_saves = 0, logs = {}, ui_ms = 1000,
    projection = {sx = 300, sy = 200, dist = 25},
    units = {},
    completed = {}, titles = {}, active_titles = {}, bodies = {},
    currency = 123, item_counts = {}, item_count = 7, capacity = 100,
    file_fail = false, settings_fail = false, skin_calls = 0,
    global_wctrl = not opts.no_wctrl,
    global_x2 = not opts.no_x2,
  }

  local function register(widget)
    if widget._freed then error("widget freed") end
    if mock.widgets[widget._id] then
      error("duplicate widget id: " .. tostring(widget._id))
    end
    mock.widgets[widget._id] = widget
    mock.order[#mock.order + 1] = widget._id
    return widget
  end

  local function free_widget(widget)
    if not widget or widget._freed then return end
    widget._freed = true
    for _, child in ipairs(widget._children or {}) do free_widget(child) end
    if mock.widgets[widget._id] == widget then
      mock.widgets[widget._id] = nil
    end
  end

  local function make_widget(widget_type, id, parent)
    if not ALLOWED_TYPES[widget_type] then
      error("unsupported widget type: " .. tostring(widget_type))
    end
    if not id or id == "" then error("widget id required") end
    if mock.widgets[id] then error("duplicate widget id: " .. tostring(id)) end
    local widget = setmetatable({
      _id = id, _type = widget_type, _parent = parent,
      _children = {}, _handlers = {}, _text = "", _shown = false,
      _extent = {0, 0}, _anchors = {}, _freed = false,
    }, Widget)
    if parent then parent._children[#parent._children + 1] = widget end
    register(widget)
    return widget
  end

  Widget.methods = {}
  function Widget.methods:Show(show)
    self._shown = show and true or false
  end
  function Widget.methods:SetExtent(width, height)
    if type(width) ~= "number" or type(height) ~= "number" then
      error("SetExtent expects numbers")
    end
    self._extent = {width, height}
  end
  function Widget.methods:AddAnchor(...)
    local count = select("#", ...)
    if count < 3 then error("AddAnchor requires a parent anchor") end
    self._anchors[#self._anchors + 1] = {...}
  end
  function Widget.methods:SetText(text)
    self._text = tostring(text or "")
  end
  function Widget.methods:GetText()
    return self._text
  end
  function Widget.methods:SetHandler(action, handler)
    if not ALLOWED_ACTIONS[action] then
      error("unsupported handler: " .. tostring(action))
    end
    if type(handler) ~= "function" then error("handler must be a function") end
    self._handlers[action] = handler
  end
  function Widget.methods:SetTitleText(text)
    self._title = tostring(text or "")
  end
  function Widget.methods:SetStyle(style)
    if type(style) ~= "string" then error("style must be a string") end
  end
  function Widget.methods:EnableDrag(enable)
    self._drag = enable and true or false
  end
  function Widget.methods:StartMoving() self._moving = true end
  function Widget.methods:StopMovingOrSizing() self._moving = false end
  function Widget.methods:CreateChildWidget(widget_type, id, _index, _auto)
    return make_widget(widget_type, id, self)
  end
  function Widget.methods:RegisterEvent(name)
    if type(name) ~= "string" or name == "" then
      error("RegisterEvent requires an event name")
    end
    local events = rawget(self, "_events") or {}
    rawset(self, "_events", events)
    events[name] = true
    return true
  end

  local interface = {}
  function interface:CreateWindow(id, title, _x, _y, _tabs)
    local widget = make_widget("window", id, nil)
    widget._title = title
    return widget
  end
  function interface:CreateEmptyWindow(id)
    return make_widget("window", id, nil)
  end
  function interface:CreateWidget(widget_type, id, parent)
    return make_widget(widget_type, id, parent)
  end
  function interface:ApplyButtonSkin(button, skin)
    if not mock.widgets[button._id] then error("button is not registered") end
    mock.skin_calls = mock.skin_calls + 1
    button._skin = skin
  end
  function interface:Free(widget)
    free_widget(widget)
  end
  function interface:GetScreenWidth() return 1280 end
  function interface:GetScreenHeight() return 720 end
  function interface:GetUIScale() return 1 end

  local unit_api = {}
  local function unit(name)
    return mock.units[name]
  end
  local function by_id(id)
    for _, value in pairs(mock.units) do
      if value.id == id then return value end
    end
    return nil
  end
  function unit_api:GetUnitId(name)
    local value = unit(name)
    if not value then return nil end
    return value.id
  end
  function unit_api:GetUnitNameById(id)
    local value = by_id(id)
    if not value then return nil end
    return value.name
  end
  function unit_api:UnitWorldPosition(name)
    local value = unit(name)
    if not value then return nil end
    if value.pos_error then error(value.pos_error) end
    if not value.pos then return nil end
    return value.pos.x, value.pos.y, value.pos.z
  end
  function unit_api:UnitInfo(name)
    local value = unit(name)
    if not value then return nil end
    if value.info_error then error(value.info_error) end
    return value.info
  end
  function unit_api:UnitHealth(name)
    local value = unit(name); return value and value.health or nil
  end
  function unit_api:UnitMaxHealth(name)
    local value = unit(name); return value and value.max_health or nil
  end
  function unit_api:UnitMana(name)
    local value = unit(name); return value and value.mana or nil
  end
  function unit_api:UnitMaxMana(name)
    local value = unit(name); return value and value.max_mana or nil
  end
  function unit_api:UnitBuffCount(name)
    local value = unit(name); return value and #(value.buffs or {}) or nil
  end
  function unit_api:UnitBuff(name, index)
    local value = unit(name); return value and value.buffs and value.buffs[index] or nil
  end
  function unit_api:UnitDeBuffCount(name)
    local value = unit(name); return value and #(value.debuffs or {}) or nil
  end
  function unit_api:UnitDeBuff(name, index)
    local value = unit(name); return value and value.debuffs and value.debuffs[index] or nil
  end

  local quest_api = {}
  function quest_api:IsCompleted(id)
    if mock.completed_error then error(mock.completed_error) end
    return mock.completed[id]
  end
  function quest_api:GetQuestContextMainTitle(id) return mock.titles[id] end
  function quest_api:GetActiveQuestTitle(id) return mock.active_titles[id] end
  function quest_api:GetQuestContextBody(id) return mock.bodies[id] end

  local time_api = {}
  function time_api:GetUiMsec() return mock.ui_ms end
  function time_api:GetLocalTime()
    return {year = 2026, month = 10, day = 19, hour = 12, min = 0, sec = 0}
  end
  function time_api:GetGameTime()
    return {hour = 8, min = 30, sec = 0}
  end

  local bag_api = {}
  function bag_api:GetCurrency() return mock.currency end
  function bag_api:CountItems() return mock.item_count end
  function bag_api:Capacity() return mock.capacity end
  function bag_api:CountBagItemByItemType(item_type)
    return mock.item_counts[item_type] or 0
  end

  local file_api = {}
  function file_api:Write(path, value)
    if type(path) ~= "string" or not path:match("^aaemu_quest_guide/observations/") then
      error("observation path outside aaemu_quest_guide/observations: " .. tostring(path))
    end
    if mock.file_fail == "error" then error("mock disk error") end
    if mock.file_fail == true then return false end
    if mock.files[path] then error("mock refuses to overwrite " .. path) end
    local payload = copy(value)
    mock.files[path] = payload
    mock.writes[#mock.writes + 1] = {path = path, payload = payload}
    return nil
  end
  function file_api:Read(path)
    if not mock.files[path] then return nil end
    return copy(mock.files[path])
  end

  local log_api = {}
  function log_api:Info(message) mock.logs[#mock.logs + 1] = {kind = "info", text = message} end
  function log_api:Err(message) mock.logs[#mock.logs + 1] = {kind = "error", text = message} end

  local api = {
    Interface = interface,
    Unit = unit_api,
    Quest = quest_api,
    Time = time_api,
    Bag = bag_api,
    File = file_api,
    Log = log_api,
  }
  function api.On(event, callback)
    if type(event) ~= "string" or type(callback) ~= "function" then
      error("On(event, callback) invalid")
    end
    mock.callbacks[event] = mock.callbacks[event] or {}
    mock.callbacks[event][#mock.callbacks[event] + 1] = callback
  end
  function api.GetSettings(addon_id)
    if addon_id ~= "aaemu_quest_guide" then return {} end
    return mock.settings
  end
  function api.SaveSettings()
    mock.settings_saves = mock.settings_saves + 1
    if mock.settings_fail then return false end
    return nil
  end
  api.DoIn = function() end
  api.Emit = function() end

  local w_ctrl = {}
  function w_ctrl.CreateEdit(id, parent)
    return parent:CreateChildWidget("editbox", id, 0, true)
  end

  local x2util = {}
  function x2util:ConvertWorldToScreen(_x, _y, _z)
    local projection = mock.projection
    if type(projection) == "function" then return projection(_x, _y, _z) end
    if not projection then return nil end
    if projection.error then error(projection.error) end
    return projection.sx, projection.sy, projection.dist
  end

  if mock.global_wctrl then _G.W_CTRL = w_ctrl end
  if mock.global_x2 then _G.X2Util = x2util end
  _G.BUTTON_BASIC = {DEFAULT = "default"}
  if opts.no_wctrl then _G.W_CTRL = nil end
  if opts.no_x2 then _G.X2Util = nil end

  function mock:widget(id) return self.widgets[id] end
  function mock:exists(id) return self.widgets[id] ~= nil end
  function mock:shown(id)
    local widget = self.widgets[id]
    return widget and widget._shown or false
  end
  function mock:text(id)
    local widget = self.widgets[id]
    if not widget then return nil end
    return widget._text
  end
  function mock:set_text(id, text)
    local widget = self.widgets[id]
    if not widget then error("missing widget: " .. tostring(id)) end
    widget:SetText(text)
  end
  function mock:fire(id, action, ...)
    local widget = self.widgets[id]
    if not widget then return false, "missing widget" end
    local handler = widget._handlers[action]
    if not handler then return false, "no handler " .. tostring(action) end
    local ok, error_message = pcall(handler, widget, ...)
    if not ok then error(error_message) end
    return true
  end
  function mock:click(id) return self:fire(id, "OnClick") end
  function mock:enter(id) return self:fire(id, "OnEnterPressed") end
  function mock:update(ms)
    self.ui_ms = self.ui_ms + (ms or 1000)
    local callbacks = self.callbacks.UPDATE or {}
    for _, callback in ipairs(callbacks) do
      local ok, error_message = pcall(callback, ms or 1000)
      if not ok then error(error_message) end
    end
  end
  function mock:advance(ms, count)
    for _ = 1, count or 1 do self:update(ms or 1000) end
  end
  function mock:event(name, args)
    self.event_log = self.event_log or {}
    self.event_log[#self.event_log + 1] = name
    args = args or {}
    local delivered = 0
    for _, widget in pairs(self.widgets) do
      local events = rawget(widget, "_events")
      if events and events[name] then
        local handler = widget._handlers.OnEvent
        if handler then
          handler(widget, name, unpack(args))
          delivered = delivered + 1
        end
      end
    end
    return delivered
  end
  function mock:set_unit(name, value) self.units[name] = value end
  function mock:remove_unit(name) self.units[name] = nil end
  function mock:set_projection(sx, sy, dist)
    self.projection = {sx = sx, sy = sy, dist = dist}
  end
  function mock:set_projection_error(message)
    self.projection = {error = message}
  end
  function mock:set_file_fail(mode) self.file_fail = mode end
  function mock:set_settings_fail(mode) self.settings_fail = mode end
  function mock:reset_pending()
    _G.AAEMU_QUEST_GUIDE_PENDING_WRITES = {}
  end
  function mock:pending_count()
    local count = 0
    for _ in pairs(_G.AAEMU_QUEST_GUIDE_PENDING_WRITES or {}) do count = count + 1 end
    return count
  end
  function mock:file_count()
    local count = 0
    for _ in pairs(self.files) do count = count + 1 end
    return count
  end

  return api, mock
end

return Mock
