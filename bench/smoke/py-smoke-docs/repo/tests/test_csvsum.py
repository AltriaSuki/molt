import unittest

from csvsum import total


class TotalTest(unittest.TestCase):
    def test_sums_a_column(self):
        rows = [["a", "1.5"], ["b", "2.25"]]
        self.assertEqual(str(total(rows, 2)), "3.75")


if __name__ == "__main__":
    unittest.main()
