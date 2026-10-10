"""Command line interface: ``python3 -m todo [--file PATH] COMMAND ...``."""

from __future__ import annotations

import argparse
import os
import sys
from datetime import date
from typing import Callable, Sequence

from . import store
from .models import ItemNotFound, TodoList, normalize_tag, parse_date
from .render import format_item, render_list

DEFAULT_FILE = os.path.join("~", ".todo.json")

# A command handler gets the loaded list and the parsed arguments, and returns
# (changed, output): whether the list must be saved, and what to print.
Handler = Callable[[TodoList, argparse.Namespace], tuple[bool, str]]


def _cmd_add(todos: TodoList, args: argparse.Namespace) -> tuple[bool, str]:
    item = todos.add(args.text, tags=args.tags, due=args.due)
    return True, f"added {item.id}\n"


def _cmd_list(todos: TodoList, args: argparse.Namespace) -> tuple[bool, str]:
    items = todos.select(include_done=args.all, tag=args.tag, overdue_on=args.today)
    return False, render_list(items)


def _cmd_done(todos: TodoList, args: argparse.Namespace) -> tuple[bool, str]:
    item = todos.complete(args.id)
    return True, f"done {item.id}\n"


def _cmd_remove(todos: TodoList, args: argparse.Namespace) -> tuple[bool, str]:
    item = todos.remove(args.id)
    return True, f"removed {item.id}\n"


def _cmd_tag(todos: TodoList, args: argparse.Namespace) -> tuple[bool, str]:
    item = todos.tag(args.id, args.tags)
    return True, format_item(item) + "\n"


def _cmd_untag(todos: TodoList, args: argparse.Namespace) -> tuple[bool, str]:
    item = todos.untag(args.id, args.tags)
    return True, format_item(item) + "\n"


COMMANDS: dict[str, Handler] = {
    "add": _cmd_add,
    "list": _cmd_list,
    "done": _cmd_done,
    "remove": _cmd_remove,
    "tag": _cmd_tag,
    "untag": _cmd_untag,
}


def _tag_arg(value: str) -> str:
    try:
        return normalize_tag(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(str(exc)) from None


def _date_arg(value: str) -> date:
    try:
        return parse_date(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(str(exc)) from None


def today() -> date:
    """$TODO_TODAY (YYYY-MM-DD) when set, else the local date."""
    override = os.environ.get("TODO_TODAY")
    if override:
        return parse_date(override)
    return date.today()


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="todo", description="A small to-do list.")
    parser.add_argument(
        "--file",
        metavar="PATH",
        default=os.environ.get("TODO_FILE", DEFAULT_FILE),
        help="the list to use (default: $TODO_FILE, else ~/.todo.json)",
    )
    sub = parser.add_subparsers(dest="command", required=True, metavar="COMMAND")

    add = sub.add_parser("add", help="add an item")
    add.add_argument("text", metavar="TEXT")
    add.add_argument(
        "--tag", dest="tags", action="append", type=_tag_arg, default=[], metavar="T",
        help="tag the item (repeatable)",
    )
    add.add_argument("--due", type=_date_arg, metavar="DATE", help="due date, YYYY-MM-DD")

    lst = sub.add_parser("list", help="show open items")
    lst.add_argument("--all", action="store_true", help="include finished items")
    lst.add_argument("--tag", type=_tag_arg, metavar="T", help="only items with this tag")
    lst.add_argument(
        "--overdue", action="store_true", help="only items due before today ($TODO_TODAY)"
    )

    done = sub.add_parser("done", help="mark an item as finished")
    done.add_argument("id", type=int, metavar="ID")

    tag = sub.add_parser("tag", help="add tags to an item")
    tag.add_argument("id", type=int, metavar="ID")
    tag.add_argument("tags", nargs="+", type=_tag_arg, metavar="T")

    untag = sub.add_parser("untag", help="remove tags from an item")
    untag.add_argument("id", type=int, metavar="ID")
    untag.add_argument("tags", nargs="+", type=_tag_arg, metavar="T")

    remove = sub.add_parser("remove", help="delete an item")
    remove.add_argument("id", type=int, metavar="ID")
    return parser


def _error(message: str) -> None:
    print(f"todo: {message}", file=sys.stderr)


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.command == "add" and not args.text.strip():
        parser.error("text must not be empty")
    if args.command == "list":
        args.today = None
        if args.overdue:
            try:
                args.today = today()
            except ValueError as exc:
                parser.error(f"TODO_TODAY: {exc}")
    path = os.path.expanduser(args.file)

    try:
        todos = store.load(path)
        changed, output = COMMANDS[args.command](todos, args)
        if changed:
            store.save(path, todos)
    except ItemNotFound as exc:
        _error(str(exc))
        return 1
    except store.UnsupportedVersion as exc:
        _error(str(exc))
        return 3
    except store.StoreError as exc:
        _error(str(exc))
        return 2
    except OSError as exc:
        _error(f"cannot write {path}: {exc.strerror}")
        return 2

    sys.stdout.write(output)
    return 0
