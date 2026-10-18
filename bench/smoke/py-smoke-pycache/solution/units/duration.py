"""Durations such as 1h30m."""

import re

_DURATION = re.compile(r"(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?")


def parse_duration(text):
    """The number of seconds `text` names, such as 5400 for "1h30m"."""
    match = _DURATION.fullmatch(text)
    if not text or not match:
        raise ValueError(f"bad duration: {text!r}")
    hours, minutes, seconds = (int(g or 0) for g in match.groups())
    return hours * 3600 + minutes * 60 + seconds
