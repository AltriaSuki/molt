"""Sizes such as 10KiB."""

import re

_UNITS = {"B": 1, "KiB": 1024, "MiB": 1024**2, "GiB": 1024**3}
_SIZE = re.compile(r"(\d+)(B|KiB|MiB|GiB)")


def parse_size(text):
    """The number of bytes `text` names, such as 10240 for "10KiB"."""
    match = _SIZE.fullmatch(text)
    if not match:
        raise ValueError(f"bad size: {text!r}")
    return int(match.group(1)) * _UNITS[match.group(2)]
