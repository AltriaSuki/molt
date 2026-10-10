import json
import os
import tempfile
import unittest

from todo import store
from todo.models import TodoList


class StoreTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = os.path.join(self.tmp.name, "todo.json")

    def write(self, data):
        with open(self.path, "w", encoding="utf-8") as fh:
            if isinstance(data, str):
                fh.write(data)
            else:
                json.dump(data, fh)

    def test_missing_file_is_an_empty_list(self):
        todos = store.load(self.path)
        self.assertEqual(len(todos), 0)
        self.assertFalse(os.path.exists(self.path))

    def test_round_trip(self):
        todos = TodoList()
        todos.add("Buy milk")
        todos.add("Call the bank")
        todos.complete(1)
        store.save(self.path, todos)

        loaded = store.load(self.path)
        self.assertEqual(
            [(item.id, item.text, item.done) for item in loaded],
            [(1, "Buy milk", True), (2, "Call the bank", False)],
        )

    def test_save_overwrites_the_previous_list(self):
        todos = TodoList()
        todos.add("Buy milk")
        store.save(self.path, todos)
        todos.remove(1)
        store.save(self.path, todos)
        self.assertEqual(len(store.load(self.path)), 0)

    def test_loads_a_version_1_file(self):
        self.write({
            "version": 1,
            "items": [
                {"id": 1, "text": "Buy milk", "done": True},
                {"id": 3, "text": "Renew passport", "done": False},
            ],
        })
        todos = store.load(self.path)
        self.assertEqual(
            [(item.id, item.text, item.done) for item in todos],
            [(1, "Buy milk", True), (3, "Renew passport", False)],
        )

    def test_invalid_json(self):
        self.write("{not json")
        with self.assertRaises(store.StoreError):
            store.load(self.path)

    def test_top_level_must_be_an_object(self):
        self.write([1, 2, 3])
        with self.assertRaises(store.StoreError):
            store.load(self.path)

    def test_unknown_format_version(self):
        self.write({"version": 99, "items": []})
        with self.assertRaises(store.StoreError) as ctx:
            store.load(self.path)
        self.assertIn("99", str(ctx.exception))

    def test_items_must_be_a_list(self):
        self.write({"version": 1, "items": {"1": "Buy milk"}})
        with self.assertRaises(store.StoreError):
            store.load(self.path)

    def test_malformed_item(self):
        self.write({"version": 1, "items": [{"id": "x", "text": "Buy milk", "done": False}]})
        with self.assertRaises(store.StoreError):
            store.load(self.path)

    def test_duplicate_ids(self):
        self.write({
            "version": 1,
            "items": [
                {"id": 1, "text": "Buy milk", "done": False},
                {"id": 1, "text": "Call the bank", "done": False},
            ],
        })
        with self.assertRaises(store.StoreError):
            store.load(self.path)

    def test_save_leaves_no_temporary_files(self):
        todos = TodoList()
        todos.add("Buy milk")
        store.save(self.path, todos)
        store.save(self.path, todos)
        self.assertEqual(os.listdir(self.tmp.name), ["todo.json"])


if __name__ == "__main__":
    unittest.main()
