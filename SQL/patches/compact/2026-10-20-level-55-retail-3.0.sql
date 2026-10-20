-- Level 51-55 retail data (3.0.4.2 backport, 2026-10-20).
--
-- Source: D:/aa-re/db-references/3.0.4.2-20240828/compact.sqlite3 (ArcheAge 3.0.4.2,
-- export dated 2024-08-28). Verified there:
--   skills.show = 1 for 23587, 23588, 23589, 23591, 23592, 23593, 23594, 23595, 23596, 23934
--     (the ten ability_level 55 capstones, one per ability; the 1.2 rows have show = 'f');
--   levels.total_exp 51..55 = 5174400, 6550320, 8751792, 12164073, 17453110
--     (the 1.2 rows are the placeholder curve 8082000, 16698960, 36282960, 80346960, 179307360);
--   quest_supplies.exp at level 51..55 = 9023, 12604, 16986, 21989, 27512
--     (the 1.2 rows are 5100, 6800, 8500, 10200, 11900; copper is already identical).
--
-- 1.2 values kept: levels.skill_points (24..28) and quest_supplies.copper (3300..3728).
-- total_mate_exp is deliberately left at the 1.2 values: the 3.0 rows for 51-55
-- (1032880, 1308064, 1748358, 2430815, 3488622) rise above the untouched 1.2 level 56 row
-- (2849000), and ExperienceManager.Load requires strictly increasing total_mate_exp.
--
-- Apply offline to the server's compact.sqlite3 copy (runtime loaders open it read-only),
-- after backing the database up; the script is idempotent and rerun-safe:
--   sqlite3 rust/crates/aaemu-game/app/Data/compact.sqlite3 < SQL/patches/compact/2026-10-20-level-55-retail-3.0.sql
-- Every UPDATE is guarded by the 1.2 original value, so an already patched or customized
-- row is left alone.
BEGIN;

UPDATE "skills" SET "show" = 't'
WHERE "id" IN (23587, 23588, 23589, 23591, 23592, 23593, 23594, 23595, 23596, 23934)
  AND "show" = 'f' AND "ability_level" = 55;

UPDATE "levels" SET "total_exp" = 5174400 WHERE "id" = 51 AND "total_exp" = 8082000;
UPDATE "levels" SET "total_exp" = 6550320 WHERE "id" = 52 AND "total_exp" = 16698960;
UPDATE "levels" SET "total_exp" = 8751792 WHERE "id" = 53 AND "total_exp" = 36282960;
UPDATE "levels" SET "total_exp" = 12164073 WHERE "id" = 54 AND "total_exp" = 80346960;
UPDATE "levels" SET "total_exp" = 17453110 WHERE "id" = 55 AND "total_exp" = 179307360;

UPDATE "quest_supplies" SET "exp" = 9023 WHERE "level" = 51 AND "exp" = 5100;
UPDATE "quest_supplies" SET "exp" = 12604 WHERE "level" = 52 AND "exp" = 6800;
UPDATE "quest_supplies" SET "exp" = 16986 WHERE "level" = 53 AND "exp" = 8500;
UPDATE "quest_supplies" SET "exp" = 21989 WHERE "level" = 54 AND "exp" = 10200;
UPDATE "quest_supplies" SET "exp" = 27512 WHERE "level" = 55 AND "exp" = 11900;

COMMIT;
