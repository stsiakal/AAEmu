"""Unit tests for the obsidian-armor backport with small in-memory databases."""

import json
import os
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from backport import (  # noqa: E402
    Borrower,
    Db,
    apply_patch,
    build_assets_section,
    build_backport,
    build_closure,
    pak_names_to_write,
    patch_client_db,
    refuse_protected_client,
    render_patch,
    resolve_item_set,
    run_backport,
    weapons_comparison,
    write_asset_list,
)

BASE_SCHEMA = """
CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, icon_id INTEGER,
    buff_id INTEGER, LEVEL INTEGER, milestone_id INTEGER, translate TEXT);
CREATE TABLE item_armors (id INTEGER PRIMARY KEY, item_id INTEGER,
    slot_type_id INTEGER, type_id INTEGER, mod_set_id INTEGER, asset_id INTEGER);
CREATE TABLE buffs (id INTEGER PRIMARY KEY, name TEXT, duration INTEGER);
CREATE TABLE unit_modifiers (id INTEGER PRIMARY KEY, owner_type TEXT,
    owner_id INTEGER, unit_attribute_id INTEGER, unit_modifier_type_id INTEGER,
    value INTEGER, linear_level_bonus INTEGER);
CREATE TABLE equip_item_attr_modifiers (id INTEGER PRIMARY KEY, alias TEXT,
    str_weight INTEGER, dex_weight INTEGER, sta_weight INTEGER, int_weight INTEGER,
    spi_weight INTEGER);
CREATE TABLE crafts (id INTEGER PRIMARY KEY, title TEXT, skill_id INTEGER,
    milestone_id INTEGER, translate TEXT);
CREATE TABLE craft_products (id INTEGER PRIMARY KEY, craft_id INTEGER,
    item_id INTEGER, amount INTEGER);
CREATE TABLE craft_materials (id INTEGER PRIMARY KEY, craft_id INTEGER,
    item_id INTEGER, amount INTEGER);
CREATE TABLE craft_pack_crafts (id INTEGER PRIMARY KEY, craft_pack_id INTEGER,
    craft_id INTEGER);
CREATE TABLE skills (id INTEGER PRIMARY KEY, name TEXT, consume_lp INTEGER,
    actability_group_id INTEGER, cooldown_time INTEGER, SHOW TEXT, milestone_id INTEGER);
CREATE TABLE skill_effects (id INTEGER PRIMARY KEY, skill_id INTEGER,
    effect_id INTEGER, weight INTEGER, chance INTEGER);
CREATE TABLE effects (id INTEGER PRIMARY KEY, actual_id INTEGER, actual_type TEXT);
CREATE TABLE craft_effects (id INTEGER PRIMARY KEY, wi_id INTEGER);
CREATE TABLE buff_effects (id INTEGER PRIMARY KEY, buff_id INTEGER, chance INTEGER,
    stack INTEGER, ab_level INTEGER);
CREATE TABLE icons (id INTEGER PRIMARY KEY, filename TEXT, name TEXT);
CREATE TABLE item_assets (id INTEGER PRIMARY KEY, model_id INTEGER, path TEXT);
CREATE TABLE localized_texts (id INTEGER PRIMARY KEY, tbl_name TEXT,
    tbl_column_name TEXT, idx INTEGER, ko TEXT, ko_ver INTEGER, en_us TEXT,
    en_us_ver INTEGER, zh_cn TEXT, zh_cn_ver INTEGER, ja TEXT, ja_ver INTEGER,
    ru TEXT, ru_ver INTEGER, zh_tw TEXT, zh_tw_ver INTEGER, de TEXT, de_ver INTEGER,
    fr TEXT, fr_ver INTEGER);
CREATE TABLE item_weapons (id INTEGER PRIMARY KEY, item_id INTEGER);
CREATE TABLE armor_assets (id INTEGER PRIMARY KEY, name TEXT, slot_type_id INTEGER,
    default_asset_id INTEGER);
CREATE TABLE item_armor_assets (id INTEGER PRIMARY KEY, armor_asset_id INTEGER,
    asset_id INTEGER);
"""

REF_SCHEMA = """
CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, icon_id INTEGER,
    buff_id INTEGER, level INTEGER, auction_charge INTEGER);
CREATE TABLE item_armors (id INTEGER PRIMARY KEY, item_id INTEGER,
    slot_type_id INTEGER, type_id INTEGER, mod_set_id INTEGER, asset_id INTEGER,
    invisible_asset INTEGER);
CREATE TABLE buffs (id INTEGER PRIMARY KEY, name TEXT, duration INTEGER,
    balance_level INTEGER);
CREATE TABLE unit_modifiers (owner_type TEXT, owner_id INTEGER,
    unit_attribute_id INTEGER, unit_modifier_type_id INTEGER, value INTEGER,
    linear_level_bonus INTEGER);
CREATE TABLE equip_item_attr_modifiers (id INTEGER PRIMARY KEY, alias TEXT,
    str_weight INTEGER, dex_weight INTEGER, sta_weight INTEGER, int_weight INTEGER,
    spi_weight INTEGER);
CREATE TABLE crafts (id INTEGER PRIMARY KEY, title TEXT, skill_id INTEGER,
    products_pack_id INTEGER);
CREATE TABLE craft_products (id INTEGER PRIMARY KEY, craft_id INTEGER,
    item_id INTEGER, amount INTEGER);
CREATE TABLE craft_materials (id INTEGER PRIMARY KEY, craft_id INTEGER,
    item_id INTEGER, amount INTEGER);
CREATE TABLE craft_pack_crafts (id INTEGER PRIMARY KEY, craft_pack_id INTEGER,
    craft_id INTEGER);
CREATE TABLE skills (id INTEGER PRIMARY KEY, name TEXT, consume_lp INTEGER,
    actability_group_id INTEGER, cooldown_time INTEGER, show INTEGER,
    account_cooldown INTEGER);
CREATE TABLE skill_effects (id INTEGER PRIMARY KEY, skill_id INTEGER,
    effect_id INTEGER, weight INTEGER, chance INTEGER, check_source_tag_src INTEGER);
CREATE TABLE effects (id INTEGER PRIMARY KEY, actual_id INTEGER, actual_type TEXT);
CREATE TABLE craft_effects (id INTEGER PRIMARY KEY, wi_id INTEGER);
CREATE TABLE buff_effects (id INTEGER PRIMARY KEY, buff_id INTEGER, chance INTEGER,
    stack INTEGER, ab_level INTEGER);
CREATE TABLE icons (id INTEGER PRIMARY KEY, filename TEXT);
CREATE TABLE item_assets (id INTEGER PRIMARY KEY, model_id INTEGER, path TEXT,
    hide_horn INTEGER);
CREATE TABLE localized_texts (tbl_name TEXT, tbl_column_name TEXT, idx INTEGER,
    en_us TEXT);
CREATE TABLE item_weapons (id INTEGER PRIMARY KEY, item_id INTEGER);
CREATE TABLE armor_assets (id INTEGER PRIMARY KEY, slot_type_id INTEGER,
    default_asset_id INTEGER);
CREATE TABLE item_armor_assets (id INTEGER PRIMARY KEY, armor_asset_id INTEGER,
    asset_id INTEGER);
"""


def make_db(schema, rows):
    conn = sqlite3.connect(":memory:")
    conn.row_factory = sqlite3.Row
    conn.executescript(schema)
    for statement, values in rows:
        conn.execute(statement, values)
    conn.commit()
    return Db(conn=conn)


def base_db():
    rows = [
        ("INSERT INTO items VALUES (?,?,?,?,?,?,?)", (12, "Existing mat", 0, 0, 1, 0, "f")),
        ("INSERT INTO items VALUES (?,?,?,?,?,?,?)", (1000, "Base mat", 0, 0, 1, 0, "f")),
        ("INSERT INTO item_assets VALUES (?,?,?)", (5, 1, "objects/item/test.cgf")),
        ("INSERT INTO icons VALUES (?,?,?)", (1, "x.dds", "x")),
        ("INSERT INTO skills VALUES (?,?,?,?,?,?,?)", (500, "Base craft skill", 100, 0, 0, "t", 0)),
        ("INSERT INTO item_weapons VALUES (?,?)", (1, 12)),
        ("INSERT INTO armor_assets VALUES (?,?,?,?)", (5, "old look", 1, 0)),
        ("INSERT INTO item_armor_assets VALUES (?,?,?)", (600, 5, 60)),
        ("INSERT INTO item_assets VALUES (?,?,?)", (60, 0, "objects/characters/test/old.cdf")),
    ]
    return make_db(BASE_SCHEMA, rows)


def ref_db():
    rows = [
        ("INSERT INTO items VALUES (?,?,?,?,?,?)", (10, "Test Armor", 77, 50, 46, 5)),
        ("INSERT INTO items VALUES (?,?,?,?,?,?)", (11, "Test Mat", 78, 0, 10, 0)),
        ("INSERT INTO item_armors VALUES (?,?,?,?,?,?,?)", (900, 10, 1, 1, 77, 5, 0)),
        ("INSERT INTO buffs VALUES (?,?,?,?)", (50, "armor buff", 0, 0)),
        ("INSERT INTO buffs VALUES (?,?,?,?)", (60, "self buff", 0, 0)),
        ("INSERT INTO unit_modifiers VALUES (?,?,?,?,?,?)", ("Buff", 50, 6, 0, 100, 0)),
        ("INSERT INTO equip_item_attr_modifiers VALUES (?,?,?,?,?,?,?)",
         (77, "str:1", 1, 2, 3, 4, 5)),
        ("INSERT INTO crafts VALUES (?,?,?,?)", (20, "Craft Armor", 500, 0)),
        ("INSERT INTO crafts VALUES (?,?,?,?)", (21, "Craft Mat", 900, 0)),
        ("INSERT INTO craft_products VALUES (?,?,?,?)", (200, 20, 10, 1)),
        ("INSERT INTO craft_products VALUES (?,?,?,?)", (201, 21, 11, 1)),
        ("INSERT INTO craft_materials VALUES (?,?,?,?)", (300, 20, 11, 2)),
        ("INSERT INTO craft_materials VALUES (?,?,?,?)", (301, 20, 1000, 1)),
        ("INSERT INTO craft_materials VALUES (?,?,?,?)", (302, 21, 12, 1)),
        ("INSERT INTO craft_pack_crafts VALUES (?,?,?)", (400, 5, 20)),
        ("INSERT INTO skills VALUES (?,?,?,?,?,?,?)", (900, "Test craft skill", 650, 11, 0, 1, 0)),
        ("INSERT INTO skill_effects VALUES (?,?,?,?,?,?)", (950, 900, 960, 0, 100, 0)),
        ("INSERT INTO skill_effects VALUES (?,?,?,?,?,?)", (951, 900, 962, 0, 100, 0)),
        ("INSERT INTO effects VALUES (?,?,?)", (960, 961, "CraftEffect")),
        ("INSERT INTO effects VALUES (?,?,?)", (962, 963, "BuffEffect")),
        ("INSERT INTO craft_effects VALUES (?,?)", (961, 56)),
        ("INSERT INTO buff_effects VALUES (?,?,?,?,?)", (963, 60, 100, 1, 0)),
        ("INSERT INTO localized_texts VALUES (?,?,?,?)", ("items", "name", 10, "Test Armor EN")),
        ("INSERT INTO localized_texts VALUES (?,?,?,?)", ("items", "description", 10, "A test")),
        ("INSERT INTO localized_texts VALUES (?,?,?,?)", ("items", "name", 11, "Test Mat EN")),
        ("INSERT INTO armor_assets VALUES (?,?,?)", (5, 1, 0)),
        ("INSERT INTO item_armor_assets VALUES (?,?,?)", (700, 5, 6)),
        ("INSERT INTO item_assets VALUES (?,?,?,?)",
         (6, 0, "objects/characters/test/x.cdf", 0)),
        ("INSERT INTO icons VALUES (?,?)", (77, "icon_x.dds")),
        ("INSERT INTO icons VALUES (?,?)", (78, "icon_y.dds")),
    ]
    return make_db(REF_SCHEMA, rows)


class BackportTests(unittest.TestCase):
    def test_resolve_item_set(self):
        self.assertEqual(resolve_item_set("obsidian-armor"), list(range(35581, 35626)))
        self.assertEqual(resolve_item_set("5, 7,5"), [5, 7])
        with self.assertRaises(ValueError):
            resolve_item_set("")

    def test_closure_recursion_and_stop_at_base(self):
        base, ref = base_db(), ref_db()
        closure = build_closure(base, ref, [10])
        self.assertEqual(closure["items"], [10, 11])
        self.assertEqual(closure["crafts"], [20, 21])
        self.assertEqual(closure["refused_crafts"], [])
        self.assertEqual(closure["refused_items"], [])

    def test_custom_ids_refused(self):
        base, ref = base_db(), ref_db()
        ref.conn.execute("INSERT INTO items VALUES (?,?,?,?,?,?)", (8000001, "custom mat", 0, 0, 1, 0))
        ref.conn.execute("INSERT INTO crafts VALUES (?,?,?,?)", (8000002, "custom craft", 500, 0))
        ref.conn.execute("INSERT INTO craft_products VALUES (?,?,?,?)", (8000010, 8000002, 11, 1))
        ref.conn.execute("INSERT INTO craft_materials VALUES (?,?,?,?)", (8000011, 8000002, 8000001, 1))
        ref.conn.execute("INSERT INTO craft_materials VALUES (?,?,?,?)", (303, 20, 8000001, 1))
        ref.conn.commit()
        closure = build_closure(base, ref, [10])
        self.assertEqual(closure["items"], [10, 11])
        self.assertNotIn(8000001, closure["items"])
        refused_craft_ids = {entry["craft_id"] for entry in closure["refused_crafts"]}
        self.assertIn(8000002, refused_craft_ids)
        refused_item_ids = {entry["item_id"] for entry in closure["refused_items"]}
        self.assertIn(8000001, refused_item_ids)
        _, _, report = build_backport(base, ref, [10])
        custom_items = {entry["id"] for entry in report["refused"]["custom_items"]}
        self.assertIn(8000001, custom_items)
        custom_crafts = {entry["id"] for entry in report["refused"]["custom_crafts"]}
        self.assertIn(8000002, custom_crafts)

    def test_collision_and_name_conflict_refused(self):
        base, ref = base_db(), ref_db()
        base.conn.execute("INSERT INTO items VALUES (?,?,?,?,?,?,?)",
                          (10, "Different name", 0, 0, 1, 0, "f"))
        base.conn.execute("INSERT INTO crafts VALUES (?,?,?,?,?)",
                          (20, "Existing craft", 500, 0, "f"))
        base.conn.commit()
        _, rows, report = build_backport(base, ref, [10])
        self.assertNotIn(10, [row["id"] for row in rows.get("items", [])])
        self.assertNotIn(20, [row["id"] for row in rows.get("crafts", [])])
        conflict_ids = {entry["id"] for entry in report["refused"]["name_conflicts"]}
        self.assertIn(10, conflict_ids)
        collision_ids = {entry["id"] for entry in report["refused"]["collisions"]}
        self.assertIn(20, collision_ids)

    def test_column_mapping_defaults_and_dropped_columns(self):
        base, ref = base_db(), ref_db()
        _, rows, report = build_backport(base, ref, [10])
        item_rows = {row["id"]: row for row in rows["items"]}
        self.assertEqual(item_rows[10]["LEVEL"], 46)
        self.assertEqual(item_rows[10]["milestone_id"], 0)
        self.assertEqual(item_rows[10]["translate"], "f")
        self.assertEqual(item_rows[11]["translate"], "f")
        self.assertEqual(report["columns_filled_with_1_2_defaults"]["items"]["milestone_id"], 2)
        self.assertEqual(report["columns_filled_with_1_2_defaults"]["items"]["translate"], 2)
        dropped = report["columns_dropped_from_3_0"]["items"]["auction_charge"]
        self.assertEqual(dropped["count"], 1)
        self.assertEqual(dropped["examples"], [{"id": 10, "value": 5}])
        craft_rows = {row["id"]: row for row in rows["crafts"]}
        self.assertEqual(craft_rows[20]["milestone_id"], 0)
        skill_rows = {row["id"]: row for row in rows["skills"]}
        self.assertEqual(skill_rows[900]["SHOW"], "t")
        self.assertEqual(report["boolean_value_conversions"]["skills"]["SHOW"], 1)

    def test_synthesized_ids_and_deterministic_sql(self):
        base, ref = base_db(), ref_db()
        _, rows, report = build_backport(base, ref, [10])
        unit_rows = rows["unit_modifiers"]
        self.assertEqual(unit_rows[0]["id"], 1)
        self.assertEqual(report["synthesized_ids"]["unit_modifiers"]["count"], 1)
        sql_one = render_patch(rows, "b", "r", "cmd", "s1", "s2")
        base2, ref2 = base_db(), ref_db()
        _, rows2, _ = build_backport(base2, ref2, [10])
        sql_two = render_patch(rows2, "b", "r", "cmd", "s1", "s2")
        self.assertEqual(sql_one, sql_two)
        self.assertIn("documented guess: borrowed from 3.0.4.2", sql_one)
        self.assertTrue(sql_one.startswith("-- version_gaps backport"))
        self.assertEqual(sql_one.count("BEGIN;"), 1)
        self.assertEqual(sql_one.count("COMMIT;"), 1)

    def test_skill_effect_chain_and_buff_borrow(self):
        base, ref = base_db(), ref_db()
        _, rows, report = build_backport(base, ref, [10])
        self.assertEqual([row["id"] for row in rows["skills"]], [900])
        self.assertEqual(sorted(row["id"] for row in rows["skill_effects"]), [950, 951])
        self.assertEqual(sorted(row["id"] for row in rows["effects"]), [960, 962])
        self.assertEqual([row["id"] for row in rows["craft_effects"]], [961])
        self.assertEqual([row["id"] for row in rows["buff_effects"]], [963])
        self.assertEqual(sorted(row["id"] for row in rows["buffs"]), [50, 60])
        self.assertEqual(
            report["buff_effects_referenced_buffs_missing_in_1_2"], [60]
        )

    def test_localized_texts_mapping(self):
        base, ref = base_db(), ref_db()
        _, rows, report = build_backport(base, ref, [10])
        localized = rows["localized_texts"]
        by_key = {(row["idx"], row["tbl_column_name"]): row for row in localized}
        name_row = by_key[(10, "name")]
        self.assertEqual(name_row["en_us"], "Test Armor EN")
        self.assertEqual(name_row["ko"], "Test Armor")
        self.assertEqual(name_row["ko_ver"], 0)
        self.assertEqual(name_row["zh_cn"], "")
        desc_row = by_key[(10, "description")]
        self.assertEqual(desc_row["en_us"], "A test")
        self.assertEqual(desc_row["ko"], "")
        self.assertEqual(report["borrowed_rows"]["localized_texts"], 3)

    def test_materials_without_source_and_icons(self):
        base, ref = base_db(), ref_db()
        _, _, report = build_backport(base, ref, [10])
        materials = {entry["item_id"]: entry for entry in report["materials_without_1_2_source"]}
        self.assertEqual(set(materials), {11})
        self.assertEqual(materials[11]["name"], "Test Mat")
        self.assertEqual(materials[11]["retail_crafts_in_3_0"], [21])
        self.assertFalse(materials[11]["custom_only"])
        icons = {(entry["item_id"], entry["icon_id"]) for entry in report["icons_missing_in_1_2"]}
        self.assertEqual(icons, {(10, 77), (11, 78)})

    def test_apply_patch_and_integrity(self):
        with tempfile.TemporaryDirectory() as tmp:
            base_path = os.path.join(tmp, "base.sqlite3")
            ref_path = os.path.join(tmp, "ref.sqlite3")
            out_sql = os.path.join(tmp, "patch.sql")
            out_db = os.path.join(tmp, "patched.sqlite3")
            for path, source in ((base_path, base_db()), (ref_path, ref_db())):
                source.conn.commit()
                target = sqlite3.connect(path)
                source.conn.backup(target)
                target.close()
            report = run_backport(base_path, ref_path, "10", out_sql, out_db)
            self.assertEqual(report["integrity_check"], "ok")
            self.assertTrue(os.path.exists(out_sql))
            conn = sqlite3.connect(out_db)
            try:
                items = conn.execute("select id from items order by id").fetchall()
                crafts = conn.execute("select id from crafts order by id").fetchall()
            finally:
                conn.close()
            self.assertEqual(items, [(10,), (11,), (12,), (1000,)])
            self.assertEqual(crafts, [(20,), (21,)])
            self.assertIn("gear backport", json.dumps(report, ensure_ascii=False))

    def test_weapons_comparison(self):
        base, ref = base_db(), ref_db()
        ref.conn.execute("INSERT INTO items VALUES (?,?,?,?,?,?)", (100, "흑요석 검", 0, 0, 1, 0))
        ref.conn.execute("INSERT INTO item_weapons VALUES (?,?)", (2, 100))
        ref.conn.execute("INSERT INTO items VALUES (?,?,?,?,?,?)", (101, "upgrade one", 0, 0, 1, 0))
        ref.conn.execute("INSERT INTO item_weapons VALUES (?,?)", (3, 101))
        ref.conn.execute("INSERT INTO crafts VALUES (?,?,?,?)", (30, "Upgrade", 500, 0))
        ref.conn.execute("INSERT INTO craft_products VALUES (?,?,?,?)", (210, 30, 101, 1))
        ref.conn.execute("INSERT INTO craft_materials VALUES (?,?,?,?)", (310, 30, 100, 1))
        ref.conn.commit()
        comparison = weapons_comparison(base, ref)
        self.assertEqual(comparison["base_obsidian_weapons"]["count"], 0)
        self.assertEqual(
            comparison["non_obsidian_named_upgrade_products"]["ref"]["count"], 1
        )
        self.assertEqual(
            comparison["non_obsidian_named_upgrade_products"]["ref"]["weapons"], 1
        )


    def test_look_chain_remap(self):
        base, ref = base_db(), ref_db()
        _, rows, report = build_backport(base, ref, [10])
        armor_rows = {row["id"]: row for row in rows["armor_assets"]}
        self.assertEqual(sorted(armor_rows), [6])
        self.assertEqual(armor_rows[6]["slot_type_id"], 1)
        self.assertEqual(armor_rows[6]["name"], "")
        asset_rows = {row["id"]: row for row in rows["item_assets"]}
        self.assertEqual(sorted(asset_rows), [61])
        self.assertEqual(asset_rows[61]["path"], "objects/characters/test/x.cdf")
        iaa = rows["item_armor_assets"]
        self.assertEqual(iaa[0]["armor_asset_id"], 6)
        self.assertEqual(iaa[0]["asset_id"], 61)
        self.assertEqual(rows["item_armors"][0]["asset_id"], 6)
        self.assertEqual(
            report["look_borrows"]["item_armors_asset_id_remap"], {"5": 6}
        )
        self.assertEqual(report["look_borrows"]["armor_assets"]["new_ids"], [6])

    def test_item_assets_path_reuse(self):
        base, ref = base_db(), ref_db()
        base.conn.execute(
            "INSERT INTO item_assets VALUES (?,?,?)",
            (500, 0, "objects/characters/test/x.cdf"),
        )
        base.conn.commit()
        _, rows, report = build_backport(base, ref, [10])
        self.assertNotIn("item_assets", rows)
        self.assertEqual(rows["item_armor_assets"][0]["asset_id"], 500)
        self.assertEqual(
            report["look_borrows"]["item_assets"]["reused_ids"][0]["id"], 500
        )

    def test_icon_reuse_and_remap(self):
        base, ref = base_db(), ref_db()
        base.conn.execute("INSERT INTO icons VALUES (?,?,?)", (77, "icon_x.dds", "x"))
        base.conn.execute("INSERT INTO icons VALUES (?,?,?)", (78, "old_y.dds", "y"))
        base.conn.commit()
        _, rows, report = build_backport(base, ref, [10])
        item_rows = {row["id"]: row for row in rows["items"]}
        self.assertEqual(item_rows[10]["icon_id"], 77)
        self.assertEqual(item_rows[11]["icon_id"], 79)
        icon_rows = {row["id"]: row for row in rows["icons"]}
        self.assertEqual(icon_rows[79]["filename"], "icon_y.dds")
        self.assertEqual(
            report["look_borrows"]["icon_borrows"]["remapped"],
            [
                {
                    "ref_id": 78,
                    "id": 79,
                    "base_filename": "old_y.dds",
                    "ref_filename": "icon_y.dds",
                }
            ],
        )

    def test_asset_list_determinism(self):
        base, ref = base_db(), ref_db()
        _, _, report = build_backport(base, ref, [10])
        with tempfile.TemporaryDirectory() as tmp:
            cdf = os.path.join(tmp, "game", "objects", "characters", "test", "x.cdf")
            os.makedirs(os.path.dirname(cdf))
            with open(cdf, "w", encoding="utf-8") as handle:
                handle.write(
                    '<CharacterDefinition><Model File="objects/characters/test/x.chr" '
                    'Material="objects/characters/test/x.mtl"/></CharacterDefinition>'
                )
            chr_path = os.path.join(
                tmp, "game", "objects", "characters", "test", "x.chr"
            )
            with open(chr_path, "wb") as handle:
                handle.write(b"CryTek")
            mtl = os.path.join(tmp, "game", "objects", "characters", "test", "x.mtl")
            with open(mtl, "w", encoding="utf-8") as handle:
                handle.write(
                    '<Material><Textures><Texture Map="Diffuse" '
                    'File="objects/Textures/t.tif"/></Textures></Material>'
                )
            texture = os.path.join(tmp, "game", "objects", "textures", "t.dds")
            os.makedirs(os.path.dirname(texture))
            with open(texture, "wb") as handle:
                handle.write(b"DDS ")
            pak10 = {
                "game/objects/characters/test/x.cdf",
                "game/objects/characters/test/x.chr",
                "game/objects/characters/test/x.mtl",
                "game/objects/textures/t.dds",
                "game/ui/icon/icon_x.dds",
                "game/ui/icon/icon_y.dds",
            }
            pak12 = {"game/objects/textures/t.dds"}
            first = build_assets_section(report["assets_needed"], tmp, pak10, pak12)
            second = build_assets_section(report["assets_needed"], tmp, pak10, pak12)
            self.assertEqual(first, second)
            self.assertEqual(first["closure_files"], 6)
            self.assertEqual(first["by_extension"]["cdf"], 1)
            self.assertIn("game/objects/textures/t.dds", first["shared_with_1_2_pak"])
            self.assertNotIn(
                "game/objects/characters/test/x.cdf", first["shared_with_1_2_pak"]
            )
            out_one = os.path.join(tmp, "a.txt")
            out_two = os.path.join(tmp, "b.txt")
            write_asset_list(out_one, first)
            write_asset_list(out_two, second)
            with open(out_one, encoding="utf-8") as handle:
                text_one = handle.read()
            with open(out_two, encoding="utf-8") as handle:
                text_two = handle.read()
            self.assertEqual(text_one, text_two)
            self.assertIn("game/objects/characters/test/x.chr", text_one)


class ClientPatchTest(unittest.TestCase):
    SCHEMA = """
    CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, icon_id INTEGER,
        male_icon_id INTEGER, LEVEL INTEGER);
    CREATE TABLE icons (id INTEGER PRIMARY KEY, filename TEXT, name TEXT);
    CREATE TABLE localized_texts (id INTEGER PRIMARY KEY, tbl_name TEXT, idx INTEGER,
        en_us TEXT);
    """

    def make(self, path, items, icons, texts=()):
        conn = sqlite3.connect(path)
        conn.executescript(self.SCHEMA)
        conn.executemany("INSERT INTO items VALUES (?,?,?,?,?)", items)
        conn.executemany("INSERT INTO icons VALUES (?,?,?)", icons)
        conn.executemany("INSERT INTO localized_texts VALUES (?,?,?,?)", texts)
        conn.commit()
        conn.close()

    def test_row_selection_idempotence_and_icon_names(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = os.path.join(tmp, "base.sqlite3")
            client = os.path.join(tmp, "client.sqlite3")
            patched = os.path.join(tmp, "patched.sqlite3")
            old_items = [(1, "old", 1, None, 5)]
            old_icons = [(1, "a/old.dds", "old")]
            self.make(base, old_items, old_icons)
            self.make(client, old_items, old_icons)
            self.make(
                patched,
                old_items + [(35592, "new", 12085, 12086, 46)],
                old_icons
                + [(12085, "costume_ar/nu_f_x.dds", ""), (12086, "costume_ar/nu_m_x.dds", "")],
                [(7, "items", 35592, "Obsidian")],
            )
            first = patch_client_db(client, base, patched)
            self.assertEqual(first["inserted"]["items"], 1)
            self.assertEqual(first["inserted"]["icons"], 2)
            self.assertEqual(first["inserted"]["localized_texts"], 1)
            self.assertEqual(first["missing_icons"], [])
            conn = sqlite3.connect(client)
            self.assertEqual(
                conn.execute("SELECT name FROM icons WHERE id = 12086").fetchone()[0],
                "nu_m_x",
            )
            snapshot = [
                conn.execute(f"SELECT * FROM {t} ORDER BY id").fetchall()
                for t in ("items", "icons", "localized_texts")
            ]
            conn.close()
            second = patch_client_db(client, base, patched)
            self.assertEqual(sum(second["inserted"].values()), 0)
            self.assertEqual(second["already_present"]["items"], 1)
            conn = sqlite3.connect(client)
            again = [
                conn.execute(f"SELECT * FROM {t} ORDER BY id").fetchall()
                for t in ("items", "icons", "localized_texts")
            ]
            conn.close()
            self.assertEqual(snapshot, again)

    def test_missing_icon_and_conflict_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = os.path.join(tmp, "base.sqlite3")
            client = os.path.join(tmp, "client.sqlite3")
            patched = os.path.join(tmp, "patched.sqlite3")
            self.make(base, [], [])
            self.make(client, [(9, "other", 0, None, 1)], [])
            self.make(patched, [(9, "mine", 0, None, 1), (10, "x", 99, None, 1)], [])
            report = patch_client_db(client, base, patched)
            self.assertEqual(report["conflicts"]["items"], [9])
            self.assertEqual(report["inserted"]["items"], 1)
            self.assertEqual(report["missing_icons"], [[10, "icon_id", 99]])

    def test_pak_names_and_protected_roots(self):
        with tempfile.TemporaryDirectory() as tmp:
            listing = os.path.join(tmp, "assets.txt")
            with open(listing, "w", encoding="utf-8") as handle:
                handle.write("game/a.dds\nGame/b.cdf\n\ngame/a.dds\n")
            self.assertEqual(
                pak_names_to_write(listing, {"game/a.dds"}), ["game/b.cdf"]
            )
            with self.assertRaises(SystemExit):
                refuse_protected_client("D:/AA1.2")
            with self.assertRaises(SystemExit):
                refuse_protected_client(tmp)  # no game_pak


class EquipSetTests(unittest.TestCase):
    """Equip sets: set rows, bonuses, bonus buffs (+ skill modifiers, tags) and procs."""

    SET_SCHEMA_BASE = """
    ALTER TABLE item_armors ADD COLUMN eiset_id INTEGER;
    CREATE TABLE equip_item_sets (id INTEGER PRIMARY KEY, name TEXT, description TEXT);
    CREATE TABLE equip_item_set_bonuses (id INTEGER PRIMARY KEY, equip_item_set_id INTEGER,
        num_pieces INTEGER, buff_id INTEGER, proc_id INTEGER);
    CREATE TABLE item_procs (id INTEGER PRIMARY KEY, name TEXT, description TEXT,
        skill_id INTEGER, chance_kind_id INTEGER, chance_rate INTEGER);
    CREATE TABLE item_proc_bindings (id INTEGER PRIMARY KEY, item_id INTEGER, proc_id INTEGER);
    CREATE TABLE skill_modifiers (id INTEGER PRIMARY KEY, owner_id INTEGER, owner_type TEXT,
        tag_id INTEGER, skill_id INTEGER, value INTEGER);
    CREATE TABLE tagged_buffs (id INTEGER PRIMARY KEY, tag_id INTEGER, buff_id INTEGER);
    CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT);
    INSERT INTO tags VALUES (565, 'set');
    INSERT INTO item_procs VALUES (1, 'old', '', 500, 1, 100);
    """
    SET_SCHEMA_REF = """
    ALTER TABLE item_armors ADD COLUMN eiset_id INTEGER;
    CREATE TABLE equip_item_sets (id INTEGER PRIMARY KEY, name TEXT);
    CREATE TABLE equip_item_set_bonuses (id INTEGER PRIMARY KEY, buff_id INTEGER,
        equip_item_set_id INTEGER, num_pieces INTEGER, proc_id INTEGER);
    CREATE TABLE item_procs (id INTEGER PRIMARY KEY, chance_kind_id INTEGER,
        chance_rate INTEGER, skill_id INTEGER, trigger_skill_id INTEGER);
    CREATE TABLE item_proc_bindings (id INTEGER PRIMARY KEY, item_id INTEGER, proc_id INTEGER);
    CREATE TABLE skill_modifiers (id INTEGER PRIMARY KEY, owner_id INTEGER, owner_type TEXT,
        tag_id INTEGER, skill_id INTEGER, value INTEGER);
    CREATE TABLE tagged_buffs (id INTEGER PRIMARY KEY, tag_id INTEGER, buff_id INTEGER);
    UPDATE item_armors SET eiset_id = 300 WHERE item_id = 10;
    INSERT INTO equip_item_sets VALUES (300, 'Test Set');
    INSERT INTO equip_item_set_bonuses VALUES (3000, 50, 300, 3, 0);
    INSERT INTO equip_item_set_bonuses VALUES (3001, 0, 300, 5, 31);
    INSERT INTO item_procs VALUES (31, 18, 100, 900, 77);
    INSERT INTO skill_modifiers VALUES (4000, 50, 'Buff', 565, 0, -7);
    INSERT INTO tagged_buffs VALUES (5000, 565, 50);
    INSERT INTO localized_texts VALUES ('equip_item_sets', 'name', 300, 'Test Set EN');
    INSERT INTO localized_texts VALUES ('buffs', 'name', 50, '388 DO NOT TRANSLATE');
    INSERT INTO localized_texts VALUES ('buffs', 'desc', 50, 'Cuts cooldowns');
    """

    def dbs(self):
        base, ref = base_db(), ref_db()
        base.conn.executescript(self.SET_SCHEMA_BASE)
        ref.conn.executescript(self.SET_SCHEMA_REF)
        return Db(conn=base.conn), Db(conn=ref.conn)

    def test_set_bonuses_buffs_modifiers_and_procs_follow_the_items(self):
        base, ref = self.dbs()
        _, rows, report = build_backport(base, ref, [10])
        self.assertEqual([r["id"] for r in rows["equip_item_sets"]], [300])
        self.assertEqual(
            sorted((r["num_pieces"], r["buff_id"], r["proc_id"]) for r in rows["equip_item_set_bonuses"]),
            [(3, 50, 0), (5, 0, 31)],
        )
        self.assertEqual([r["id"] for r in rows["item_procs"]], [31])
        self.assertEqual([r["id"] for r in rows["skill_modifiers"]], [4000])
        self.assertEqual([r["id"] for r in rows["tagged_buffs"]], [5000])
        self.assertIn(900, [r["id"] for r in rows["skills"]])
        self.assertEqual(report["proc_chance_kinds"]["31"]["kind_used_in_1_2"], False)
        dropped = report["columns_dropped_from_3_0"]["item_procs"]
        self.assertIn("trigger_skill_id", dropped)

    def test_set_texts_skip_placeholders(self):
        base, ref = self.dbs()
        _, rows, report = build_backport(base, ref, [10])
        keys = {(r["tbl_name"], r["tbl_column_name"], r["idx"]): r["en_us"] for r in rows["localized_texts"]}
        self.assertEqual(keys[("equip_item_sets", "name", 300)], "Test Set EN")
        self.assertEqual(keys[("buffs", "desc", 50)], "Cuts cooldowns")
        self.assertNotIn(("buffs", "name", 50), keys)

    def test_existing_set_is_not_borrowed_again(self):
        base, ref = self.dbs()
        base.conn.execute("INSERT INTO equip_item_sets VALUES (300, 'old', '')")
        _, rows, report = build_backport(base, ref, [10])
        self.assertNotIn("equip_item_sets", rows)
        self.assertNotIn("equip_item_set_bonuses", rows)

    def test_missing_tag_is_reported(self):
        base, ref = self.dbs()
        base.conn.execute("DELETE FROM tags")
        _, _, report = build_backport(base, ref, [10])
        self.assertIn(
            {"table": "tags", "id": 565, "needed_by": "skill_modifiers:50"},
            report["refused"]["missing_references"],
        )

    def test_bonus_with_unknown_unit_attribute_is_omitted(self):
        base, ref = self.dbs()
        base.conn.execute("INSERT INTO unit_modifiers VALUES (1, 'Item', 1, 186, 0, 1, 0)")
        ref.conn.execute("INSERT INTO buffs VALUES (51, 'pve', 0, 0)")
        ref.conn.execute("INSERT INTO unit_modifiers VALUES ('Buff', 51, 199, 0, -70, 0)")
        ref.conn.execute("INSERT INTO equip_item_set_bonuses VALUES (3002, 51, 300, 4, 0)")
        _, rows, report = build_backport(base, ref, [10])
        self.assertEqual(
            sorted(r["num_pieces"] for r in rows["equip_item_set_bonuses"]), [3, 5]
        )
        self.assertEqual(report["set_bonuses_omitted"][0]["buff_id"], 51)
        self.assertNotIn(51, [r["id"] for r in rows["buffs"]])

    def test_orphan_craft_is_reported_not_borrowed(self):
        base, ref = self.dbs()
        ref.conn.execute("INSERT INTO craft_products VALUES (210, 99, 10, 1)")
        _, rows, report = build_backport(base, ref, [10])
        self.assertEqual(report["orphan_crafts_in_3_0"], [99])
        self.assertNotIn(99, [r["id"] for r in rows["crafts"]])

    def test_ranges_and_named_sets(self):
        self.assertEqual(resolve_item_set("5-7"), [5, 6, 7])
        self.assertEqual(resolve_item_set("crazed-sets"), list(range(38877, 38898)))
        self.assertEqual(resolve_item_set("9,crazed-sets")[0], 9)


class ClientNullNormalizationTest(unittest.TestCase):
    def test_zero_becomes_null_where_client_rows_are_null(self):
        with tempfile.TemporaryDirectory() as tmp:
            paths = {n: os.path.join(tmp, n + ".sqlite3") for n in ("base", "client", "patched")}
            for name, path in paths.items():
                conn = sqlite3.connect(path)
                conn.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, exp_date INTEGER, price INTEGER)")
                conn.executemany(
                    "INSERT INTO items VALUES (?,?,NULL,?)",
                    [(i, f"old{i}", 0) for i in range(1, 201)],
                )
                if name == "patched":
                    conn.execute("INSERT INTO items VALUES (9000, 'new', 0, 0)")
                conn.commit()
                conn.close()
            report = patch_client_db(paths["client"], paths["base"], paths["patched"])
            self.assertEqual(report["zero_to_null"], {"items.exp_date": 1})
            conn = sqlite3.connect(paths["client"])
            row = conn.execute("SELECT exp_date, price FROM items WHERE id = 9000").fetchone()
            conn.close()
            self.assertEqual(row, (None, 0))
            again = patch_client_db(paths["client"], paths["base"], paths["patched"])
            self.assertEqual(sum(again["inserted"].values()), 0)
            self.assertEqual(again["conflicts"], {})


if __name__ == "__main__":
    unittest.main()
