-- Runtime controller: selection, pages, completion, recorder and navigation
-- wiring. No game actions are performed.

local U = require("aaemu_quest_guide/util")
local Catalog = require("aaemu_quest_guide/catalog")
local Checklist = require("aaemu_quest_guide/checklist")
local Nav = require("aaemu_quest_guide/nav")
local Progress = require("aaemu_quest_guide/progress")
local Recorder = require("aaemu_quest_guide/recorder")
local UI = require("aaemu_quest_guide/ui")

local Core = {}
Core.__index = Core

local PAGE_KEYS = {step = true, req = true, rewards = true, sources = true,
                   branches = true, info = true}
local IDENTITY_CHECK_MS = 2000

function Core.new(api, helpers)
  local env = _G or getfenv(0)
  helpers = helpers or {
    w_ctrl = env.W_CTRL,
    x2util = env.X2Util,
    button_basic = env.BUTTON_BASIC,
  }
  local self = setmetatable({
    api = api,
    helpers = helpers,
    alive = true,
    ui_ms = 0,
    page = "step",
    view = "quest",
    text_page = 1,
    step_index = 1,
    quest = nil,
    manual = {},
    filter = "",
    list = nil,
    completion = {ok = false, error = "not queried"},
    classic = {},
    settings = {},
    identity = nil,
    player_key = nil,
    context_key = nil,
    last_identity_check = 0,
    last_settings_save = 0,
    last_completion_check = 0,
    status = "Ready.",
  }, Core)
  local ok_settings, settings = pcall(api.GetSettings, "aaemu_quest_guide")
  if ok_settings and type(settings) == "table" then self.settings = settings end
  self.catalog, self.catalog_error = Catalog.new()
  self.index_positions = {}
  if self.catalog then
    for position, entry in ipairs(self.catalog.index.quests or {}) do
      self.index_positions[entry.id] = position
    end
  end
  self.checklist = Checklist.new(api, self)
  self.progress = Progress.new(api, self)
  self.nav = Nav.new(api, self)
  self.recorder = Recorder.new(api, self)
  self.ui = UI.new(api, self)
  self:_register_events()
  self:_initial_identity()
  return self
end

function Core:now()
  return self.ui_ms
end

function Core:meta()
  return self.catalog and self.catalog.meta or {}
end

function Core:set_status(text)
  self.status = U.truncate(tostring(text or ""), 220)
  if self.ui then self.ui:set_status(self.status) end
end

function Core:set_nav_status(text)
  if self.ui then self.ui:set_nav_status(text) end
end

function Core:set_rec_status(text)
  if self.ui then self.ui:set_rec_status(text) end
end

function Core:get_form()
  if self.ui then return self.ui:get_form() end
  return {server = "", context = "", build = "", world = "", note = ""}
end

function Core:_current_context_key()
  local form = self:get_form() or {}
  return table.concat({
    U.trim(form.server or ""), U.trim(form.context or ""),
    U.trim(form.build or ""),
  }, "|")
end

function Core:_clear_progress(reason)
  self.quest = nil
  self.step_index = 1
  self.manual = {}
  self.completion = {ok = false, error = "not queried"}
  self.classic = {}
  self.text_page = 1
  self.page = "step"
  if self.checklist then self.checklist.page = 1 end
  if reason and self.nav then self.nav:clear(reason) end
end

function Core:_save_progress_for(identity, context_key)
  if not identity or not context_key or not self.quest then return end
  local db = self.catalog and self.catalog:database_hash() or "unknown"
  local key = context_key .. "|" .. identity.name .. "|" .. db
  self.settings.resume = self.settings.resume or {}
  self.settings.resume[key] = {
    quest_id = self.quest.id,
    step_index = self.step_index,
    manual = U.list_from_set(self.manual, 200),
    page = self.page,
  }
end

function Core:_sync_identity_now()
  local identity, identity_error = self.recorder:identity()
  local key = identity and (type(identity.id) .. ":" .. tostring(identity.id) .. "|" .. identity.name) or nil
  if key == self.player_key then
    self.identity = identity
    return identity, false
  end
  if self.player_key == nil and key ~= nil then
    -- Initial identification is not a character switch; do not stop or clear
    -- anything just because the first UPDATE has not run yet.
    self.player_key = key
    self.identity = identity
    return identity, false
  end
  if self.player_key ~= nil then
    self:_save_progress_for(self.identity, self.context_key)
    self.recorder:stop("identity change")
    self:_clear_progress("Character changed: navigation and manual progress cleared.")
    self.player_key = key
    self.identity = identity
    if self.progress then self.progress:reload() end
    if identity then
      self:try_resume()
    else
      self:set_status("Character identity unknown (" ..
        tostring(identity_error or "missing") .. "); progress isolated.")
    end
    return identity, true
  end
  self.player_key = key
  self.identity = identity
  return identity, false
end

function Core:_sync_context_now()
  local current = self:_current_context_key()
  if self.context_key == nil then
    self.context_key = current
    return false
  end
  if current == self.context_key then return false end
  self:_save_progress_for(self.identity, self.context_key)
  self.recorder:stop("context change")
  self:_clear_progress("Context changed: navigation and manual progress cleared.")
  self.context_key = current
  if self.progress then self.progress:reload() end
  if self.identity then self:try_resume() end
  self:set_status("Declared server/context changed; old progress was isolated.")
  return true
end

-- UI actions call this before mutating or persisting state. It catches a
-- character or context switch even when no UPDATE tick has arrived yet.
function Core:_ensure_current(action)
  local _, identity_changed = self:_sync_identity_now()
  local context_changed = self:_sync_context_now()
  if identity_changed or context_changed then
    self:set_status("Character/context changed; " .. tostring(action or "action") ..
      " was canceled after isolating the previous progress.")
    if self.ui then self.ui:refresh_all() end
    return false
  end
  return true
end

function Core:current_step()
  if not self.quest or type(self.quest.steps) ~= "table" then return nil end
  return self.quest.steps[self.step_index]
end

function Core:step_key(step)
  step = step or self:current_step()
  if not step then return "" end
  return tostring(step.component_id) .. ":" .. tostring(step.act_id)
end

function Core:is_manually_acked()
  local key = self:step_key()
  return key ~= "" and self.manual[key] == true
end

function Core:manual_summary()
  if self:is_manually_acked() then
    return "yes (current step; not completion)"
  end
  for _ in pairs(self.manual or {}) do
    return "yes (previous step; not completion)"
  end
  return "no"
end

function Core:selected_info()
  local step = self:current_step() or {}
  return {
    quest_id = self.quest and self.quest.id or nil,
    step_index = self.step_index,
    component_id = step.component_id,
    act_id = step.act_id,
    manually_acknowledged = self:is_manually_acked(),
  }
end

function Core:classic_title()
  local title = self.classic and self.classic.title
  if type(title) == "string" and title ~= "" then return title end
  if self.quest then return "Quest#" .. tostring(self.quest.id) end
  return "Quest#?"
end

function Core:classic_body()
  local body = self.classic and self.classic.body
  if type(body) == "string" and body ~= "" then return body end
  return nil
end

function Core:completion_text()
  local value = self.completion or {}
  if not self.quest then return "not selected" end
  if self.progress and self.progress:is_observed(self.quest.id) then
    return "completed (turn-in/reward observed)"
  end
  if not value.ok then
    if self.progress and self.progress:is_manual(self.quest.id) then
      return "manual mark [m] (not client completion)"
    end
    return "unavailable (" .. tostring(value.error or "error") .. ")"
  end
  if value.nil_value then return "unavailable (nil)" end
  if value.value == true then return "completed (observed)" end
  if self.progress and self.progress:is_manual(self.quest.id) then
    return "manual mark [m] (not client completion)"
  end
  if value.value == false then return "not completed (observed)" end
  return "unavailable (unexpected value)"
end

function Core:_safe_quest_text(method, quest_id)
  local ok, value, error_message = U.safe_method(self.api.Quest, method, quest_id)
  if ok and type(value) == "string" and U.trim(value) ~= "" then
    return value
  end
  return nil, (not ok) and tostring(error_message) or nil
end

function Core:refresh_classic()
  if not self.quest then
    self.classic = {}
    self.completion = {ok = false, error = "not selected"}
    return
  end
  local qid = self.quest.id
  local title, title_error = self:_safe_quest_text("GetQuestContextMainTitle", qid)
  if not title then
    title = self:_safe_quest_text("GetActiveQuestTitle", qid)
  end
  local body, body_error = self:_safe_quest_text("GetQuestContextBody", qid)
  local ok, completed, completed_error = U.safe_method(
    self.api.Quest, "IsCompleted", qid)
  if ok then
    if completed == nil then
      self.completion = {ok = true, nil_value = true}
    else
      self.completion = {ok = true, value = completed}
    end
  else
    self.completion = {ok = false, error = tostring(completed_error)}
  end
  self.classic = {
    title = title or ("Quest#" .. tostring(qid)),
    body = body,
    title_error = title_error,
    body_error = body_error,
  }
end

function Core:refresh_completion()
  self.progress:reload()
  self.progress:refresh_live()
  self:refresh_classic()
  if self.ui then self.ui:refresh_all() end
  self:set_status("Classic completion refreshed from the client API (observed only).")
end

function Core:open_quest(id, restore)
  self:_sync_identity_now()
  self:_sync_context_now()
  id = tonumber(id)
  if not id or id ~= math.floor(id) or id <= 0 then
    self:set_status("Quest ID must be a positive integer.")
    return false
  end
  if not self.catalog then
    self:set_status("Catalog unavailable: " .. tostring(self.catalog_error))
    return false
  end
  local quest, error_message = self.catalog:quest(id)
  if not quest then
    self:set_status(error_message)
    return false
  end
  self.quest = quest
  self.view = "quest"
  self.step_index = 1
  self.manual = {}
  self.text_page = 1
  self.page = "step"
  if restore then
    self.step_index = U.clamp(tonumber(restore.step_index) or 1, 1,
      math.max(1, #(quest.steps or {})))
    self.manual = U.set_from_list(restore.manual)
    self.page = PAGE_KEYS[restore.page] and restore.page or "step"
  end
  self.nav:clear("Quest changed: navigation cleared.")
  self:refresh_classic()
  if self.ui then self.ui:refresh_all() end
  self:set_status("Opened quest #" .. tostring(id) ..
    (restore and " (resumed)" or "") .. ".")
  if not restore then self:save_resume() end
  return true
end

function Core:open_from_edit()
  if not self.ui or not self.ui.id_edit then
    self:set_status("Numeric quest ID entry is unavailable (W_CTRL.CreateEdit missing).")
    return
  end
  local text = U.trim(self.ui:get_text(self.ui.id_edit))
  if text == "" then
    self:set_status("Enter a numeric quest ID first.")
    return
  end
  self:open_quest(text)
end

function Core:_browser_ids()
  return self.list or (self.catalog and self.catalog.index.quests or {})
end

function Core:_browser_position()
  if not self.quest then return nil end
  if self.list then
    for position, id in ipairs(self.list) do
      if tonumber(id) == self.quest.id then return position end
    end
    return nil
  end
  return self.index_positions[self.quest.id]
end

function Core:_move_quest(delta)
  if not self:_ensure_current("quest browsing") then return end
  if not self.catalog then return end
  local ids = self:_browser_ids()
  if #ids == 0 then self:set_status("No quests in the browser.") return end
  local position = self:_browser_position()
  if not position then position = delta > 0 and 0 or (#ids + 1) end
  local next_position = U.clamp(position + delta, 1, #ids)
  if next_position == position then
    self:set_status("Reached the end of the browser list.")
    return
  end
  local entry = ids[next_position]
  local id = type(entry) == "table" and entry.id or entry
  self:open_quest(id)
end

function Core:previous_quest() self:_move_quest(-1) end
function Core:next_quest() self:_move_quest(1) end

function Core:find()
  if not self:_ensure_current("search") then return end
  if not self.catalog then self:set_status("Catalog unavailable.") return end
  local text = self.ui and self.ui.filter_edit and U.trim(self.ui:get_text(self.ui.filter_edit)) or ""
  self.filter = text
  if self.view == "checklist" then
    self.checklist.page = 1
    if self.ui then self.ui:refresh_all() end
    self:set_status("Checklist text filter: " ..
      (text == "" and "(cleared)" or U.truncate(text, 80)) .. ".")
    return
  end
  if text == "" then
    self.list = nil
    self:set_status("Filter cleared; all " .. tostring(self.catalog:size()) .. " quests are in the browser.")
    if self.ui then self.ui:refresh_all() end
    return
  end
  local ids, count = self.catalog:search(text, 200)
  if not ids or #ids == 0 then
    self:set_status("No catalog match for filter: " .. text)
    return
  end
  self.list = ids
  self:set_status("Found " .. tostring(#ids) ..
    (count and count >= 200 and " (bounded to 200)" or "") ..
    " quest(s); opening the first match.")
  self:open_quest(ids[1])
end

function Core:open_route()
  if not self:_ensure_current("route") then return end
  if not self.catalog then self:set_status("Catalog unavailable.") return end
  local ids = self.catalog:route_ids()
  self.list = ids
  self.filter = "route"
  if self.ui and self.ui.filter_edit then
    U.safe_method(self.ui.filter_edit, "SetText", "route")
  end
  self:set_status("Candidate starter route 330 -> 6198 is unverified, not optimal. " ..
    "Quest 330 must be taken before 6198.")
  if #ids > 0 then
    self:open_quest(ids[1])
    self:set_page("info")
  end
end

function Core:set_page(page)
  if not PAGE_KEYS[page] then return end
  self.page = page
  self.view = "quest"
  self.text_page = 1
  if self.ui then self.ui:refresh_all() end
end

function Core:previous_text_page()
  if self.view == "checklist" then
    if self.checklist:next_page(-1) and self.ui then self.ui:refresh_all() end
    return
  end
  self.text_page = math.max(1, self.text_page - 1)
  if self.ui then self.ui:refresh_all() end
end

function Core:next_text_page()
  if self.view == "checklist" then
    if self.checklist:next_page(1) and self.ui then self.ui:refresh_all() end
    return
  end
  self.text_page = self.text_page + 1
  if self.ui then self.ui:refresh_all() end
end

function Core:_advance_step(delta)
  if not self:_ensure_current("step advance") then return end
  if not self.quest then
    self:set_status("Open a quest first.")
    return
  end
  local count = #(self.quest.steps or {})
  if count == 0 then
    self:set_status("This quest has no normal steps.")
    return
  end
  local old = self.step_index
  if delta > 0 then
    local key = self:step_key()
    if key ~= "" then self.manual[key] = true end
  end
  self.step_index = U.clamp(self.step_index + delta, 1, count)
  self.text_page = 1
  self.nav:clear("Step changed: navigation cleared.")
  if self.step_index == old and delta > 0 then
    self:set_status("Last step acknowledged manually; this is not completion.")
  else
    self:set_status("Step " .. tostring(self.step_index) .. "/" .. tostring(count) ..
      (delta > 0 and " (previous step acknowledged manually, not completion)" or "."))
  end
  if self.ui then self.ui:refresh_all() end
  self:save_resume()
end

function Core:previous_step() self:_advance_step(-1) end
function Core:next_step() self:_advance_step(1) end

function Core:on_form_changed()
  self:_sync_context_now()
  self.nav:clear("World/context label changed: navigation cleared.")
  if self.ui then self.ui:refresh_all() end
end

function Core:open_checklist()
  if not self:_ensure_current("checklist") then return end
  if not self.catalog then
    self:set_status("Catalog unavailable: " .. tostring(self.catalog_error))
    return false
  end
  self.view = "checklist"
  self.progress:reload()
  self.progress:refresh_live()
  self.checklist.page = 1
  if self.ui then self.ui:refresh_all() end
  self:set_status("Checklist refreshed from the client API. " ..
    self.checklist:filter_summary() .. ".")
  return true
end

function Core:set_checklist_filter(name)
  if not self.checklist:set_filter(name) then return end
  self.view = "checklist"
  if self.ui then self.ui:refresh_all() end
  self:set_status("Checklist filter: " .. self.checklist:filter_summary() .. ".")
end

function Core:toggle_checklist_placeholders()
  self.checklist.hide_placeholders = not self.checklist.hide_placeholders
  self.checklist.page = 1
  self.view = "checklist"
  if self.ui then self.ui:refresh_all() end
  self:set_status("Checklist filter: " .. self.checklist:filter_summary() .. ".")
end

function Core:toggle_checklist_ref()
  if not self.checklist:ref_available() then
    self:set_status("This catalog was built without --ref-db; " ..
      "the not-in-3.0 filter is unavailable.")
    return
  end
  self.checklist.hide_not_ref = not self.checklist.hide_not_ref
  self.checklist.page = 1
  self.view = "checklist"
  if self.ui then self.ui:refresh_all() end
  self:set_status("Checklist filter: " .. self.checklist:filter_summary() .. ".")
end

function Core:checklist_open_row(index)
  local row = self.checklist.rows[tonumber(index) or 0]
  if not row then return end
  self:open_quest(row.id)
end

function Core:checklist_go(index)
  local row = self.checklist.rows[tonumber(index) or 0]
  if not row then return end
  self:go_to_giver(row.id)
end

function Core:go_to_giver(quest_id)
  if not self:_ensure_current("go to giver") then return false end
  if not self.catalog then
    self:set_status("Catalog unavailable: " .. tostring(self.catalog_error))
    return false
  end
  local id = tonumber(quest_id) or (self.quest and self.quest.id)
  if not id then
    self:set_status("Open or select a quest first.")
    return false
  end
  local quest, error_message = self.catalog:quest(id)
  if not quest then
    self:set_status(error_message)
    return false
  end
  local start = quest.start
  if type(start) ~= "table" or not start.step_index then
    self:set_status("Quest #" .. tostring(id) ..
      " has no accept step in the catalog; no waypoint.")
    return false
  end
  local step = (quest.steps or {})[start.step_index]
  if not step or not start.has_position
      or type(step.locations) ~= "table" or #step.locations == 0 then
    local kind = tostring(start.kind or (step and step.target and step.target.kind) or "?")
    local status = tostring(start.status or (step and step.status) or "?")
    self:set_status("Quest #" .. tostring(id) .. " starts from " .. kind ..
      " with no 1.2 world position (status " .. status ..
      "); navigation not started.")
    return false, "no position"
  end
  self:open_quest(id)
  self.step_index = U.clamp(tonumber(start.step_index) or 1, 1,
    math.max(1, #(quest.steps or {})))
  self.text_page = 1
  self.page = "step"
  self:save_resume()
  if self.ui then self.ui:refresh_all() end
  local ok, message = self.nav:activate_candidate()
  self.nav:update(self:now())
  if self.ui then self.ui:refresh_all() end
  self:set_status(message)
  return ok, message
end

function Core:mark_done()
  if not self:_ensure_current("manual mark") then return false end
  local id = self.quest and self.quest.id
  if not id then
    self:set_status("Open a quest first to mark it done.")
    return false
  end
  local ok, message = self.progress:set_manual(id, true)
  if self.ui then self.ui:refresh_all() end
  self:set_status(message)
  return ok
end

function Core:unmark_done()
  if not self:_ensure_current("manual mark") then return false end
  local id = self.quest and self.quest.id
  if not id then
    self:set_status("Open a quest first to remove its manual mark.")
    return false
  end
  local ok, message = self.progress:set_manual(id, false)
  if self.ui then self.ui:refresh_all() end
  self:set_status(message)
  return ok
end

function Core:on_game_event(name, ...)
  if not self.alive then return end
  self:_sync_identity_now()
  self:_sync_context_now()
  local changed, message = self.progress:handle_event(name, ...)
  if changed then
    if self.ui then self.ui:refresh_all() end
    self:set_status(message)
  end
end

function Core:use_candidate()
  if not self:_ensure_current("candidate navigation") then return end
  local ok, message = self.nav:activate_candidate()
  self.nav:update(self:now())
  if self.ui then self.ui:refresh_all() end
  self:set_status(message)
  if not ok and self.ui then self.ui:set_status(message) end
end

function Core:capture_waypoint(unit)
  if not self:_ensure_current("waypoint capture") then return end
  local ok, message = self.nav:capture(unit)
  self.nav:update(self:now())
  if self.ui then self.ui:refresh_all() end
  self:set_status(message)
  if not ok and self.ui then self.ui:set_status(message) end
end

function Core:clear_navigation()
  self.nav:clear("Navigation cleared.")
  if self.ui then self.ui:refresh_all() end
  self:set_status(self.nav:status_text())
end

function Core:start_recording()
  if not self:_ensure_current("start recording") then return end
  local ok = self.recorder:start()
  if ok then
    self.settings.form = self:get_form()
    self:save_settings("recording start")
  end
  if self.ui then self.ui:refresh_all() end
  self:set_status(self.recorder:status_text())
end

function Core:stop_recording()
  self.recorder:stop("user stop")
  self.settings.form = self:get_form()
  self:save_resume()
  if self.ui then self.ui:refresh_all() end
  self:set_status(self.recorder:status_text())
end

function Core:snapshot(unit)
  if not self:_ensure_current("snapshot") then return end
  self.recorder:snapshot(unit)
  if self.ui then self.ui:refresh_all() end
  self:set_status(self.recorder:status_text())
end

function Core:report(reason)
  if not self:_ensure_current("report") then return end
  local form = self:get_form()
  self.recorder:annotate(reason, form.note)
  if self.ui then self.ui:refresh_all() end
  self:set_status(self.recorder:status_text())
end

function Core:save_observations()
  self:_sync_identity_now()
  local ok = self.recorder:retry_flush()
  self.settings.form = self:get_form()
  self:save_resume()
  if self.ui then self.ui:refresh_all() end
  if ok then
    local path = self.recorder.last_path or "(no unsaved records)"
    self:set_status("Save/export: " .. tostring(path))
  else
    self:set_status(self.recorder:status_text())
  end
end

function Core:save_settings(reason)
  local ok, result, error_message = U.safe_func(self.api, "SaveSettings")
  local success = ok and (result == nil or result == true)
  self.last_settings_save = self:now()
  if not success then
    return false, tostring(error_message or result or "SaveSettings failed")
  end
  return true, nil
end

function Core:_resume_key(identity, context_key)
  identity = identity or self.identity or self.recorder:identity()
  if not identity then return nil end
  context_key = context_key or self.context_key or self:_current_context_key()
  local db = self.catalog and self.catalog:database_hash() or "unknown"
  return table.concat({context_key, identity.name, db}, "|")
end

function Core:storage_key()
  return self:_resume_key()
end

function Core:save_resume()
  local _, identity_changed = self:_sync_identity_now()
  local context_changed = self:_sync_context_now()
  if identity_changed or context_changed or not self.identity then return false end
  local key = self:_resume_key(self.identity, self.context_key)
  if not key then return false end
  local settings = self.settings
  settings.resume = settings.resume or {}
  local manual = U.list_from_set(self.manual, 200)
  settings.resume[key] = {
    quest_id = self.quest and self.quest.id or nil,
    step_index = self.step_index,
    manual = manual,
    page = self.page,
  }
  settings.form = self:get_form()
  local ok = self:save_settings("resume")
  return ok
end

function Core:try_resume()
  local identity = self.identity or self.recorder:identity()
  if not identity then
    self:set_status("Resume unavailable: player identity is unknown.")
    return false
  end
  local key = self:_resume_key(identity, self.context_key)
  local saved = key and (self.settings.resume or {})[key]
  if type(saved) ~= "table" or not saved.quest_id then
    return false
  end
  if self:open_quest(saved.quest_id, saved) then
    self:set_status("Resumed quest #" .. tostring(saved.quest_id) ..
      " for " .. identity.name .. " (manual acknowledgements restored; completion is re-read).")
    return true
  end
  return false
end

function Core:_initial_identity()
  self.context_key = self:_current_context_key()
  local identity = self:_sync_identity_now()
  self.progress:reload()
  if not identity then
    self:set_status("Player identity unavailable; recording and resume are disabled until it is known.")
  elseif not self:try_resume() then
    self:set_status("Player identified; no saved progress for this declared context.")
  end
end

function Core:notify_identity_changed(identity)
  self:_save_progress_for(self.identity, self.context_key)
  self:_clear_progress("Character changed: navigation and manual progress cleared.")
  if identity then
    self.player_key = type(identity.id) .. ":" .. tostring(identity.id) .. "|" .. identity.name
    self.identity = identity
    if self.progress then self.progress:reload() end
    if not self:try_resume() then
      self:set_status("Character changed to " .. identity.name ..
        "; no saved progress for this character/context.")
    end
  else
    self.player_key = nil
    self.identity = nil
    if self.progress then self.progress:reload() end
    self:set_status("Character identity became unknown; recording/progress isolated.")
  end
  if self.ui then self.ui:refresh_all() end
end

function Core:notify_context_changed()
  self:_save_progress_for(self.identity, self.context_key)
  self:_clear_progress("Declared context changed: navigation and manual progress cleared.")
  self.context_key = self:_current_context_key()
  if self.progress then self.progress:reload() end
  if self.identity then self:try_resume() end
  if self.ui then self.ui:refresh_all() end
end

function Core:_sync_identity(now)
  if now < self.last_identity_check + IDENTITY_CHECK_MS then return end
  self.last_identity_check = now
  self:_sync_identity_now()
  self:_sync_context_now()
end

function Core:on_update(dt)
  if not self.alive then return end
  local ok, ms = U.safe_method(self.api.Time, "GetUiMsec")
  if ok and U.finite(ms) then
    self.ui_ms = ms
  else
    self.ui_ms = self.ui_ms + (tonumber(dt) or 0)
  end
  self:_sync_identity(self.ui_ms)
  if self.quest and self.ui_ms >= self.last_completion_check + 5000 then
    self.last_completion_check = self.ui_ms
    local before = self:completion_text()
    self:refresh_classic()
    if self:completion_text() ~= before and self.ui then
      self.ui:refresh_all()
    end
  end
  self.nav:update(self.ui_ms)
  self.recorder:tick(self.ui_ms)
end

function Core:_register_events()
  local state = self
  U.safe_func(self.api, "On", "UPDATE", function(dt)
    if state.alive then state:on_update(dt) end
  end)
end

function Core:show_main()
  if self.ui then self.ui:show_main() end
end

function Core:destroy()
  if not self.alive then return end
  self.recorder:destroy()
  self.nav:destroy()
  if self.ui then self.ui:destroy() end
  self.alive = false
end

return Core
