# todo

A small command line to-do list. The list lives in one JSON file; every
command loads it, does one thing and, if something changed, saves it again.

```
$ python3 -m todo --file ~/lists/home.json add "Buy milk" --tag home --due 2026-01-05
added 1
$ python3 -m todo --file ~/lists/home.json add "Call the bank"
added 2
$ python3 -m todo --file ~/lists/home.json tag 2 Errands
[ ] 2 Call the bank #errands
$ python3 -m todo --file ~/lists/home.json done 2
done 2
$ python3 -m todo --file ~/lists/home.json list
[ ] 1 Buy milk #home (due 2026-01-05)
$ python3 -m todo --file ~/lists/home.json list --all
[ ] 1 Buy milk #home (due 2026-01-05)
[x] 2 Call the bank #errands
$ TODO_TODAY=2026-01-06 python3 -m todo --file ~/lists/home.json list --overdue
[ ] 1 Buy milk #home (due 2026-01-05)
```

`--file PATH` picks the list; without it `todo` uses `$TODO_FILE`, or
`~/.todo.json` when that is not set either. A file that does not exist yet
is an empty list, created by the first command that saves.

## Commands

| command                             | what it does                                         | prints         |
|-------------------------------------|------------------------------------------------------|----------------|
| `add TEXT [--tag T]... [--due DATE]`| adds an open item; leading/trailing spaces dropped   | `added ID`     |
| `list [--tag T] [--overdue] [--all]`| shows open items, oldest first (filters below)       | one line each  |
| `done ID`                           | marks the item as done                               | `done ID`      |
| `tag ID T...`                       | adds tags to the item                                | the item line  |
| `untag ID T...`                     | removes tags from the item (missing ones are fine)   | the item line  |
| `remove ID`                         | deletes the item                                     | `removed ID`   |

`list` shows open items; `--all` adds done ones, `--tag T` keeps items with
that tag, and `--overdue` keeps items due strictly before today. The filters
combine. "Today" is `$TODO_TODAY` (`YYYY-MM-DD`) when set, else the local date.

A tag is one or more ASCII letters, digits, `-` or `_`; tags are stored in
lowercase, so `Home` and `home` are the same tag. A due date is written
`YYYY-MM-DD`.

A list line is `[ ]` (open) or `[x]` (done), the id, the text, the tags in
sorted order as `#tag`, and the due date as `(due YYYY-MM-DD)`:
`[ ] 3 Buy milk #home #shop (due 2026-01-05)`. When nothing is shown, `list`
prints nothing.

Ids are positive integers and are never reused: a new item gets the list's
`next_id`, which only ever grows.

## Exit status

| status | meaning                                                          |
|--------|------------------------------------------------------------------|
| 0      | success                                                          |
| 1      | the id names no item (message on stderr, nothing is saved)       |
| 2      | usage error, or the file can't be read or isn't a valid list     |
| 3      | the file is in a format version this `todo` does not know        |

## File format (version 2)

```json
{
  "version": 2,
  "next_id": 3,
  "items": [
    {"id": 1, "text": "Buy milk", "done": false, "tags": ["home"], "due": "2026-01-05"},
    {"id": 2, "text": "Call the bank", "done": true, "tags": [], "due": null}
  ]
}
```

Items are kept in the order they were added. Saves are atomic: the new list
is written to a temporary file in the same directory and renamed over the
old one.

### Version 1 files

Version 1 (`{"version": 1, "items": [{"id", "text", "done"}]}`, written by
todo 1.x) is still read: its items get no tags and no due date, and `next_id`
is the highest id plus 1. Reading never changes the file. The first save
copies the version 1 file byte for byte to `PATH.v1.bak` (unless something
is already there) and then writes version 2.

## Layout

- `todo/models.py`: `Item` and `TodoList` (adding, finding, completing,
  tagging, filtering and removing items; `ItemNotFound` for unknown ids),
  plus `normalize_tag` and `parse_date`.
- `todo/store.py`: `load` and `save` for the JSON file, the version 1
  migration and backup; `StoreError` and `UnsupportedVersion`.
- `todo/render.py`: `format_item` and `render_list`, the text output.
- `todo/cli.py`: argument parsing and the commands; `python3 -m todo` runs
  `main()`.

## Running the tests

Python 3.11 or newer, standard library only. From the project root:

```
python3 -m unittest -q tests.test_models tests.test_store tests.test_render tests.test_cli
```
