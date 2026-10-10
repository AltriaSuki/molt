"""Command line interface: ``python3 -m todo [--file PATH] COMMAND ...``."""

from __future__ import annotations

import argparse
import os
import sys
from typing import Callable, Sequence

from . import store
from .models import ItemNotFound, TodoList
from .render import render_list

DEFAULT_FILE = os.path.join("~", ".todo.json")

# A command handler gets the loaded list and the parsed arguments, and returns
# (changed, output): whether the list must be saved, and what to print.
Handler = Callable[[TodoList, argparse.Namespace], tuple[bool, str]]


def _cmd_add(todos: TodoList, args: argparse.Namespace) -> tuple[bool, str]:
    item = todos.add(args.text)
    return True, f"added {item.id}\n"


def _cmd_list(todos: TodoList, args: argparse.Namespace) -> tuple[bool, str]:
    items = list(todos) if args.all else todos.open_items()
    return False, render_list(items)


def _cmd_done(todos: TodoList, args: argparse.Namespace) -> tuple[bool, str]:
    item = todos.complete(args.id)
    return True, f"done {item.id}\n"


def _cmd_remove(todos: TodoList, args: argparse.Namespace) -> tuple[bool, str]:
    item = todos.remove(args.id)
    return True, f"removed {item.id}\n"


COMMANDS: dict[str, Handler] = {
    "add": _cmd_add,
    "list": _cmd_list,
    "done": _cmd_done,
    "remove": _cmd_remove,
}


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

    lst = sub.add_parser("list", help="show open items")
    lst.add_argument("--all", action="store_true", help="include finished items")

    done = sub.add_parser("done", help="mark an item as finished")
    done.add_argument("id", type=int, metavar="ID")

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
    path = os.path.expanduser(args.file)

    try:
        todos = store.load(path)
        changed, output = COMMANDS[args.command](todos, args)
        if changed:
            store.save(path, todos)
    except ItemNotFound as exc:
        _error(str(exc))
        return 1
    except store.StoreError as exc:
        _error(str(exc))
        return 2
    except OSError as exc:
        _error(f"cannot write {path}: {exc.strerror}")
        return 2

    sys.stdout.write(output)
    return 0
