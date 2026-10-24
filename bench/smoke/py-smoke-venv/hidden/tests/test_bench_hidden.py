import pytest

from temps import parse_temperature
from temps.convert import parse_temperature as direct


def test_both_units():
    assert parse_temperature is direct
    assert parse_temperature("21.5C") == pytest.approx(21.5)
    assert parse_temperature("212F") == pytest.approx(100.0)
    assert parse_temperature("-40F") == pytest.approx(-40.0)
    assert parse_temperature("-3C") == pytest.approx(-3.0)
    assert parse_temperature("0.5F") == pytest.approx(-17.5)
    assert isinstance(parse_temperature("10C"), float)


@pytest.mark.parametrize("text", ["", "21K", "21", "C", "21 C", "21c", "--1C", "1.C", "+5C", "5CF"])
def test_bad(text):
    with pytest.raises(ValueError) as caught:
        parse_temperature(text)
    assert str(caught.value) == f"bad temperature: {text!r}"
