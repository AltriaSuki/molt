import re

_TEMPERATURE = re.compile(r"(-?\d+(?:\.\d+)?)([CF])")


def to_celsius(fahrenheit):
    """Degrees Fahrenheit in degrees Celsius."""
    return (fahrenheit - 32) * 5 / 9


def parse_temperature(text):
    """A temperature such as "21.5C" or "70F", in degrees Celsius."""
    match = _TEMPERATURE.fullmatch(text)
    if not match:
        raise ValueError(f"bad temperature: {text!r}")
    value = float(match.group(1))
    return value if match.group(2) == "C" else to_celsius(value)
