import unittest

from todo.models import Item
from todo.render import format_item, render_list


class RenderTests(unittest.TestCase):
    def test_open_item(self):
        self.assertEqual(format_item(Item(3, "Buy milk")), "[ ] 3 Buy milk")

    def test_done_item(self):
        self.assertEqual(format_item(Item(12, "Call the bank", done=True)), "[x] 12 Call the bank")

    def test_render_list(self):
        items = [Item(1, "Buy milk"), Item(2, "Call the bank", done=True)]
        self.assertEqual(render_list(items), "[ ] 1 Buy milk\n[x] 2 Call the bank\n")

    def test_render_empty_list(self):
        self.assertEqual(render_list([]), "")


if __name__ == "__main__":
    unittest.main()
