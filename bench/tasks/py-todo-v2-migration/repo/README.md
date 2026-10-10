# todo

A small command line to-do list. The list lives in one JSON file; every
command loads it, does one thing and, if something changed, saves it again.

```
$ python3 -m todo --file ~/lists/home.json add "Buy milk"
added 1
$ python3 -m todo --file ~/lists/home.json add "Call the bank"
added 2
$ python3 -m todo --file ~/lists/home.json done 2
done 2
$ python3 -m todo --file ~/lists/home.json list
[ ] 1 Buy milk
$ python3 -m todo --file ~/lists/home.json list --all
[ ] 1 Buy milk
[x] 2 Call the bank
```

`--file PATH` picks the list; without it `todo` uses `$TODO_FILE`, or
`~/.todo.json` when that is not set either. A file that does not exist yet
is an empty list, created by the first command that saves.

## Commands

| command       | what it does                                             | prints        |
|---------------|----------------------------------------------------------|---------------|
| `add TEXT`    | adds an open item; leading/trailing spaces are dropped   | `added ID`    |
| `list [--all]`| shows open items, oldest first; `--all` adds done ones  | one line each |
| `done ID`     | marks the item as done                                   | `done ID`     |
| `remove ID`   | deletes the item                                         | `removed ID`  |

A list line is `[ ]` (open) or `[x]` (done), the id and the text:
`[ ] 3 Buy milk`. When nothing is shown, `list` prints nothing.

Ids are positive integers; a new item gets the highest id in the list plus 1.

## Exit status

| status | meaning                                                          |
|--------|------------------------------------------------------------------|
| 0      | success                                                          |
| 1      | the id names no item (message on stderr, nothing is saved)       |
| 2      | usage error, or the file can't be read or isn't a valid list     |

## File format (version 1)

```json
{
  "version": 1,
  "items": [
    {"id": 1, "text": "Buy milk", "done": false},
    {"id": 2, "text": "Call the bank", "done": true}
  ]
}
```

Items are kept in the order they were added. Saves are atomic: the new list
is written to a temporary file in the same directory and renamed over the
old one.

## Layout

- `todo/models.py`: `Item` and `TodoList` (adding, finding, completing and
  removing items; `ItemNotFound` for unknown ids).
- `todo/store.py`: `load` and `save` for the JSON file; `StoreError`.
- `todo/render.py`: `format_item` and `render_list`, the text output.
- `todo/cli.py`: argument parsing and the commands; `python3 -m todo` runs
  `main()`.

## Running the tests

Python 3.11 or newer, standard library only. From the project root:

```
python3 -m unittest -q tests.test_models tests.test_store tests.test_render tests.test_cli
```
