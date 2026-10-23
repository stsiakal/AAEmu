-- Classic UI. Widget use is limited to the documented/example patterns and
-- every optional style/anchor helper is guarded.

local U = require("aaemu_quest_guide/util")
local Pages = require("aaemu_quest_guide/pages")

local UI = {}
UI.__index = UI

local PAGES = {
  {"step", "Step"},
  {"req", "Requirements"},
  {"rewards", "Rewards"},
  {"sources", "Sources"},
  {"branches", "Branches"},
  {"info", "Info"},
}
local PAGE_BY_KEY = {
  step = "step", req = "requirements", rewards = "rewards",
  sources = "sources", branches = "branches", info = "info",
}
local TEXT_WIDTH = 96
local CONTENT_LINES = 18
local CHECKLIST_ROWS = 16
local CHECKLIST_ROW_HEIGHT = 18

local function is_widget(value)
  return type(value) == "table"
end

function UI.new(api, state)
  local self = setmetatable({
    api = api,
    state = state,
    helpers = state.helpers or {},
    main = nil,
    toggle = nil,
    handlers_ok = true,
    content = nil,
    content_page = 1,
    content_pages = 1,
    form = {server = "", context = "", build = "", world = "", note = ""},
    status = "Ready.",
    nav_status = "",
    rec_status = "Recording off.",
    completion_text = "unknown",
    checklist_row_buttons = {},
    checklist_go_buttons = {},
    filter_buttons = {},
    event_sink = nil,
    events_ok = false,
  }, UI)
  self:build()
  self:load_form((state.settings or {}).form or {})
  self:refresh_all()
  return self
end

function UI:_child(parent, widget_type, id)
  if not is_widget(parent) then return nil end
  local ok, widget = U.safe_method(parent, "CreateChildWidget", widget_type, id, 0, true)
  if ok and widget then return widget end
  local ok2, widget2 = U.safe_method(self.api.Interface, "CreateWidget",
                                     widget_type, id, parent)
  if ok2 and widget2 then return widget2 end
  return nil
end

function UI:_label(parent, id, text, x, y, width, height)
  local label = self:_child(parent, "label", id)
  if not label then return nil end
  U.safe_method(label, "SetExtent", width or 200, height or 16)
  U.safe_method(label, "AddAnchor", "TOPLEFT", parent, "TOPLEFT", x or 0, y or 0)
  U.safe_method(label, "SetText", text or "")
  U.safe_method(label, "Show", true)
  return label
end

function UI:_button(parent, id, text, x, y, width, height, handler)
  local button = self:_child(parent, "button", id)
  if not button then return nil end
  U.safe_method(button, "SetExtent", width or 70, height or 22)
  U.safe_method(button, "AddAnchor", "TOPLEFT", parent, "TOPLEFT", x or 0, y or 0)
  U.safe_method(button, "SetText", text or "")
  local basic = self.helpers.button_basic
  local skin = type(basic) == "table" and basic.DEFAULT or nil
  local skinned = false
  if skin ~= nil then
    skinned = U.safe_method(self.api.Interface, "ApplyButtonSkin", button, skin)
  end
  if not skinned then
    U.safe_method(button, "SetStyle", "text_default")
  end
  local ok, _ = U.safe_method(button, "SetHandler", "OnClick", handler)
  if not ok then self.handlers_ok = false end
  U.safe_method(button, "Show", true)
  return button
end

function UI:_edit(parent, id, x, y, width, height)
  local edit = nil
  local widget_api = self.helpers.w_ctrl
  if type(widget_api) == "table" and type(widget_api.CreateEdit) == "function" then
    local ok, result = pcall(widget_api.CreateEdit, id, parent)
    if ok then edit = result end
  end
  if not edit then return nil end
  U.safe_method(edit, "SetExtent", width or 100, height or 22)
  U.safe_method(edit, "AddAnchor", "TOPLEFT", parent, "TOPLEFT", x or 0, y or 0)
  U.safe_method(edit, "SetText", "")
  U.safe_method(edit, "Show", true)
  return edit
end

function UI:_bind(widget, action, handler)
  if not widget then return false end
  local ok, _ = U.safe_method(widget, "SetHandler", action, handler)
  if not ok then self.handlers_ok = false end
  return ok
end

function UI:build()
  local api = self.api
  local created, main, error_message = U.safe_method(
    api.Interface, "CreateWindow", "aqg_main",
    "AAEmu 1.2 Quest Guide", 0, 0)
  if not created or not main then
    local fallback_ok, fallback = U.safe_method(
      api.Interface, "CreateEmptyWindow", "aqg_main")
    if fallback_ok and fallback then main = fallback end
  end
  if not main then
    self.status = "Main window unavailable: " .. tostring(error_message)
    return
  end
  self.main = main
  U.safe_method(main, "SetExtent", 680, 615)
  U.safe_method(main, "AddAnchor", "CENTER", "UIParent", "CENTER", 0, 0)
  U.safe_method(main, "SetTitleText", "AAEmu 1.2 Quest Guide")
  if U.safe_method(main, "EnableDrag", true) then
    self:_bind(main, "OnDragStart", function(self_widget)
      U.safe_method(self_widget, "StartMoving")
    end)
    self:_bind(main, "OnDragStop", function(self_widget)
      U.safe_method(self_widget, "StopMovingOrSizing")
    end)
  end

  self.status_label = self:_label(main, "aqg_status", "Ready.", 8, 6, 660)
  self.id_edit = self:_edit(main, "aqg_quest_id", 8, 26, 88, 22)
  self:_button(main, "aqg_open", "Open", 100, 26, 48, 22,
    function() self.state:open_from_edit() end)
  self:_button(main, "aqg_prev_quest", "Prev Quest", 152, 26, 70, 22,
    function() self.state:previous_quest() end)
  self:_button(main, "aqg_next_quest", "Next Quest", 226, 26, 70, 22,
    function() self.state:next_quest() end)
  self:_button(main, "aqg_route", "Route 330>6198", 300, 26, 96, 22,
    function() self.state:open_route() end)
  self:_button(main, "aqg_refresh", "Refresh", 400, 26, 62, 22,
    function() self.state:refresh_completion() end)
  self:_button(main, "aqg_hide", "Hide", 466, 26, 46, 22,
    function() self:hide_main() end)
  self:_button(main, "aqg_checklist", "Checklist", 516, 26, 80, 22,
    function() self.state:open_checklist() end)
  self.go_giver_button = self:_button(main, "aqg_go_giver", "Go to giver",
    600, 26, 76, 22, function() self.state:go_to_giver() end)

  self.filter_edit = self:_edit(main, "aqg_filter", 8, 50, 150, 22)
  self:_button(main, "aqg_find", "Find", 162, 50, 46, 22,
    function() self.state:find() end)
  self.filter_status = self:_label(main, "aqg_filter_status", "All quests.", 214, 53, 450)
  self.filter_buttons = {
    self:_button(main, "aqg_cl_open", "Not completed", 214, 50, 84, 22,
      function() self.state:set_checklist_filter("open") end),
    self:_button(main, "aqg_cl_done", "Completed", 302, 50, 68, 22,
      function() self.state:set_checklist_filter("done") end),
    self:_button(main, "aqg_cl_all", "All", 374, 50, 32, 22,
      function() self.state:set_checklist_filter("all") end),
    self:_button(main, "aqg_cl_placeholders", "Hide placeholders", 410, 50, 112, 22,
      function() self.state:toggle_checklist_placeholders() end),
    self:_button(main, "aqg_cl_ref", "Hide not in 3.0", 526, 50, 104, 22,
      function() self.state:toggle_checklist_ref() end),
  }

  local tab_x = 8
  for _, page in ipairs(PAGES) do
    local key = page[1]
    self:_button(main, "aqg_page_" .. key, page[2], tab_x, 74, 76, 20,
      function() self.state:set_page(key) end)
    tab_x = tab_x + 80
  end
  self:_button(main, "aqg_prev_text", "<", 488, 74, 30, 20,
    function() self.state:previous_text_page() end)
  self:_button(main, "aqg_next_text", ">", 520, 74, 30, 20,
    function() self.state:next_text_page() end)
  self.page_label = self:_label(main, "aqg_pageinfo", "", 554, 76, 118)

  self.meta_label = self:_label(main, "aqg_meta", "", 8, 96, 496)
  self.mark_done_button = self:_button(main, "aqg_mark_done", "Mark done",
    508, 96, 78, 18, function() self.state:mark_done() end)
  self.mark_undo_button = self:_button(main, "aqg_mark_undo", "Unmark",
    590, 96, 78, 18, function() self.state:unmark_done() end)
  self.header_label = self:_label(main, "aqg_header", "Expected data 1.2 | Observed Classic",
    8, 114, 660)
  self.content = self:_label(main, "aqg_content", "", 8, 134, 660, 300)
  for index = 1, CHECKLIST_ROWS do
    local y = 134 + (index - 1) * CHECKLIST_ROW_HEIGHT
    local row_index = index
    self.checklist_row_buttons[index] = self:_button(
      main, "aqg_cl_row_" .. tostring(index), "", 8, y, 590, 17,
      function() self.state:checklist_open_row(row_index) end)
    self.checklist_go_buttons[index] = self:_button(
      main, "aqg_cl_go_" .. tostring(index), "Go", 604, y, 64, 17,
      function() self.state:checklist_go(row_index) end)
  end
  self.step_label = self:_label(main, "aqg_stepline", "", 8, 436, 660)
  self:_button(main, "aqg_prev_step", "Prev Step", 8, 456, 68, 22,
    function() self.state:previous_step() end)
  self:_button(main, "aqg_next_step", "Next Step", 80, 456, 68, 22,
    function() self.state:next_step() end)
  self.ack_label = self:_label(main, "aqg_ack", "", 154, 461, 190)
  self.completion_label = self:_label(main, "aqg_completion", "Classic completion: unknown",
    344, 461, 230)
  self:_button(main, "aqg_refresh_completion", "Refresh completion", 576, 456, 104, 22,
    function() self.state:refresh_completion() end)

  self:_button(main, "aqg_rec_start", "Start", 8, 482, 52, 20,
    function() self.state:start_recording() end)
  self:_button(main, "aqg_rec_stop", "Stop", 64, 482, 48, 20,
    function() self.state:stop_recording() end)
  self:_button(main, "aqg_snap_player", "Snapshot player", 116, 482, 100, 20,
    function() self.state:snapshot("player") end)
  self:_button(main, "aqg_snap_target", "Snapshot target", 220, 482, 100, 20,
    function() self.state:snapshot("target") end)
  self:_button(main, "aqg_save", "Save/Export", 324, 482, 76, 20,
    function() self.state:save_observations() end)
  self.rec_status_label = self:_label(main, "aqg_recstatus", "Recording off.", 406, 485, 274)

  self:_button(main, "aqg_report_blocked", "Blocked", 8, 506, 62, 20,
    function() self.state:report("Blocked") end)
  self:_button(main, "aqg_report_npc", "Missing NPC", 74, 506, 80, 20,
    function() self.state:report("Missing NPC") end)
  self:_button(main, "aqg_report_doodad", "Missing doodad", 158, 506, 94, 20,
    function() self.state:report("Missing doodad") end)
  self:_button(main, "aqg_report_outcome", "Wrong outcome", 256, 506, 92, 20,
    function() self.state:report("Wrong outcome") end)
  self.note_edit = self:_edit(main, "aqg_note", 352, 506, 328, 20)

  self:_button(main, "aqg_nav_use", "Use 1.2 candidate", 8, 532, 110, 20,
    function() self.state:use_candidate() end)
  self:_button(main, "aqg_nav_capture_player", "Capture player", 122, 532, 94, 20,
    function() self.state:capture_waypoint("player") end)
  self:_button(main, "aqg_nav_capture_target", "Capture target", 220, 532, 94, 20,
    function() self.state:capture_waypoint("target") end)
  self:_button(main, "aqg_nav_clear", "Clear nav", 318, 532, 64, 20,
    function() self.state:clear_navigation() end)
  self.world_edit = self:_edit(main, "aqg_world", 386, 532, 90, 20)
  self:_label(main, "aqg_world_hint", "Classic world label", 480, 535, 190)

  self.nav_status_label = self:_label(main, "aqg_navstatus", "Navigation off.", 8, 556, 660)
  self.banner = self:_label(main, "aqg_banner", "", 8, 574, 660)

  self.server_edit = self:_edit(main, "aqg_server", 56, 590, 90, 18)
  self:_label(main, "aqg_server_label", "Server", 8, 592, 46)
  self.build_edit = self:_edit(main, "aqg_build", 188, 590, 70, 18)
  self:_label(main, "aqg_build_label", "Build", 150, 592, 36)
  self.context_edit = self:_edit(main, "aqg_context", 314, 590, 170, 18)
  self:_label(main, "aqg_context_label", "Context", 264, 592, 48)
  self.note_hint = self:_label(main, "aqg_hint",
    "Note is used by report buttons. No automatic game actions.", 500, 592, 172)

  self:_bind(self.id_edit, "OnEnterPressed", function() self.state:open_from_edit() end)
  self:_bind(self.filter_edit, "OnEnterPressed", function() self.state:find() end)
  for _, edit in ipairs({self.world_edit, self.server_edit, self.build_edit,
                         self.context_edit}) do
    self:_bind(edit, "OnTextChanged", function() self.state:on_form_changed() end)
  end

  U.safe_method(main, "Show", true)

  local toggle_ok, toggle, toggle_error = U.safe_method(
    api.Interface, "CreateEmptyWindow", "aqg_toggle")
  if toggle_ok and toggle then
    self.toggle = toggle
    U.safe_method(toggle, "SetExtent", 74, 24)
    U.safe_method(toggle, "AddAnchor", "TOPRIGHT", "UIParent", "TOPRIGHT", -18, 118)
    self:_button(toggle, "aqg_toggle_button", "Quest Guide",
      0, 0, 74, 24, function() self:show_main() end)
    U.safe_method(toggle, "Show", true)
  else
    self.status = "Show Guide button unavailable: " .. tostring(toggle_error)
  end
  self:_register_event_sink()
  if not self.handlers_ok then
    self.status = "Some click handlers are unavailable; use the Addon Manager to reload."
  end
end

function UI:set_status(text)
  self.status = U.truncate(tostring(text or ""), 220)
  if self.status_label then U.safe_method(self.status_label, "SetText", self.status) end
end

function UI:set_nav_status(text)
  self.nav_status = U.truncate(tostring(text or ""), 220)
  if self.nav_status_label then
    U.safe_method(self.nav_status_label, "SetText", self.nav_status)
  end
end

function UI:set_rec_status(text)
  self.rec_status = U.truncate(tostring(text or ""), 220)
  if self.rec_status_label then
    U.safe_method(self.rec_status_label, "SetText", self.rec_status)
  end
end

function UI:get_text(widget)
  if not widget then return "" end
  local ok, text = U.safe_method(widget, "GetText")
  if ok and type(text) == "string" then return text end
  return ""
end

function UI:get_form()
  local form = {
    server = U.truncate(U.trim(self:get_text(self.server_edit)), 48),
    context = U.truncate(U.trim(self:get_text(self.context_edit)), 64),
    build = U.truncate(U.trim(self:get_text(self.build_edit)), 40),
    world = U.truncate(U.trim(self:get_text(self.world_edit)), 48),
    note = U.truncate(U.trim(self:get_text(self.note_edit)), 200),
  }
  self.form = form
  return form
end

function UI:load_form(form)
  form = form or {}
  for _, pair in ipairs({
    {self.server_edit, "server"},
    {self.context_edit, "context"},
    {self.build_edit, "build"},
    {self.world_edit, "world"},
    {self.note_edit, "note"},
  }) do
    if pair[1] and form[pair[2]] then
      U.safe_method(pair[1], "SetText", U.truncate(tostring(form[pair[2]]), 200))
    end
  end
end

function UI:set_quest_id(text)
  if self.id_edit then U.safe_method(self.id_edit, "SetText", tostring(text or "")) end
end

function UI:_register_event_sink()
  local ok, sink = U.safe_method(
    self.api.Interface, "CreateEmptyWindow", "aqg_events", "UIParent")
  if not ok or not sink then
    self.events_ok = false
    return false
  end
  self.event_sink = sink
  U.safe_method(sink, "SetExtent", 1, 1)
  U.safe_method(sink, "AddAnchor", "TOPLEFT", "UIParent", "TOPLEFT", 0, 0)
  self:_bind(sink, "OnEvent", function(_widget, event, ...)
    if type(event) == "string" then self.state:on_game_event(event, ...) end
  end)
  for _, name in ipairs({"CHAT_MSG_QUEST", "ADDED_ITEM", "REMOVED_ITEM"}) do
    U.safe_method(sink, "RegisterEvent", name)
  end
  U.safe_method(sink, "Show", false)
  self.events_ok = true
  return true
end

function UI:set_view(view)
  local checklist = view == "checklist"
  if self.content then U.safe_method(self.content, "Show", not checklist) end
  if self.filter_status then
    U.safe_method(self.filter_status, "Show", not checklist)
  end
  for _, button in ipairs(self.filter_buttons) do
    if button then U.safe_method(button, "Show", checklist) end
  end
  for _, button in ipairs({self.go_giver_button, self.mark_done_button,
                           self.mark_undo_button}) do
    if button then U.safe_method(button, "Show", not checklist) end
  end
  if not checklist then
    for index = 1, CHECKLIST_ROWS do
      local row = self.checklist_row_buttons[index]
      local go = self.checklist_go_buttons[index]
      if row then U.safe_method(row, "Show", false) end
      if go then U.safe_method(go, "Show", false) end
    end
  end
end

function UI:refresh_page()
  local key = PAGE_BY_KEY[self.state.page] or "step"
  local lines = Pages.lines(self.state, key)
  local slice, page, pages = Pages.slice(lines, self.state.text_page, CONTENT_LINES)
  self.content_page = page
  self.content_pages = pages
  self.state.text_page = page
  if self.content then
    U.safe_method(self.content, "SetText", table.concat(slice, "\n"))
  end
  if self.page_label then
    U.safe_method(self.page_label, "SetText", "Page " .. tostring(page) ..
      "/" .. tostring(pages) .. " | " .. tostring(key))
  end
end

function UI:_meta_text()
  local catalog = self.state.catalog
  local meta = self.state:meta() or {}
  local db = tostring(catalog and catalog:database_hash() or "unknown")
  local text = "DB " .. db:sub(1, 12) .. " | " ..
    tostring(catalog and catalog:size() or 0) .. " quests"
  local ref = ((meta.inputs or {}).ref_db)
  if ref and ref.sha256 then
    text = text .. " | ref " .. tostring(ref.sha256):sub(1, 12)
  end
  return text .. " | candidate route 330 -> 6198 is unverified"
end

function UI:refresh_checklist()
  local checklist = self.state.checklist
  if not checklist then return end
  checklist:build()
  local visible = #checklist.rows
  if self.header_label then
    U.safe_method(self.header_label, "SetText",
      "Checklist | completed " .. tostring(checklist.done_on_page) .. " / " ..
      tostring(visible) .. " visible | page " .. tostring(checklist.page) ..
      "/" .. tostring(checklist.page_count) .. " | " ..
      tostring(checklist.total) .. " matching | done " ..
      tostring(checklist.done_in_scope) .. "/" .. tostring(checklist.scope_count))
  end
  if self.page_label then
    U.safe_method(self.page_label, "SetText", "Page " .. tostring(checklist.page) ..
      "/" .. tostring(checklist.page_count) .. " | checklist")
  end
  for index = 1, CHECKLIST_ROWS do
    local row = checklist.rows[index]
    local button = self.checklist_row_buttons[index]
    local go = self.checklist_go_buttons[index]
    if button then
      U.safe_method(button, "SetText", row and row.text or "")
      U.safe_method(button, "Show", row ~= nil)
    end
    if go then U.safe_method(go, "Show", row ~= nil) end
  end
  if self.step_label then
    U.safe_method(self.step_label, "SetText",
      "Checklist rows: click a row to open it, or Go to start the 1.2 candidate " ..
      "waypoint for its accept step. " .. checklist:filter_summary())
  end
  if self.ack_label then U.safe_method(self.ack_label, "SetText", "") end
  if self.completion_label then
    U.safe_method(self.completion_label, "SetText", "")
  end
  if self.filter_status then U.safe_method(self.filter_status, "Show", false) end
  if self.content then U.safe_method(self.content, "Show", false) end
end

function UI:refresh_quest()
  self:set_view("quest")
  local catalog = self.state.catalog
  local quest = self.state.quest
  local quest_title = "no quest"
  if quest then
    quest_title = "Quest #" .. tostring(quest.id) .. " " .. tostring(quest.name or "")
  end
  if self.header_label then
    local title = self.state:classic_title()
    U.safe_method(self.header_label, "SetText",
      "Expected 1.2 data | Observed Classic | " .. quest_title ..
      " | Classic title: " .. title)
  end
  self:refresh_page()
  local steps = quest and quest.steps or {}
  local step = self.state:current_step()
  if self.step_label then
    U.safe_method(self.step_label, "SetText",
      "Step " .. tostring(self.state.step_index) .. "/" .. tostring(#steps) ..
      (step and (" | component " .. tostring(step.component_id) ..
        " act " .. tostring(step.act_id)) or "") ..
      " | page " .. tostring(self.content_page) .. "/" .. tostring(self.content_pages))
  end
  if self.ack_label then
    U.safe_method(self.ack_label, "SetText",
      "Manual ack: " .. self.state:manual_summary())
  end
  if self.completion_label then
    U.safe_method(self.completion_label, "SetText",
      "Classic completion: " .. self.state:completion_text())
  end
  if self.filter_status then
    local count = self.state.list and #self.state.list or (catalog and catalog:size() or 0)
    U.safe_method(self.filter_status, "SetText",
      tostring(count) .. " quest(s) in browser" ..
      (self.state.filter ~= "" and (" | filter: " .. self.state.filter) or ""))
  end
end

function UI:refresh_all()
  if not self.main then
    if self.status_label then U.safe_method(self.status_label, "SetText", self.status) end
    return
  end
  if self.status_label then U.safe_method(self.status_label, "SetText", self.status) end
  if self.meta_label then
    U.safe_method(self.meta_label, "SetText", self:_meta_text())
  end
  if self.state.view == "checklist" then
    self:set_view("checklist")
    self:refresh_checklist()
  else
    self:refresh_quest()
  end
  if self.rec_status_label then
    U.safe_method(self.rec_status_label, "SetText", self.state.recorder:status_text())
  end
  if self.nav_status_label then
    U.safe_method(self.nav_status_label, "SetText", self.state.nav:status_text())
  end
  if self.banner then
    U.safe_method(self.banner, "SetText", self.state.nav:banner_text())
  end
  if not self.handlers_ok then
    U.safe_method(self.status_label, "SetText",
      "Some click handlers are unavailable; use the Addon Manager to reload.")
  end
end

function UI:show_main()
  if self.main then U.safe_method(self.main, "Show", true) end
end

function UI:hide_main()
  if self.main then U.safe_method(self.main, "Show", false) end
end

function UI:destroy()
  if self.main then
    U.safe_method(self.main, "Show", false)
    U.safe_method(self.api.Interface, "Free", self.main)
  end
  if self.toggle then
    U.safe_method(self.toggle, "Show", false)
    U.safe_method(self.api.Interface, "Free", self.toggle)
  end
  if self.event_sink then
    U.safe_method(self.event_sink, "Show", false)
    U.safe_method(self.api.Interface, "Free", self.event_sink)
  end
  self.main = nil
  self.toggle = nil
  self.content = nil
  self.status_label = nil
  self.nav_status_label = nil
  self.rec_status_label = nil
  self.event_sink = nil
end

return UI

