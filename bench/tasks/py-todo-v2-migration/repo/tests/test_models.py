import unittest

from todo.models import Item, ItemNotFound, TodoList


class TodoListTests(unittest.TestCase):
    def test_add_assigns_increasing_ids(self):
        todos = TodoList()
        first = todos.add("Buy milk")
        second = todos.add("Call the bank")
        self.assertEqual((first.id, second.id), (1, 2))
        self.assertEqual([item.text for item in todos], ["Buy milk", "Call the bank"])
        self.assertFalse(first.done)

    def test_add_strips_text(self):
        todos = TodoList()
        self.assertEqual(todos.add("  Water plants \n").text, "Water plants")

    def test_add_rejects_empty_text(self):
        todos = TodoList()
        with self.assertRaises(ValueError):
            todos.add("   ")
        self.assertEqual(len(todos), 0)

    def test_get_unknown_id(self):
        todos = TodoList([Item(1, "Buy milk")])
        with self.assertRaises(ItemNotFound) as ctx:
            todos.get(7)
        self.assertEqual(ctx.exception.item_id, 7)
        self.assertIn("7", str(ctx.exception))

    def test_complete(self):
        todos = TodoList([Item(1, "Buy milk"), Item(2, "Call the bank")])
        todos.complete(2)
        self.assertEqual([item.done for item in todos], [False, True])

    def test_complete_unknown_id(self):
        todos = TodoList([Item(1, "Buy milk")])
        with self.assertRaises(ItemNotFound):
            todos.complete(2)

    def test_remove(self):
        todos = TodoList([Item(1, "Buy milk"), Item(2, "Call the bank")])
        removed = todos.remove(1)
        self.assertEqual(removed.text, "Buy milk")
        self.assertEqual([item.id for item in todos], [2])
        with self.assertRaises(ItemNotFound):
            todos.remove(1)

    def test_new_ids_follow_the_highest_existing_id(self):
        todos = TodoList([Item(2, "Call the bank"), Item(5, "Renew passport")])
        self.assertEqual(todos.add("Buy milk").id, 6)

    def test_open_items(self):
        todos = TodoList([Item(1, "a", done=True), Item(2, "b"), Item(3, "c")])
        self.assertEqual([item.id for item in todos.open_items()], [2, 3])

    def test_duplicate_ids_are_rejected(self):
        with self.assertRaises(ValueError):
            TodoList([Item(1, "a"), Item(1, "b")])


class ItemTests(unittest.TestCase):
    def test_dict_round_trip(self):
        item = Item(3, "Buy milk", done=True)
        self.assertEqual(Item.from_dict(item.to_dict()), item)

    def test_from_dict_reads_stored_fields(self):
        item = Item.from_dict({"id": 4, "text": "Call the bank", "done": False})
        self.assertEqual((item.id, item.text, item.done), (4, "Call the bank", False))

    def test_from_dict_rejects_malformed_items(self):
        bad = [
            {"id": "1", "text": "a", "done": False},
            {"id": 0, "text": "a", "done": False},
            {"id": True, "text": "a", "done": False},
            {"id": 1, "text": "", "done": False},
            {"id": 1, "text": 5, "done": False},
            {"id": 1, "text": "a", "done": "yes"},
            ["not", "an", "object"],
        ]
        for data in bad:
            with self.subTest(data=data):
                with self.assertRaises(ValueError):
                    Item.from_dict(data)


if __name__ == "__main__":
    unittest.main()
