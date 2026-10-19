-- Bounded page text builders. Nothing here runs every frame: pages are rebuilt
-- only when the selected quest/step/page changes.

local U = require("aaemu_quest_guide/util")

local Pages = {}

local PER_PAGE = 16

local function number(value, fallback)
  if U.finite(value) then return tostring(value) end
  return fallback or "?"
end

local function add(lines, text)
  lines[#lines + 1] = U.truncate(text, 110)
end

local function step_target(step)
  local target = step and step.target or {}
  local kind = tostring(target.kind or "none")
  local count = number(target.count, "1")
  if kind == "none" then return "none (no world target)", count end
  local ids = U.join_ids(target.ids or {}, 12)
  return kind .. " template ids [" .. ids .. "]", count
end

local ACTION_VERBS = {
  QuestActConAcceptNpc = "Accept at NPC",
  QuestActConAcceptNpcKill = "Accept/kill at NPC",
  QuestActConAcceptNpcEmotion = "Express emotion at NPC",
  QuestActConReportNpc = "Report to NPC",
  QuestActConReportDoodad = "Report at doodad",
  QuestActConReportJournal = "Report from the quest journal",
  QuestActConAcceptDoodad = "Use/accept at doodad",
  QuestActObjTalk = "Talk to NPC",
  QuestActObjTalkNpcGroup = "Talk to any NPC in group",
  QuestActObjMonsterHunt = "Kill NPC",
  QuestActObjMonsterGroupHunt = "Kill any NPC in group",
  QuestActObjZoneKill = "Kill in zone",
  QuestActObjItemGather = "Gather/collect item",
  QuestActObjItemGroupGather = "Gather/collect any item in group",
  QuestActObjItemUse = "Use item",
  QuestActObjItemGroupUse = "Use any item in group",
  QuestActConAcceptItem = "Obtain/accept item",
  QuestActConAcceptItemGain = "Obtain item",
  QuestActConAcceptItemEquip = "Equip item",
  QuestActEtcItemObtain = "Obtain item",
  QuestActObjInteraction = "Interact with doodad",
  QuestActObjCraft = "Craft recipe",
  QuestActObjLevel = "Reach level",
  QuestActObjAbilityLevel = "Reach ability level",
  QuestActObjMateLevel = "Reach mate level",
  QuestActConAcceptLevelUp = "Reach level",
  QuestActObjSphere = "Reach/enter quest-sign area",
  QuestActConAcceptSphere = "Reach/accept at quest-sign area",
  QuestActCheckSphere = "Check quest-sign area",
  QuestActObjCompleteQuest = "Complete quest",
  QuestActObjCinema = "Watch cinema",
  QuestActObjAggro = "Build aggro ranks on target",
  QuestActObjExpressFire = "Use expression at NPC",
  QuestActObjZoneMonsterHunt = "Kill in zone",
  QuestActObjZoneNpcTalk = "Talk to NPC in zone",
  QuestActObjZoneQuestComplete = "Complete quests in zone",
  QuestActConAcceptComponent = "Continue from component",
  QuestActConAutoComplete = "Advance automatically (no player action)",
  QuestActConFail = "Fail branch (optional)",
  QuestActCheckTimer = "Wait for timer/condition",
  QuestActCheckDistance = "Check distance",
  QuestActCheckGuard = "Check guard condition",
}
local SUPPLY_VERB = "Expected supply/reward act"

local function action_verb(step)
  local act = tostring(step and step.act_type or "")
  local verb = ACTION_VERBS[act]
  if verb then return verb end
  if act:match("^QuestActSupply") then return SUPPLY_VERB end
  return "Manual/unsupported action (unknown act type; inspect raw act type)"
end

local function location_worlds(step, limit)
  local worlds = {}
  local seen = {}
  for _, location in ipairs((step and step.locations) or {}) do
    local world = tostring(location.world or "?")
    if not seen[world] then
      seen[world] = true
      worlds[#worlds + 1] = world
      if #worlds >= (limit or 3) then break end
    end
  end
  return table.concat(worlds, ", ")
end

function Pages.step(state)
  local lines = {}
  local quest = state.quest
  local step = state:current_step()
  if not quest then
    add(lines, "No quest selected. Enter a numeric quest ID and press Open.")
    return lines
  end
  if not step then
    add(lines, "Quest #" .. tostring(quest.id) .. " has no normal step at this position.")
    return lines
  end
  add(lines, "Classic UI title (runtime): " .. state:classic_title())
  local body = state:classic_body()
  add(lines, "Classic UI body (runtime): " ..
    (body and U.truncate(body, 180) or "(unavailable; fallback Quest#" .. tostring(quest.id) .. ")"))
  add(lines, "Internal compact name: " .. tostring(quest.name or "(none)") ..
    " (Korean 1.2 data, not Classic localization)")
  add(lines, "Component " .. tostring(step.component_id) ..
    " (" .. tostring(step.component_kind or "?") .. ")" ..
    "  |  Act " .. tostring(step.act_id) .. " " .. tostring(step.act_type or "?"))
  local target, count = step_target(step)
  add(lines, "Concrete action: " .. action_verb(step) .. " -> " .. target ..
    "  x" .. tostring(count))
  if step.target and step.target.emotion_id then
    add(lines, "Emotion requirement: " .. tostring(step.target.emotion_id))
  end
  if step.act_type == "QuestActObjCraft" and step.target then
    add(lines, "Craft recipe: " .. tostring(step.target.craft_id or "?"))
  end
  add(lines, "Catalog status: " .. tostring(step.status or "?"))
  if step.locations and #step.locations > 0 then
    add(lines, "Candidate 1.2 worlds: " .. location_worlds(step) ..
      " (" .. tostring(#step.locations) .. " location(s), unverified)")
    local first = step.locations[1] or {}
    if first.schedule then
      add(lines, "Conditional spawn: schedule id(s) " ..
        U.join_ids(first.schedule.schedule_ids or {}, 6))
    end
  else
    add(lines, "No candidate waypoint for this step (status " ..
      tostring(step.status or "?") .. ").")
  end
  if step.spheres and #step.spheres > 0 then
    add(lines, "Quest-sign sphere candidates: " .. tostring(#step.spheres) ..
      " (also unverified 1.2 placements)")
  end
  add(lines, "Manual acknowledgement: " .. state:manual_summary())
  add(lines, "Classic completion is read separately from client IsCompleted.")
  if quest.blockers and #quest.blockers > 0 then
    add(lines, "Quest blockers: " .. table.concat(quest.blockers, "; "))
  end
  add(lines, "Observed Classic actions are not executed by this addon.")
  return lines
end

function Pages.requirements(state)
  local lines = {}
  local quest = state.quest
  if not quest then
    add(lines, "No quest selected.")
    return lines
  end
  local acceptance = quest.acceptance or {}
  add(lines, "Acceptance: any one Start component is an alternative.")
  local starts = {}
  for _, id in ipairs(acceptance.start_components or {}) do
    starts[#starts + 1] = tostring(id)
  end
  add(lines, "Start components: " .. (table.concat(starts, ", ") ~= ""
    and table.concat(starts, ", ") or "none (auxiliary context)"))
  if #(quest.prereq_quests or {}) > 0 then
    add(lines, "Prerequisite quest ids referenced by Start groups: " ..
      U.join_ids(quest.prereq_quests, 12) ..
      " (aggregated; not mandatory all-of)")
  end
  add(lines, "Requirements are catalog data; they are not evaluated as pass/fail.")
  for _, group in ipairs(quest.requirement_groups or {}) do
    local head = "Component " .. tostring(group.component_id) .. " " ..
      tostring(group.component_kind or "?") .. " (" .. tostring(group.mode or "?") .. ")"
    if group.scope == "accept" then head = head .. " [accept alternative]" end
    if group.optional_branch then head = head .. " [optional branch]" end
    add(lines, head)
    if group.unrestricted or #(group.requirements or {}) == 0 then
      add(lines, "  unrestricted (no requirements in this component)")
    end
    for _, requirement in ipairs(group.requirements or {}) do
      add(lines, "  " .. tostring(requirement.kind or "?") ..
        " (kind " .. tostring(requirement.kind_id) .. ")" ..
        " value1=" .. number(requirement.value1, "nil") ..
        " value2=" .. number(requirement.value2, "nil") ..
        "  [not evaluated]")
    end
  end
  if #(quest.requirement_groups or {}) == 0 then
    add(lines, "No component requirement rows are present.")
  end
  return lines
end

function Pages.rewards(state)
  local lines = {}
  local quest = state.quest
  if not quest then
    add(lines, "No quest selected.")
    return lines
  end
  local rewards = quest.rewards or {}
  local expected = rewards.expected or {}
  local supply = rewards.level_supply or {}
  add(lines, "Expected 1.2 reward data (not observed Classic values):")
  add(lines, "  supply act EXP: " .. number(expected.act_exp, "0") ..
    " | supply act copper: " .. number(expected.act_copper, "0"))
  add(lines, "  level-based quest_supplies: EXP " .. number(supply.exp, "none") ..
    " | copper " .. number(supply.copper, "none") ..
    (supply.LEVEL and (" | level " .. tostring(supply.LEVEL)) or ""))
  if #(expected.items or {}) > 0 then
    for _, item in ipairs(expected.items) do
      add(lines, "  item " .. tostring(item.item_id or "?") ..
        " x" .. number(item.count, "1") ..
        (item.grade_id and (" grade " .. tostring(item.grade_id)) or ""))
    end
  end
  if #(expected.selective_items or {}) > 0 then
    for _, item in ipairs(expected.selective_items) do
      add(lines, "  selective item " .. tostring(item.item_id or "?") ..
        " x" .. number(item.count, "1"))
    end
  end
  if #(expected.appellations or {}) > 0 then
    add(lines, "  appellation ids: " .. U.join_ids(expected.appellations, 10))
  end
  local points = {}
  for _, key in ipairs({"honor_points", "living_points", "jury_points",
                        "crime_points", "aa_points", "lp"}) do
    if expected[key] and expected[key] ~= 0 then
      points[#points + 1] = key .. "=" .. tostring(expected[key])
    end
  end
  if #points > 0 then add(lines, "  points: " .. table.concat(points, " ")) end
  if #(expected.skills or {}) > 0 then
    add(lines, "  skill ids: " .. U.join_ids(expected.skills, 10))
  end
  if #(expected.interactions or {}) > 0 then
    add(lines, "  interaction ids: " .. U.join_ids(expected.interactions, 10))
  end
  if #(expected.remove_items or {}) > 0 then
    for _, item in ipairs(expected.remove_items) do
      add(lines, "  removes item " .. tostring(item.item_id or "?") ..
        " x" .. number(item.count, "1"))
    end
  end
  if #(rewards.supply_acts or {}) > 0 then
    add(lines, "Supply acts:")
    for index = 1, math.min(#rewards.supply_acts, 10) do
      local act = rewards.supply_acts[index]
      add(lines, "  act " .. tostring(act.act_id) .. " " ..
        tostring(act.act_type or "?") .. " [" ..
        tostring(act.component_kind or "?") .. "]")
    end
  end
  add(lines, "No reward is claimed until the Classic client confirms it.")
  return lines
end

function Pages.sources(state)
  local lines = {}
  local step = state:current_step()
  if not step then
    add(lines, "No selected step.")
    return lines
  end
  add(lines, "Sources for step act " .. tostring(step.act_id) .. ":")
  for index = 1, math.min(#(step.locations or {}), 5) do
    local location = step.locations[index]
    add(lines, "  candidate world " .. tostring(location.world or "?") ..
      " @ " .. number(location.x, "?") .. ", " .. number(location.y, "?") ..
      ", " .. number(location.z, "?") ..
      (location.schedule and " [conditional schedule]" or ""))
  end
  if #(step.locations or {}) == 0 then
    add(lines, "  no static spawn location (status " .. tostring(step.status) .. ")")
  end
  for _, entry in ipairs(step.item_sources or {}) do
    add(lines, "  item " .. tostring(entry.item_id or "?") .. " sources:")
    for index = 1, math.min(#(entry.sources or {}), 3) do
      local source = entry.sources[index]
      add(lines, "    " .. tostring(source.type or "?") ..
        " npc/doodad ids " .. U.join_ids(source.npc_ids or source.doodad_ids or {}, 6))
    end
    if #(entry.sources or {}) == 0 then add(lines, "    none modeled") end
  end
  if step.sources and #step.sources > 0 then
    add(lines, "Dynamic/summon sources:")
    for index = 1, math.min(#step.sources, 4) do
      local source = step.sources[index]
      add(lines, "  effect " .. tostring(source.effect_id or "?") ..
        " skill ids " .. U.join_ids(source.skill_ids or {}, 6))
    end
  end
  for index = 1, math.min(#(step.spheres or {}), 3) do
    local sphere = step.spheres[index]
    add(lines, "  sphere world " .. tostring(sphere.world or "?") ..
      " radius " .. number(sphere.radius, "?"))
  end
  add(lines, "All positions are 1.2 candidates, not verified Classic coordinates.")
  return lines
end

function Pages.branches(state)
  local lines = {}
  local quest = state.quest
  if not quest then
    add(lines, "No quest selected.")
    return lines
  end
  add(lines, "Optional failure branches. They are not required for successful completion.")
  local any = false
  for _, branch_name in ipairs({"fail", "drop"}) do
    local components = (quest.branches or {})[branch_name] or {}
    for _, component in ipairs(components) do
      any = true
      add(lines, branch_name:upper() .. " component " .. tostring(component.component_id) ..
        " (" .. tostring(component.component_kind or branch_name) .. ")")
      for _, step in ipairs(component.steps or {}) do
        local target, count = step_target(step)
        add(lines, "  act " .. tostring(step.act_id) .. " " .. tostring(step.act_type) ..
          " -> " .. target .. " x" .. tostring(count) ..
          " [" .. tostring(step.status) .. "]")
        if step.act_detail then
          local detail = step.act_detail
          add(lines, "    detail id " .. tostring(step.act_detail_id) ..
            (detail.exp and (" exp=" .. tostring(detail.exp)) or "") ..
            (detail.amount and (" amount=" .. tostring(detail.amount)) or "") ..
            (detail.item_id and (" item=" .. tostring(detail.item_id)) or ""))
        end
      end
    end
  end
  if not any then add(lines, "No Fail/Drop branch components in this catalog record.") end
  add(lines, "Branch rewards are never counted as successful expected rewards.")
  return lines
end

function Pages.info(state)
  local lines = {}
  local quest = state.quest
  local meta = state:meta() or {}
  if quest then
    add(lines, "Internal 1.2 name: " .. tostring(quest.name or "(none)"))
    add(lines, "Category " .. tostring(quest.category_id) .. " " ..
      tostring(quest.category_name or "") .. " | level " .. tostring(quest.level) ..
      " | min level " .. tostring(quest.min_level))
    add(lines, "Catalog status: " .. tostring(quest.status) ..
      " | zone " .. tostring(quest.zone_id) ..
      " | repeatable " .. tostring(quest.repeatable))
    if quest.blockers and #quest.blockers > 0 then
      add(lines, "Blockers: " .. table.concat(quest.blockers, "; "))
    end
    add(lines, "Classic completion (observed): " .. state:completion_text())
    add(lines, "Classic runtime title: " .. state:classic_title())
    local body = state:classic_body()
    add(lines, "Classic runtime body: " ..
      (body and U.truncate(body, 220) or "(unavailable; fallback Quest#" .. tostring(quest.id) .. ")"))
    add(lines, "Manual step acknowledgement: " ..
      (state:is_manually_acked() and "yes" or "no") .. " (not completion)")
  else
    add(lines, "No quest selected.")
  end
  add(lines, "Expected data version: " .. tostring(meta.data_version or "1.2"))
  local db = ((meta.inputs or {}).db or {})
  add(lines, "Database sha256: " .. tostring(db.sha256 or "unknown"))
  local gen = meta.generator or {}
  add(lines, "Builder: " .. tostring(gen.name or "?") .. " " ..
    tostring(gen.version or "?") .. " commit " .. tostring(gen.code_commit or "?"))
  add(lines, "Compact names/texts are Korean internal 1.2 data, not Classic localization.")
  add(lines, "Classic title/body are read at runtime from the client API.")
  add(lines, "Current Classic world is not exposed; candidate worlds need explicit confirmation.")
  add(lines, "Route 330 -> 6198 is a candidate starter route, not validated or optimal.")
  local notices = meta.notices or {}
  add(lines, "Routes/warnings: " .. tostring(notices.route or ""))
  return lines
end

function Pages.lines(state, page)
  local builder = Pages[page] or Pages.step
  local lines = builder(state)
  if #lines == 0 then lines = {"(empty page)"} end
  return lines
end

function Pages.slice(lines, page, per_page)
  per_page = per_page or PER_PAGE
  local page_count = math.max(1, math.ceil(#lines / per_page))
  page = U.clamp(tonumber(page) or 1, 1, page_count)
  local first = (page - 1) * per_page + 1
  local last = math.min(#lines, first + per_page - 1)
  local out = {}
  for index = first, last do out[#out + 1] = lines[index] end
  return out, page, page_count
end

Pages.PER_PAGE = PER_PAGE

return Pages
