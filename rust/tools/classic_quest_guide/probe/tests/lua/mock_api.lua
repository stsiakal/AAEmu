-- Strict stub of the Classic addon API for the probe tests. Unknown widget
-- methods raise. State-changing API functions exist only to be counted: the
-- probe must never call them (mock.violations stays empty).

local Widget = {}
Widget.__index = function(self, key)
  local method = Widget.methods[key]
  if method then return method end
  error("unknown widget API: " .. tostring(key), 2)
end
Widget.methods = {}

local Mock = {}

local function deep_copy(value, seen)
  if type(value) ~= "table" then return value end
  seen = seen or {}
  if seen[value] then return "<cycle>" end
  seen[value] = true
  local out = {}
  for k, v in pairs(value) do out[deep_copy(k, seen)] = deep_copy(v, seen) end
  seen[value] = nil
  return out
end

function Mock.new(opts)
  opts = opts or {}
  local mock = {
    widgets = {}, order = {}, files = {}, file_order = {}, logs = {},
    ui_ms = 1000, on_callbacks = {}, reject = opts.reject or {},
    violations = {}, calls = {}, file_fail = false, settings = {},
    units = {}, bag = {}, bag_capacity = 5, currency = 777,
    quests = {}, save_settings_count = 0,
  }

  local function count(name)
    mock.calls[name] = (mock.calls[name] or 0) + 1
  end

  local function violate(name)
    mock.violations[#mock.violations + 1] = name
  end

  local function make_widget(id)
    local widget = setmetatable({
      _id = id, _handlers = {}, _events = {}, _shown = false, _text = "",
    }, Widget)
    mock.widgets[id] = widget
    mock.order[#mock.order + 1] = id
    return widget
  end

  function Widget.methods:Show(show) self._shown = show ~= false end
  function Widget.methods:SetExtent() end
  function Widget.methods:AddAnchor() end
  function Widget.methods:SetText(text) self._text = text end
  function Widget.methods:GetText() return self._text end
  function Widget.methods:SetStyle() end
  function Widget.methods:SetTitleText() end
  function Widget.methods:EnableDrag() return true end
  function Widget.methods:ReleaseHandler(action) self._handlers[action] = nil end
  function Widget.methods:StartMoving() end
  function Widget.methods:StopMovingOrSizing() end
  function Widget.methods:SetHandler(action, handler)
    self._handlers[action] = handler
  end
  function Widget.methods:RegisterEvent(name)
    count("RegisterEvent")
    if mock.reject[name] then error("event rejected: " .. name) end
    self._events[name] = true
  end
  function Widget.methods:CreateChildWidget(kind, id)
    return make_widget(id)
  end

  local interface = {}
  function interface:CreateWindow(id) return make_widget(id) end
  function interface:CreateEmptyWindow(id) return make_widget(id) end
  function interface:CreateWidget(kind, id) return make_widget(id) end
  function interface:ApplyButtonSkin() return true end
  function interface:Free(widget) widget._freed = true end
  function interface:GetScreenWidth() return 1280 end
  function interface:GetScreenHeight() return 720 end
  function interface:GetUIScale() return 1 end

  local unit = {}
  function unit:GetUnitId(token)
    local u = mock.units[token]
    return u and u.id or nil
  end
  function unit:UnitName(token) local u = mock.units[token]; return u and u.name end
  function unit:UnitInfo(token) local u = mock.units[token]; return u and u.info end
  function unit:UnitModifierInfo(token) local u = mock.units[token]; return u and u.mods end
  function unit:UnitWorldPosition(token)
    local u = mock.units[token]
    if not u then return nil end
    return u.x, u.y, u.z
  end
  function unit:GetCurrentZoneGroup() return mock.zone or 7 end
  function unit:UnitHealth(token) local u = mock.units[token]; return u and u.hp end
  function unit:UnitMaxHealth(token) local u = mock.units[token]; return u and u.max_hp end
  function unit:UnitLevel(token) local u = mock.units[token]; return u and u.level end
  function unit:GetFactionName(token)
    local u = mock.units[token]
    return u and (u.faction or (u.info and u.info.faction))
  end
  function unit:GetUnitWorldPosition(id)
    for _, u in pairs(mock.units) do
      if u.id == id then return u.x, u.y, u.z end
    end
  end
  function unit:GetNpcInfo(token) local u = mock.units[token]; return u and u.npc end
  function unit:UnitBuffCount(token)
    local u = mock.units[token]
    return u and #(u.buffs or {}) or 0
  end
  function unit:UnitBuff(token, index)
    local u = mock.units[token]
    return u and u.buffs and u.buffs[index]
  end
  function unit:UnitDeBuffCount(token)
    local u = mock.units[token]
    return u and #(u.debuffs or {}) or 0
  end
  function unit:UnitDeBuff(token, index)
    local u = mock.units[token]
    return u and u.debuffs and u.debuffs[index]
  end
  function unit:GetUnitNameById(id)
    for _, u in pairs(mock.units) do if u.id == id then return u.name end end
  end
  function unit:GetUnitInfoById(id)
    for _, u in pairs(mock.units) do
      if u.id == id then return u.by_id_info or u.info end
    end
  end
  function unit:GetUnitGradeById() return 3 end
  -- State-changing: must never be called.
  function unit:TargetUnit() violate("Unit.TargetUnit") end

  local bag = {}
  function bag:Capacity() return mock.bag_capacity end
  function bag:CountItems()
    local n = 0
    for _ in pairs(mock.bag) do n = n + 1 end
    return n
  end
  function bag:GetCurrency() return mock.currency end
  function bag:GetBagItemInfo(_, index) return mock.bag[index] end
  function bag:EquipBagItem() violate("Bag.EquipBagItem") end

  local quest = {}
  function quest:IsCompleted(id)
    count("Quest:IsCompleted")
    local q = mock.quests[id]
    return q and q.completed or false
  end
  function quest:GetActiveQuestTitle(id)
    count("Quest:GetActiveQuestTitle")
    local q = mock.quests[id]
    return q and q.active_title
  end
  function quest:GetQuestContextMainTitle(id)
    count("Quest:GetQuestContextMainTitle")
    local q = mock.quests[id]
    return q and q.main_title
  end
  function quest:GetQuestContextBody(id)
    count("Quest:GetQuestContextBody")
    local q = mock.quests[id]
    return q and q.body
  end

  local time = {}
  function time:GetUiMsec() return mock.ui_ms end
  function time:GetLocalTime()
    return {year = 2026, month = 10, day = 22, hour = 12, minute = 0, second = 0}
  end
  function time:GetGameTime() return {hour = 5, minute = 5} end

  local file = {}
  function file:Write(path, tbl)
    if mock.file_fail then error("disk full") end
    if mock.files[path] == nil then mock.file_order[#mock.file_order + 1] = path end
    mock.files[path] = deep_copy(tbl)
  end
  function file:Read() return nil end

  local log = {}
  function log:Info(message) mock.logs[#mock.logs + 1] = {"info", message} end
  function log:Err(message) mock.logs[#mock.logs + 1] = {"error", message} end

  local chat = {}
  function chat:DispatchChatMessage() violate("Chat.DispatchChatMessage") end
  function chat:ExpressEmotion() violate("Chat.ExpressEmotion") end
  local team = {}
  function team:InviteToTeam() violate("Team.InviteToTeam") end
  function team:SetRole() violate("Team.SetRole") end
  function team:IsPartyTeam() return false end
  local cursor = {}
  function cursor:ClearCursor() violate("Cursor.ClearCursor") end
  local map = {}
  function map:ToggleMapWithPortal() violate("Map.ToggleMapWithPortal") end
  function map:GetPlayerSextants() return {"a"} end

  local api = {
    Interface = interface, Unit = unit, Bag = bag, Time = time, File = file,
    Log = log, Chat = chat, Team = team, Cursor = cursor, Map = map,
    Quest = quest,
    Bank = {
      GetCurrency = function() return 55 end,
      Capacity = function() return 10 end,
    },
    Equipment = {
      GetEquippedItemTooltipInfo = function(_, slot)
        if slot == 1 then return {name = "Helm"} end
      end,
    },
  }
  function api.On(event, callback)
    count("On")
    mock.on_callbacks[event] = mock.on_callbacks[event] or {}
    table.insert(mock.on_callbacks[event], callback)
  end
  function api.GetSettings(id)
    mock.settings[id] = mock.settings[id] or {}
    return mock.settings[id]
  end
  function api.SaveSettings() mock.save_settings_count = mock.save_settings_count + 1 end

  -- Test controls -------------------------------------------------------
  function mock:update(ms)
    self.ui_ms = self.ui_ms + ms
    for _, callback in ipairs(self.on_callbacks.UPDATE or {}) do callback(ms) end
  end
  function mock:advance(ms, steps)
    for _ = 1, steps or 1 do self:update(ms) end
  end
  -- Fires a UI event the way the loader does: handler(widget, event, ...).
  function mock:emit(event, ...)
    for _, id in ipairs(self.order) do
      local widget = self.widgets[id]
      if widget._events[event] and widget._handlers.OnEvent then
        widget._handlers.OnEvent(widget, event, ...)
      end
    end
    for _, callback in ipairs(self.on_callbacks[event] or {}) do callback(...) end
  end
  function mock:click(id)
    local widget = self.widgets[id]
    widget._handlers.OnClick(widget)
  end
  function mock:text(id) return self.widgets[id]._text end
  function mock:exists(id) return self.widgets[id] ~= nil end
  function mock:set_unit(token, unit_table) self.units[token] = unit_table end
  function mock:set_quest(id, fields) self.quests[id] = fields end
  function mock:file_count() return #self.file_order end

  return api, mock
end

return Mock
