import unittest

from units import parse_size


class SizeTest(unittest.TestCase):
    def test_units(self):
        self.assertEqual(parse_size("10KiB"), 10240)
        self.assertEqual(parse_size("3B"), 3)

    def test_bad(self):
        with self.assertRaisesRegex(ValueError, r"^bad size: '10kb'$"):
            parse_size("10kb")


if __name__ == "__main__":
    unittest.main()
