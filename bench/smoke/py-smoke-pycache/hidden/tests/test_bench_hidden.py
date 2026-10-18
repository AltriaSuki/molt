import unittest

from units import parse_duration
from units.duration import parse_duration as direct


class DurationTest(unittest.TestCase):
    def test_parts(self):
        self.assertIs(parse_duration, direct)
        self.assertEqual(parse_duration("1h30m"), 5400)
        self.assertEqual(parse_duration("45s"), 45)
        self.assertEqual(parse_duration("2h"), 7200)
        self.assertEqual(parse_duration("90s"), 90)
        self.assertEqual(parse_duration("1h2m3s"), 3723)
        self.assertEqual(parse_duration("0s"), 0)

    def test_bad(self):
        for text in ["", "1x", "30m1h", "1h1h", "1 h", "h", "1.5h", "-1s", "1h30"]:
            with self.subTest(text=text):
                with self.assertRaises(ValueError) as caught:
                    parse_duration(text)
                self.assertEqual(str(caught.exception), f"bad duration: {text!r}")


if __name__ == "__main__":
    unittest.main()
