# Classic probe 0.1.1 — live findings (2026-10-22, ~3.5 min, zone group 5)

Sandbox: only print, string, table, math, pairs, ipairs, tonumber, tostring, type, pcall, xpcall, unpack, getmetatable, require, baseDir + api and UI constants. No _G/next/select/setmetatable/error/io/os. X2Unit/X2Quest/X2Bag/X2Chat/X2House/X2Map hidden; X2Player, X2Util, X2Hotkey present.

## Events: 217 registered, 0 failed, 26 fired
- Noise (99.9%): END_QUEST_CHAT_BUBBLE, TARGET_OVER — drop.
- CHAT_MSG_QUEST: [text, name, unitIdHex, isPlayer, kind, n, n, n, questTypeId, bool] — arg9 = questTypeId (0 = flavour).
- QUEST_LEFT_TIME_UPDATED: [questTypeId, 0].
- ADDED_ITEM: ["|i<itemTypeId>,...;", count, source, ""] — source 10 after kills, 35 after quest turn-in (inferred).
- REMOVED_ITEM: [link, count, "consume", 40, ""].
- BAG_UPDATE: [1, slot] then [-1,-1].
- PLAYER_MONEY: [delta, "delta", 0, ""]; EXP_CHANGED: [unitHex, amount, "amount"].
- UNIT_COMBAT_STATE_CHANGED: [bool, unitHex]; TARGET_CHANGED: [hexId, "npc"]; NPC_INTERACTION_END: {}.
- SPELLCAST_START: ["Open", 1000, "player", false]; SKILL_LEARNED: ["Charge","skill"]; LABORPOWER_CHANGED; ACTABILITY_EXPERT_CHANGED.
- DYNAMIC_ACTION_BAR_SHOW: ["npc"|"doodad", N] — N stable for quest NPCs (Lucius 28855, Gallagher 28976, Mayor Gott 28853), differs between same-name mobs → probably not a template id; test across respawns.
- Never fired: ITEM/MONEY_ACQUISITION_BY_LOOT, INTERACTION_*, LOOT_*, QUEST_HIDDEN_*, QUEST_TASK_READY, doodad and mount events.

## Unit identity
Runtime ids only. GetUnitInfoById → {type:"npc", level, name, faction}; UnitInfo/UnitModifierInfo empty for NPCs; GetNpcInfo/GetUnitType missing. No template id. Fallback fingerprint: name + max health + level + faction.

## API
Items expose itemType (the item type id), name, stack, grade, cost, category. Position: UnitWorldPosition. Zone name: GetZoneStateInfoByZoneId. Quest getters not yet called. API surface listing empty (no `next`).

## Quest observability
Dialogue, questTypeId and NPC observable. Accept/progress/complete inferred: turn-in = ADDED_ITEM source 35 + PLAYER_MONEY + EXP_CHANGED in the same ms. Call Quest:IsCompleted(id) / GetActiveQuestTitle(id) for each id seen (untested).

## Gaps
Template ids, loot window contents, quest objectives, mount events, merchant data, skill ids, combat log.

## Recorder design (derived records)
Kill = combat ends + EXP_CHANGED; Loot = ADDED_ITEM source 10 within 3 s of the kill (item id, npc fingerprint, position); container contents = "Open" cast + consume + same-ms adds; quest reward = source 35 + CHAT_MSG_QUEST id. Catalogues of NPCs (fingerprint + positions) and items.

Probe 0.2: call Quest getters per seen id; fix surface listing without `next` (probe known names); dump the mount unit and a corpse; walk a loot window and a merchant; test DYNAMIC_ACTION_BAR_SHOW N across respawns.
