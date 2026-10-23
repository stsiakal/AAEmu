-- Per-character UI data (character options and key bindings) lived in the generic `options`
-- table (`owner` / `key` / `value`) since the C# server. The client treats the payload as an
-- opaque blob per UI-data type and rewrites it as a whole, so the rows now live in their own
-- table keyed by character and type; `systems::services::character_packets` reads and writes
-- types 1 (character option), 2 (key bindings) and 5 (common server data).
--
-- The migration copies numeric legacy keys only: `options.key` is a varchar and non-numeric
-- keys were never UI data (the Rust server parsed them as u16 on load and failed otherwise).

CREATE TABLE IF NOT EXISTS `character_ui_data` (
  `character_id` int unsigned NOT NULL,
  `ui_data_type` smallint unsigned NOT NULL COMMENT '1 character option, 2 key bindings, 5 common server data; native types are 0..6',
  `data` mediumtext NOT NULL COMMENT 'Opaque client blob (UTF-8, at most 8191 bytes)',
  `updated_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (`character_id`,`ui_data_type`) USING BTREE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Per-character client UI data (options, key bindings)';

INSERT IGNORE INTO `character_ui_data` (`character_id`, `ui_data_type`, `data`)
SELECT `owner`, CAST(TRIM(`key`) AS UNSIGNED), `value`
FROM `options`
WHERE TRIM(`key`) REGEXP '^[0-9]{1,5}$' AND CAST(TRIM(`key`) AS UNSIGNED) <= 6;
