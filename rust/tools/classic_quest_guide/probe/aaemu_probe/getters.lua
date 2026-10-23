-- Getter catalogue for the one-shot API inventory, the target dump and the
-- member-existence probe. Every entry that is ever called only reads state.
-- Anything that changes state (Set*, Equip*, Invite*, Change*, Dispatch*,
-- Express*, TargetUnit, ClearCursor, Move*, ToggleMap*, Save*) is absent from
-- the call lists, and the runtime refuses those prefixes as a second guard;
-- SURFACE_NAMES below only reads the member type, never calls it.

local G = {}

-- {library, method, arg1, arg2, ...}. Libraries are fields of the api table.
G.INVENTORY = {
  {"Unit", "GetCurrentZoneGroup"},
  {"Unit", "GetHighAbilityRscInfo"},
  {"Unit", "GetUnitId", "player"},
  {"Unit", "UnitName", "player"},
  {"Unit", "UnitInfo", "player"},
  {"Unit", "UnitModifierInfo", "player"},
  {"Unit", "UnitHealth", "player"},
  {"Unit", "UnitMaxHealth", "player"},
  {"Unit", "UnitMana", "player"},
  {"Unit", "UnitMaxMana", "player"},
  {"Unit", "UnitClass", "player"},
  {"Unit", "UnitGearScore", "player"},
  {"Unit", "UnitTeamAuthority", "player"},
  {"Unit", "UnitIsTeamMember", "player"},
  {"Unit", "UnitIsForceAttack", "player"},
  {"Unit", "UnitIsOffline", "player"},
  {"Unit", "GetFactionName", "player"},
  {"Unit", "GetUnitScreenNameTagOffset", "player"},
  {"Unit", "GetUnitScreenPosition", "player"},
  {"Unit", "UnitWorldPosition", "player"},
  {"Unit", "UnitDistance", "player"},
  {"Unit", "UnitBuffCount", "player"},
  {"Unit", "UnitDeBuffCount", "player"},
  {"Unit", "UnitBuff", "player", 1},
  {"Unit", "UnitDeBuff", "player", 1},
  -- Undocumented names seen in the real api.lua; recorded as missing if absent.
  {"Unit", "UnitLevel", "player"},
  {"Unit", "UnitHeirLevel", "player"},
  {"Unit", "UnitRace", "player"},
  {"Unit", "UnitGender", "player"},
  {"Unit", "UnitCombatState", "player"},
  {"Unit", "GetUnitType", "player"},
  {"Unit", "GetUnitTypeString", "player"},
  {"Unit", "GetNpcInfo", "player"},
  {"Unit", "GetTargetKindType", "player"},
  {"Unit", "GetCombatRelationshipStr", "player"},
  {"Unit", "GetUnitMateType", "player"},
  {"Team", "IsPartyTeam"},
  {"Team", "IsPartyRaid"},
  {"Team", "GetTeamDistributorName"},
  {"Team", "GetTeamPlayerIndex"},
  {"Bag", "Capacity"},
  {"Bag", "CountItems"},
  {"Bag", "GetCurrency"},
  {"Bag", "GetBagItemInfo", 1, 1},
  {"Bank", "Capacity"},
  {"Bank", "CountItems"},
  {"Bank", "GetCurrency"},
  {"Cursor", "GetCursorPickedBagItemIndex"},
  {"Cursor", "GetCursorPickedBagItemAmount"},
  {"Cursor", "GetCursorInfo"},
  {"Time", "GetUiMsec"},
  {"Time", "GetLocalTime"},
  {"Time", "GetGameTime"},
  {"Input", "IsShiftKeyDown"},
  {"Input", "IsControlKeyDown"},
  {"Input", "IsAltKeyDown"},
  {"Input", "GetMousePos"},
  {"Player", "GetShowingAppellation"},
  {"Player", "GetGamePoints"},
  {"Player", "GetCrimeInfo"},
  {"Interface", "GetScreenWidth"},
  {"Interface", "GetScreenHeight"},
  {"Interface", "GetUIScale"},
  {"Ability", "GetUnitClassName", "player"},
  {"Ability", "GetAbilityFromView", 1},
  {"Ability", "GetAbilityFromView", 2},
  {"Ability", "GetAbilityFromView", 3},
  {"Equipment", "GetEquippedSkillsetLunagems", "player"},
  {"Store", "GetSellerShareRatio"},
  {"Store", "GetSpecialtyRatio"},
  {"Store", "GetProductionZoneGroups"},
  {"SiegeWeapon", "GetSiegeWeaponSpeed"},
  {"SiegeWeapon", "GetSiegeWeaponTurnSpeed"},
  {"ItemEnchant", "GetRatioInfos"},
  {"ItemEnchant", "GetEnchantItemInfo"},
  {"ItemEnchant", "GetSupportItemInfo"},
  {"ItemEnchant", "GetTargetItemInfo"},
  {"Map", "GetPlayerSextants"},
  {"Option", "GetOnlyUseMyPortalSetting"},
  {"Option", "GetCustomCloneModelCountSetting"},
  {"Option", "GetCustomCloneModeSetting"},
}

-- Equipment slots 1..28 (GetEquippedItemTooltipInfo(slot)), expanded at run time.
G.EQUIP_SLOTS = 28

-- One-shot calls whose argument is the current zone group.
G.ZONE_CALLS = {
  {"Zone", "GetZoneStateInfoByZoneId"},
}

-- Quest getters called for every questTypeId seen in CHAT_MSG_QUEST (arg 9)
-- and QUEST_LEFT_TIME_UPDATED (arg 1).
G.QUEST_GETTERS = {
  "IsCompleted", "GetActiveQuestTitle", "GetQuestContextMainTitle",
  "GetQuestContextBody",
}

-- Fixed member names probed per library. The sandbox has no `next`, so a
-- library cannot be enumerated; these are the documented names from api.lua
-- plus the undocumented names already tried. Only the member type is read,
-- nothing is called. State-changing names stay in the list because their
-- existence is part of the surface; the probe never calls them.
G.SURFACE_NAMES = {
  Log = {"Info", "Err", "WriteEventParameters"},
  File = {"Write", "Read"},
  Interface = {
    "CreateWindow", "CreateEmptyWindow", "CreateWidget", "CreateStatusBar",
    "CreateComboBox", "ApplyButtonSkin", "SetTooltipOnPos", "Free",
    "GetScreenWidth", "GetScreenHeight", "GetUIScale",
  },
  Unit = {
    "GetUnitNameById", "GetUnitInfoById", "GetUnitScreenPosition",
    "UnitDistance", "GetUnitId", "UnitBuffCount", "UnitName", "UnitBuff",
    "UnitWorldPosition", "UnitDeBuffCount", "UnitDeBuff", "UnitHealth",
    "UnitMaxHealth", "UnitMana", "UnitMaxMana", "UnitInfo", "UnitModifierInfo",
    "UnitClass", "UnitGearScore", "UnitTeamAuthority", "UnitIsTeamMember",
    "UnitIsForceAttack", "GetFactionName", "GetUnitScreenNameTagOffset",
    "GetHighAbilityRscInfo", "UnitIsOffline", "GetOverHeadMarkerUnitId",
    "GetCurrentZoneGroup", "TargetUnit",
    -- Undocumented names already tried by the probe.
    "UnitLevel", "UnitHeirLevel", "UnitRace", "UnitGender", "UnitCombatState",
    "UnitHiddenBuffCount", "UnitRemovableDebuffCount", "GetUnitType",
    "GetUnitTypeString", "GetNpcInfo", "GetTargetKindType",
    "GetCombatRelationshipStr", "GetUnitMateType", "GetUnitMateTypeById",
    "GetUnitGradeById", "GetUnitWorldPosition",
  },
  Team = {
    "InviteToTeam", "SetRole", "IsPartyTeam", "IsPartyRaid",
    "GetMemberIndexByName", "GetTeamDistributorName", "GetTeamPlayerIndex",
    "GetRole", "MoveTeamMember",
  },
  Bag = {
    "EquipBagItem", "GetBagItemInfo", "CountItems", "Capacity",
    "CountBagItemByItemType", "GetCurrency",
  },
  Cursor = {
    "ClearCursor", "SetCursorImage", "GetCursorPickedBagItemIndex",
    "GetCursorPickedBagItemAmount", "GetCursorInfo",
  },
  Time = {
    "GetUiMsec", "GetLocalTime", "PeriodTimeToDate", "TimeToDate",
    "GetGameTime",
  },
  Quest = {
    "IsCompleted", "GetActiveQuestTitle", "GetQuestContextMainTitle",
    "GetQuestContextBody",
  },
  Input = {"IsShiftKeyDown", "IsControlKeyDown", "IsAltKeyDown", "GetMousePos"},
  Player = {
    "ChangeAppellation", "GetShowingAppellation", "GetGamePoints",
    "GetCrimeInfo",
  },
  Ability = {
    "GetBuffTooltip", "GetAbilityFromView", "IsActiveAbility",
    "GetSkillsetNameById", "GetUnitClassName", "GetClassNameFromSkillsetIds",
  },
  Item = {"GetItemInfoByType"},
  Skill = {"GetSkillTooltip"},
  Store = {
    "GetSellerShareRatio", "GetSpecialtyRatio", "GetProductionZoneGroups",
    "GetSellableZoneGroups", "GetSpecialtyRatioBetween",
  },
  Bank = {"CountItems", "Capacity", "GetCurrency", "GetLinkText"},
  Equipment = {
    "GetEquippedItemTooltipInfo", "GetEquippedItemTooltipText",
    "GetEquippedSkillsetLunagems",
  },
  SiegeWeapon = {"GetSiegeWeaponSpeed", "GetSiegeWeaponTurnSpeed"},
  ItemEnchant = {
    "GetRatioInfos", "GetEnchantItemInfo", "GetSupportItemInfo",
    "GetTargetItemInfo",
  },
  Map = {"ToggleMapWithPortal", "GetPlayerSextants"},
  Zone = {"GetZoneStateInfoByZoneId"},
  Craft = {
    "GetCraftBaseInfo", "GetCraftMaterialInfo", "GetCraftProductInfo",
    "GetCraftTypeByItemType", "GetCraftTypesByName",
  },
  Option = {
    "GetOnlyUseMyPortalSetting", "SetOnlyUseMyPortalSetting",
    "GetCustomCloneModelCountSetting", "SetCustomCloneModelCountSetting",
    "GetCustomCloneModeSetting", "SetCustomCloneModeSetting",
  },
  Chat = {"DispatchChatMessage", "ExpressEmotion"},
}

-- The only per-target getters used by the target dump. Each entry is tried
-- with the "target" token; entries flagged by_id are also tried with the unit id.
G.TARGET = {
  {"Unit", "GetUnitId"},
  {"Unit", "UnitName"},
  {"Unit", "UnitInfo"},
  {"Unit", "UnitModifierInfo"},
  {"Unit", "UnitHealth"},
  {"Unit", "UnitMaxHealth"},
  {"Unit", "UnitMana"},
  {"Unit", "UnitMaxMana"},
  {"Unit", "UnitClass"},
  {"Unit", "UnitGearScore"},
  {"Unit", "UnitTeamAuthority"},
  {"Unit", "UnitIsTeamMember"},
  {"Unit", "UnitIsForceAttack"},
  {"Unit", "UnitIsOffline"},
  {"Unit", "GetFactionName"},
  {"Unit", "GetUnitScreenNameTagOffset"},
  {"Unit", "GetUnitScreenPosition"},
  {"Unit", "UnitWorldPosition"},
  {"Unit", "UnitDistance"},
  {"Unit", "UnitBuffCount"},
  {"Unit", "UnitDeBuffCount"},
  {"Unit", "UnitLevel"},
  {"Unit", "UnitHeirLevel"},
  {"Unit", "UnitRace"},
  {"Unit", "UnitGender"},
  {"Unit", "UnitCombatState"},
  {"Unit", "UnitHiddenBuffCount"},
  {"Unit", "UnitRemovableDebuffCount"},
  {"Unit", "GetUnitType"},
  {"Unit", "GetUnitTypeString"},
  {"Unit", "GetNpcInfo"},
  {"Unit", "GetTargetKindType"},
  {"Unit", "GetCombatRelationshipStr"},
  {"Unit", "GetUnitMateType"},
  {"Ability", "GetUnitClassName"},
}

-- Getters keyed by unit id.
G.TARGET_BY_ID = {
  {"Unit", "GetUnitNameById"},
  {"Unit", "GetUnitInfoById"},
  {"Unit", "GetUnitMateTypeById"},
  {"Unit", "GetUnitGradeById"},
  {"Unit", "GetUnitWorldPosition"},
}

-- Libraries whose method names are listed (names and types only, never called)
-- so undocumented additions show up in the log.
G.SURFACE_LIBS = {
  "File", "Unit", "Team", "Log", "Interface", "Cursor", "Input", "Time",
  "Player", "Bag", "Ability", "Item", "Skill", "Store", "Bank", "Equipment",
  "SiegeWeapon", "ItemEnchant", "Map", "Quest", "Zone", "Craft", "Option",
  "Nametag", "Chat", "Voice", "House",
}

-- Own mount/pet tokens. Dumped on every target change so a targeted pet can
-- be matched to its token and its identity recorded.
G.PET_TOKENS = {"playerpet1", "playerpet2"}

-- Globals probed for existence only (type names, never called).
G.GLOBALS = {
  "UI_EVENTS", "ADDON", "UIC", "UIParent", "X2Util", "X2Player", "X2Unit",
  "X2Quest", "X2Chat", "X2Bag", "X2Time", "X2House", "X2Map", "X2Hotkey",
  "W_CTRL", "W_ICON", "W_UNIT", "W_ETC", "W_BAR", "W_MONEY", "W_BTN",
}

local FORBIDDEN = {
  "Set", "Equip", "Invite", "Change", "Dispatch", "Express", "TargetUnit",
  "ClearCursor", "Move", "ToggleMap", "Save", "Toggle", "Make", "Use",
  "Cast", "Send", "Create", "Free", "Delete", "Remove", "Buy", "Sell",
}

-- Second guard: refuse state-changing names even if one slips into a list.
function G.is_read_only(name)
  if type(name) ~= "string" then return false end
  for _, prefix in ipairs(FORBIDDEN) do
    if name:sub(1, #prefix) == prefix then return false end
  end
  return true
end

return G
