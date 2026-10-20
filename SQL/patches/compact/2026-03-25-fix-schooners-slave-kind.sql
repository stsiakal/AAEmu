-- Fix: correct SlaveKind for merchant schooners.
-- Previously these ship templates were using the SmallSailingShip class (slave_kind_id = 2),
-- which made their handling/physics match small sailing ships instead of the intended class.
--
-- This patch sets slave_kind_id = 10 for the schooner templates by their template ids.

UPDATE `slaves`
SET `slave_kind_id` = 10
WHERE `id` IN (75, 80)
  AND `slave_kind_id` = 2;   -- original SmallSailingShip value; reruns and custom rows stay untouched

