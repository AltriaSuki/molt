"""Plain-text rendering of a summary."""

from __future__ import annotations

from .summary import Summary


def _duration_lines(summary: Summary) -> list[str]:
    avg = summary.avg_duration_ms
    if avg is None:
        return ["avg duration: n/a", "max duration: n/a"]
    return [
        f"avg duration: {avg:.1f} ms",
        f"max duration: {summary.max_duration_ms} ms",
    ]


def render_text(summary: Summary) -> str:
    """The report printed by ``python3 -m logsum``, ending with a newline."""
    lines = [
        f"requests: {summary.requests}",
        f"bytes: {summary.bytes}",
        f"skipped: {summary.skipped}",
    ]
    lines += [f"{cls}: {count}" for cls, count in summary.status_counts().items()]
    lines += _duration_lines(summary)

    top = summary.top_paths()
    if top:
        width = len(str(top[0][1]))
        lines.append("top paths:")
        lines += [f"  {count:>{width}}  {path}" for path, count in top]
    else:
        lines.append("top paths: none")
    return "\n".join(lines) + "\n"
