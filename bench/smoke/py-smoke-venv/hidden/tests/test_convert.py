import pytest

from temps import to_celsius


def test_freezing_and_boiling():
    assert to_celsius(32) == 0
    assert to_celsius(212) == pytest.approx(100)
