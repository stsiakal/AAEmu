"""Tests for the Classic probe evidence importer.

Run from this folder:

    python -m unittest test_import

The fixtures under ``tests/probe_observations`` are synthetic and small. They
exercise the parser subset, every evidence kind, reward/turn-in completion
inference, duplicate reward additions, determinism, malformed input and the
optional 1.2 database annotations.
"""

from __future__ import annotations

import contextlib
import io
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import import_observations  # noqa: E402

HERE = Path(__file__).resolve().parent
FIXTURES = HERE / "tests" / "probe_observations"


def evidence_records(evidence, kind):
    return [record for record in evidence["records"] if record["kind"] == kind]


def item_record(evidence, item_id):
    for record in evidence_records(evidence, "item_source"):
        if record["item_id"] == item_id:
            return record
    raise AssertionError(f"item {item_id} missing")


class ParserTests(unittest.TestCase):
    def test_data_only_subset_and_edges(self):
        text = r'''
        -- a comment before the table
        return {
            ["a.b"] = "quote:\" slash:\\ tab:\t dec:\065 hex:\x41",
            bare = 'single \' quote',
            nums = { 1, -2, 3.5, .5, 1e3, 0x10 },
            bools = { true, false },
            nested = { key = { 1, 2, [4] = 4 } },
            nil_value = nil,
            trailing = { 1, 2, },
            ["1"] = "string key",
        }
        '''
        table = import_observations.parse_lua_table(text, "edge.lua")
        self.assertEqual(
            table["a.b"], 'quote:" slash:\\ tab:\t dec:A hex:A'
        )
        self.assertEqual(table["bare"], "single ' quote")
        self.assertEqual(
            import_observations.as_array(table["nums"], "nums"),
            [1, -2, 3.5, 0.5, 1000.0, 16],
        )
        self.assertEqual(
            import_observations.as_array(table["bools"], "bools"),
            [True, False],
        )
        nested = table["nested"]["key"]
        self.assertEqual(nested[1], 1)
        self.assertEqual(nested[2], 2)
        self.assertEqual(nested[4], 4)
        self.assertNotIn("nil_value", table)
        self.assertEqual(
            import_observations.as_array(table["trailing"], "trailing"),
            [1, 2],
        )
        self.assertEqual(table["1"], "string key")

    def test_rejects_lua_code_and_malformed_data(self):
        cases = [
            ("{ a = function() end }", "unsupported identifier"),
            ("{ a = 1 + 2 }", "expected ',' or '}'"),
            ("{ a = 1, a = 2 }", "duplicate table key"),
            ('{ a = "unterminated }', "unterminated string"),
            ("{ a = . }", "expected a number"),
        ]
        for text, message in cases:
            with self.subTest(text=text):
                with self.assertRaises(import_observations.LuaParseError) as raised:
                    import_observations.parse_lua_table(text, "bad.lua")
                self.assertIn("bad.lua", str(raised.exception))
                self.assertIn("offset", str(raised.exception))
                self.assertIn(message, str(raised.exception))


class ImporterTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.output = self.root / "evidence.json"
        self.records, self.summary = import_observations.run_import(
            [FIXTURES / "basic"], self.output
        )
        self.evidence = json.loads(self.output.read_text(encoding="utf-8"))

    def tearDown(self):
        self.temp.cleanup()

    def test_every_record_kind_and_provenance(self):
        counts = self.summary["evidence_counts"]
        self.assertEqual(counts, {
            "item_source": 4,
            "kill": 1,
            "npc_position": 1,
            "quest": 2,
            "reward": 1,
        })
        for record in self.evidence["records"]:
            for key in ("source", "probe_version", "session", "file", "seq", "lt"):
                self.assertIn(key, record, f"{record['kind']} lacks {key}")
            self.assertEqual(
                record["source"], "observed in ArcheAge Classic 3.5"
            )
        self.assertEqual(self.summary["files"]["probe"], 2)
        self.assertEqual(self.summary["files"]["boot"], 1)

    def test_quests_and_completion_evidence(self):
        quests = {
            record["quest_id"]: record
            for record in evidence_records(self.evidence, "quest")
        }
        self.assertEqual(sorted(quests), [100, 101])
        quest = quests[100]
        self.assertEqual(quest["title"], "Test Quest")
        self.assertTrue(quest["completed"])
        self.assertTrue(quest["completion_inferred"])
        self.assertEqual(
            [item["kind"] for item in quest["completion_evidence"]],
            ["turn_in", "quest_reward"],
        )
        self.assertEqual(quest["accepted"]["evidence"], "quest_state")
        self.assertEqual(
            quests[101]["completion_evidence"][0]["kind"], "quest_state"
        )
        self.assertFalse(quests[101]["completion_inferred"])

    def test_reward_and_item_sources(self):
        rewards = evidence_records(self.evidence, "reward")
        self.assertEqual(len(rewards), 1)
        self.assertEqual(rewards[0]["quest_id"], 100)
        self.assertEqual(rewards[0]["items"], [{"id": 502, "count": 2}])
        self.assertEqual(rewards[0]["money"], 10)
        self.assertEqual(rewards[0]["xp"], 20)

        supply = item_record(self.evidence, 500)
        self.assertEqual(supply["name"], "Test Supply")
        self.assertEqual(supply["how"], ["quest_supply"])
        self.assertEqual(supply["sources"][0]["quest_id"], 100)
        self.assertEqual(supply["sources"][0]["count"], 1)

        reward = item_record(self.evidence, 502)
        self.assertEqual(reward["how"], ["quest_reward"])
        self.assertEqual(reward["sources"][0]["count"], 2)
        self.assertEqual(len(reward["sources"][0]["observations"]), 1)

        loot = item_record(self.evidence, 501)
        self.assertEqual(loot["how"], ["loot"])
        self.assertEqual(loot["sources"][0]["unit"]["name"], "Test Boar")

        other = item_record(self.evidence, 503)
        self.assertEqual(other["how"], ["other"])
        self.assertEqual(other["sources"][0]["probe_source"], 41)

    def test_npc_positions_and_kills(self):
        npcs = evidence_records(self.evidence, "npc_position")
        self.assertEqual(len(npcs), 1)
        npc = npcs[0]
        self.assertEqual(npc["unit"]["name"], "Test Boar")
        self.assertEqual(npc["unit"]["unit_ids"], ["boar01", "boar02"])
        self.assertEqual(len(npc["positions"]), 2)
        self.assertTrue(all(item["zone"] == 5 for item in npc["positions"]))

        kills = evidence_records(self.evidence, "kill")
        self.assertEqual(len(kills), 1)
        kill = kills[0]
        self.assertEqual(kill["unit"]["name"], "Test Boar")
        self.assertEqual(kill["count"], 2)
        self.assertEqual(len(kill["positions"]), 2)
        second = [
            item for item in kill["kills"] if item.get("ms") == 6100
        ][0]
        self.assertTrue(second["lt_inherited"])
        self.assertEqual(second["lt"], "106")

    def test_deterministic_output(self):
        second = self.root / "evidence-2.json"
        import_observations.run_import([FIXTURES / "basic"], second)
        self.assertEqual(self.output.read_bytes(), second.read_bytes())
        kinds = [record["kind"] for record in self.evidence["records"]]
        self.assertEqual(
            kinds,
            sorted(
                kinds,
                key=lambda kind: [
                    "quest",
                    "reward",
                    "item_source",
                    "npc_position",
                    "kill",
                ].index(kind),
            ),
        )

    def test_malformed_recording_names_file_and_offset(self):
        output = self.root / "malformed.json"
        with self.assertRaises(import_observations.LuaParseError) as raised:
            import_observations.run_import(
                [FIXTURES / "malformed"], output
            )
        message = str(raised.exception)
        self.assertIn("probe_0001_001.lua", message)
        self.assertIn("offset", message)

    def test_structural_error_names_file(self):
        broken = self.root / "broken"
        broken.mkdir()
        (broken / "probe_0009_001.lua").write_text(
            '{ probe_version = "0.2.0", session = "0009", records = { '
            '{ k = "meta" } } }',
            encoding="utf-8",
        )
        with self.assertRaises(import_observations.EvidenceError) as raised:
            import_observations.run_import([broken], self.root / "out.json")
        self.assertIn("probe_0009_001.lua", str(raised.exception))

    def test_summary_on_stdout(self):
        output = self.root / "stdout.json"
        capture = io.StringIO()
        with contextlib.redirect_stdout(capture):
            code = import_observations.main(
                ["--input", str(FIXTURES / "basic"), "--output", str(output)]
            )
        self.assertEqual(code, 0)
        summary = json.loads(capture.getvalue())
        self.assertEqual(summary["evidence_counts"]["quest"], 2)
        self.assertEqual(
            [quest["quest_id"] for quest in summary["quests"]], [100, 101]
        )
        self.assertEqual(summary["completions"][0]["quest_id"], 100)

    def test_database_annotations(self):
        db = self.root / "compact.sqlite3"
        conn = sqlite3.connect(db)
        conn.executescript(
            """
            CREATE TABLE quest_contexts (
                id INTEGER PRIMARY KEY, name TEXT, LEVEL INTEGER);
            CREATE TABLE quest_components (
                id INTEGER PRIMARY KEY, quest_context_id INTEGER,
                component_kind_id INTEGER);
            CREATE TABLE quest_acts (
                id INTEGER PRIMARY KEY, quest_component_id INTEGER,
                act_detail_id INTEGER, act_detail_type TEXT);
            CREATE TABLE quest_act_supply_items (
                id INTEGER PRIMARY KEY, item_id INTEGER, count INTEGER);
            INSERT INTO quest_contexts VALUES (100, 'Test Quest 1.2', 2);
            INSERT INTO quest_components VALUES (1, 100, 3);
            INSERT INTO quest_components VALUES (2, 100, 8);
            INSERT INTO quest_acts VALUES
                (10, 1, 1000, 'QuestActSupplyItem');
            INSERT INTO quest_acts VALUES
                (11, 2, 1001, 'QuestActSupplyItem');
            INSERT INTO quest_act_supply_items VALUES (1000, 500, 1);
            INSERT INTO quest_act_supply_items VALUES (1001, 502, 1);
            """
        )
        conn.close()

        output = self.root / "annotated.json"
        evidence, summary = import_observations.run_import(
            [FIXTURES / "basic"], output, db
        )
        quests = {
            record["quest_id"]: record
            for record in evidence_records(evidence, "quest")
        }
        self.assertEqual(quests[100]["expectation_1_2"]["title"], "Test Quest 1.2")
        self.assertEqual(quests[100]["match"], "mismatch")
        self.assertIn("count 2 != 1.2 count 1", quests[100]["match_notes"][0])
        self.assertEqual(quests[101]["match"], "mismatch")

        supply = item_record(evidence, 500)
        self.assertEqual(supply["match"], "match")
        reward = item_record(evidence, 502)
        self.assertEqual(reward["match"], "mismatch")
        self.assertEqual(summary["db"]["match_counts"]["mismatch"], 4)


if __name__ == "__main__":
    unittest.main()
