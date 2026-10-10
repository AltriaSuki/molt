"""End-to-end tests of tags, due dates and the version 2 file format.

Every test runs ``python3 -m todo --file PATH ...`` in a subprocess against a
file in a fresh temporary directory.
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# A version 1 file as an older todo (or a person) might have left it: compact,
# keys in no particular order, no trailing newline. The backup must keep these
# exact bytes.
V1_BYTES = (
    b'{"items":[{"text":"Buy milk","id":1,"done":true},\n'
    b'  {"done":false,"id":3,"text":"Renew passport"}],\n'
    b' "version":1}'
)


class TodoCliCase(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = tmp.name
        self.path = os.path.join(self.dir, "todo.json")
        self.backup = self.path + ".v1.bak"

    def run_todo(self, *argv, today=None):
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": self.dir,
            "LANG": "C.UTF-8",
            "PYTHONDONTWRITEBYTECODE": "1",
        }
        if today is not None:
            env["TODO_TODAY"] = today
        proc = subprocess.run(
            [sys.executable, "-m", "todo", "--file", self.path, *argv],
            cwd=ROOT,
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )
        return proc.returncode, proc.stdout, proc.stderr

    def ok(self, *argv, today=None):
        code, out, err = self.run_todo(*argv, today=today)
        self.assertEqual(code, 0, f"todo {' '.join(argv)} failed: {err}")
        return out

    def write_bytes(self, data, path=None):
        with open(path or self.path, "wb") as fh:
            fh.write(data)

    def write_json(self, data):
        self.write_bytes(json.dumps(data).encode("utf-8"))

    def read_bytes(self, path=None):
        with open(path or self.path, "rb") as fh:
            return fh.read()

    def read_json(self):
        with open(self.path, encoding="utf-8") as fh:
            return json.load(fh)

    def files(self):
        return sorted(os.listdir(self.dir))

    def assertUsageError(self, *argv):
        code, out, _ = self.run_todo(*argv)
        self.assertEqual(code, 2, f"todo {' '.join(argv)} should be a usage error")
        self.assertEqual(out, "")


def item(id, text, done=False, tags=(), due=None):
    return {"id": id, "text": text, "done": done, "tags": list(tags), "due": due}


class VersionTwoFormatTests(TodoCliCase):
    def test_add_writes_a_version_2_file(self):
        out = self.ok("add", "Buy milk", "--tag", "Home", "--tag", "shop", "--tag", "HOME",
                      "--due", "2026-01-05")
        self.assertEqual(out, "added 1\n")
        self.assertEqual(self.read_json(), {
            "version": 2,
            "next_id": 2,
            "items": [item(1, "Buy milk", tags=["home", "shop"], due="2026-01-05")],
        })
        self.assertEqual(self.files(), ["todo.json"])

    def test_add_without_tags_or_due(self):
        self.assertEqual(self.ok("add", "  Call the bank "), "added 1\n")
        self.assertEqual(self.ok("add", "Pay rent"), "added 2\n")
        self.assertEqual(self.read_json(), {
            "version": 2,
            "next_id": 3,
            "items": [item(1, "Call the bank"), item(2, "Pay rent")],
        })

    def test_tags_are_stored_sorted(self):
        self.ok("add", "Plan trip", "--tag", "zeta", "--tag", "Alpha", "--tag", "m-1", "--tag", "b_2")
        self.assertEqual(self.read_json()["items"][0]["tags"], ["alpha", "b_2", "m-1", "zeta"])

    def test_list_line_format(self):
        self.ok("add", "Buy milk", "--tag", "shop", "--tag", "home", "--due", "2026-01-05")
        self.ok("add", "Call the bank")
        self.ok("add", "Pay rent", "--due", "2026-02-01")
        self.ok("add", "Water plants", "--tag", "Garden")
        self.ok("done", "2")
        self.assertEqual(self.ok("list", "--all"), (
            "[ ] 1 Buy milk #home #shop (due 2026-01-05)\n"
            "[x] 2 Call the bank\n"
            "[ ] 3 Pay rent (due 2026-02-01)\n"
            "[ ] 4 Water plants #garden\n"
        ))
        self.assertEqual(self.ok("list"), (
            "[ ] 1 Buy milk #home #shop (due 2026-01-05)\n"
            "[ ] 3 Pay rent (due 2026-02-01)\n"
            "[ ] 4 Water plants #garden\n"
        ))

    def test_ids_are_never_reused(self):
        for text in ("a", "b", "c"):
            self.ok("add", text)
        self.assertEqual(self.ok("remove", "3"), "removed 3\n")
        self.assertEqual(self.ok("add", "d"), "added 4\n")
        for n in ("1", "2", "4"):
            self.ok("remove", n)
        self.assertEqual(self.read_json(), {"version": 2, "next_id": 5, "items": []})
        self.assertEqual(self.ok("add", "e"), "added 5\n")
        self.assertEqual(self.read_json()["next_id"], 6)

    def test_next_id_is_read_from_a_version_2_file(self):
        self.write_json({
            "version": 2,
            "next_id": 10,
            "items": [item(1, "Buy milk"), item(2, "Pay rent", tags=["home"])],
        })
        self.assertEqual(self.ok("add", "Call the bank"), "added 10\n")
        data = self.read_json()
        self.assertEqual(data["next_id"], 11)
        self.assertEqual([entry["id"] for entry in data["items"]], [1, 2, 10])

    def test_version_2_fields_survive_other_commands(self):
        self.write_json({
            "version": 2,
            "next_id": 4,
            "items": [
                item(1, "Buy milk", tags=["home", "shop"], due="2026-01-05"),
                item(3, "Pay rent", due="2026-02-01"),
            ],
        })
        self.assertEqual(self.ok("done", "1"), "done 1\n")
        self.assertEqual(self.read_json(), {
            "version": 2,
            "next_id": 4,
            "items": [
                item(1, "Buy milk", done=True, tags=["home", "shop"], due="2026-01-05"),
                item(3, "Pay rent", due="2026-02-01"),
            ],
        })
        self.assertEqual(self.files(), ["todo.json"])

    def test_bad_next_id_is_not_a_valid_list(self):
        cases = {
            "missing": {"version": 2, "items": [item(1, "Buy milk")]},
            "equal to an id": {"version": 2, "next_id": 2, "items": [item(1, "a"), item(2, "b")]},
            "below an id": {"version": 2, "next_id": 3, "items": [item(5, "a")]},
            "not a number": {"version": 2, "next_id": "6", "items": [item(5, "a")]},
        }
        for name, data in cases.items():
            with self.subTest(name):
                self.write_json(data)
                before = self.read_bytes()
                code, out, err = self.run_todo("add", "Call the bank")
                self.assertEqual(code, 2)
                self.assertEqual(out, "")
                self.assertNotEqual(err.strip(), "")
                self.assertEqual(self.read_bytes(), before)


class TagTests(TodoCliCase):
    def test_tag_adds_to_existing_tags(self):
        self.ok("add", "Buy milk", "--tag", "shop")
        self.assertEqual(self.ok("tag", "1", "Home", "urgent", "SHOP"),
                         "[ ] 1 Buy milk #home #shop #urgent\n")
        self.assertEqual(self.read_json()["items"][0]["tags"], ["home", "shop", "urgent"])

    def test_tag_prints_done_items_with_due_date(self):
        self.ok("add", "Pay rent", "--due", "2026-02-01")
        self.ok("done", "1")
        self.assertEqual(self.ok("tag", "1", "money"), "[x] 1 Pay rent #money (due 2026-02-01)\n")

    def test_untag(self):
        self.ok("add", "Buy milk", "--tag", "home", "--tag", "shop", "--tag", "urgent")
        self.assertEqual(self.ok("untag", "1", "URGENT", "work"), "[ ] 1 Buy milk #home #shop\n")
        self.assertEqual(self.read_json()["items"][0]["tags"], ["home", "shop"])
        self.assertEqual(self.ok("untag", "1", "home", "shop"), "[ ] 1 Buy milk\n")
        self.assertEqual(self.read_json()["items"][0]["tags"], [])

    def test_untag_a_tag_the_item_does_not_have(self):
        self.ok("add", "Buy milk", "--tag", "home")
        self.assertEqual(self.ok("untag", "1", "work"), "[ ] 1 Buy milk #home\n")
        self.assertEqual(self.read_json()["items"][0]["tags"], ["home"])

    def test_unknown_ids(self):
        self.ok("add", "Buy milk", "--tag", "home")
        before = self.read_bytes()
        for argv in (("tag", "9", "work"), ("untag", "9", "home"), ("done", "9"), ("remove", "9")):
            with self.subTest(argv=argv):
                code, out, err = self.run_todo(*argv)
                self.assertEqual(code, 1)
                self.assertEqual(out, "")
                self.assertIn("9", err)
                self.assertEqual(self.read_bytes(), before)

    def test_invalid_tags_are_usage_errors(self):
        for bad in ("two words", "#home", "a,b", "", "home!", "café"):
            with self.subTest(tag=bad):
                if os.path.exists(self.path):
                    os.remove(self.path)
                self.assertUsageError("add", "Buy milk", "--tag", bad)
                self.assertFalse(os.path.exists(self.path))
        self.ok("add", "Buy milk", "--tag", "home")
        before = self.read_bytes()
        self.assertUsageError("tag", "1", "work", "bad tag")
        self.assertUsageError("untag", "1", "#home")
        self.assertUsageError("list", "--tag", "bad tag")
        self.assertEqual(self.read_bytes(), before)

    def test_list_filters_by_tag_case_insensitively(self):
        self.ok("add", "Buy milk", "--tag", "home", "--tag", "shop")
        self.ok("add", "Send report", "--tag", "work")
        self.ok("add", "Fix the tap", "--tag", "Home")
        self.ok("add", "Call the bank")
        self.ok("done", "3")
        self.assertEqual(self.ok("list", "--tag", "HOME"), "[ ] 1 Buy milk #home #shop\n")
        self.assertEqual(self.ok("list", "--tag", "home", "--all"),
                         "[ ] 1 Buy milk #home #shop\n[x] 3 Fix the tap #home\n")
        self.assertEqual(self.ok("list", "--tag", "Work"), "[ ] 2 Send report #work\n")
        self.assertEqual(self.ok("list", "--tag", "garden", "--all"), "")


class DueDateTests(TodoCliCase):
    def setUp(self):
        super().setUp()
        self.write_json({
            "version": 2,
            "next_id": 6,
            "items": [
                item(1, "Pay rent", tags=["home"], due="2026-01-04"),
                item(2, "Renew passport", due="2026-01-05"),
                item(3, "Book flights", tags=["home"], due="2026-01-06"),
                item(4, "Call the bank"),
                item(5, "File taxes", done=True, tags=["home"], due="2025-12-31"),
            ],
        })

    def test_overdue_means_due_before_today(self):
        self.assertEqual(self.ok("list", "--overdue", today="2026-01-05"),
                         "[ ] 1 Pay rent #home (due 2026-01-04)\n")

    def test_overdue_with_all_includes_done_items(self):
        self.assertEqual(self.ok("list", "--overdue", "--all", today="2026-01-05"), (
            "[ ] 1 Pay rent #home (due 2026-01-04)\n"
            "[x] 5 File taxes #home (due 2025-12-31)\n"
        ))

    def test_today_comes_from_todo_today(self):
        self.assertEqual(self.ok("list", "--overdue", today="2026-01-07"), (
            "[ ] 1 Pay rent #home (due 2026-01-04)\n"
            "[ ] 2 Renew passport (due 2026-01-05)\n"
            "[ ] 3 Book flights #home (due 2026-01-06)\n"
        ))
        self.assertEqual(self.ok("list", "--overdue", "--all", today="2025-12-31"), "")

    def test_overdue_combines_with_tag(self):
        self.assertEqual(self.ok("list", "--overdue", "--tag", "home", today="2026-01-07"), (
            "[ ] 1 Pay rent #home (due 2026-01-04)\n"
            "[ ] 3 Book flights #home (due 2026-01-06)\n"
        ))
        self.assertEqual(
            self.ok("list", "--tag", "HOME", "--overdue", "--all", today="2026-01-05"),
            "[ ] 1 Pay rent #home (due 2026-01-04)\n[x] 5 File taxes #home (due 2025-12-31)\n",
        )

    def test_list_without_overdue_ignores_due_dates(self):
        self.assertEqual(self.ok("list", today="2026-01-07"), (
            "[ ] 1 Pay rent #home (due 2026-01-04)\n"
            "[ ] 2 Renew passport (due 2026-01-05)\n"
            "[ ] 3 Book flights #home (due 2026-01-06)\n"
            "[ ] 4 Call the bank\n"
        ))

    def test_without_todo_today_the_real_date_is_used(self):
        self.ok("add", "Long ago", "--due", "2000-01-01")
        self.ok("add", "Far future", "--due", "2999-12-31")
        out = self.ok("list", "--overdue")
        self.assertIn("[ ] 6 Long ago (due 2000-01-01)\n", out)
        self.assertNotIn("Far future", out)
        self.assertNotIn("Call the bank", out)

    def test_invalid_due_dates_are_usage_errors(self):
        for bad in ("2026-02-30", "2026-1-5", "20260105", "2026-W02-1", "05/01/2026",
                    "2026-01-05T09:00", "tomorrow", "", "2026-13-01"):
            with self.subTest(due=bad):
                before = self.read_bytes()
                self.assertUsageError("add", "Water plants", "--due", bad)
                self.assertEqual(self.read_bytes(), before)

    def test_leap_day_is_a_valid_due_date(self):
        self.assertEqual(self.ok("add", "Leap", "--due", "2024-02-29"), "added 6\n")
        self.assertEqual(self.read_json()["items"][-1]["due"], "2024-02-29")


class MigrationTests(TodoCliCase):
    def setUp(self):
        super().setUp()
        self.write_bytes(V1_BYTES)

    def migrated(self, *items, next_id):
        return {"version": 2, "next_id": next_id, "items": list(items)}

    def test_list_reads_version_1_without_writing(self):
        self.assertEqual(self.ok("list", "--all"), "[x] 1 Buy milk\n[ ] 3 Renew passport\n")
        self.assertEqual(self.ok("list"), "[ ] 3 Renew passport\n")
        self.assertEqual(self.ok("list", "--overdue", "--all", today="2030-01-01"), "")
        self.assertEqual(self.ok("list", "--tag", "home"), "")
        self.assertEqual(self.read_bytes(), V1_BYTES)
        self.assertEqual(self.files(), ["todo.json"])

    def test_first_save_backs_up_and_writes_version_2(self):
        self.assertEqual(self.ok("done", "3"), "done 3\n")
        self.assertEqual(self.files(), ["todo.json", "todo.json.v1.bak"])
        self.assertEqual(self.read_bytes(self.backup), V1_BYTES)
        self.assertEqual(self.read_json(), self.migrated(
            item(1, "Buy milk", done=True),
            item(3, "Renew passport", done=True),
            next_id=4,
        ))

    def test_add_on_a_version_1_file(self):
        self.assertEqual(self.ok("add", "Pay rent", "--tag", "Home", "--due", "2026-02-01"),
                         "added 4\n")
        self.assertEqual(self.read_bytes(self.backup), V1_BYTES)
        self.assertEqual(self.read_json(), self.migrated(
            item(1, "Buy milk", done=True),
            item(3, "Renew passport"),
            item(4, "Pay rent", tags=["home"], due="2026-02-01"),
            next_id=5,
        ))

    def test_next_id_survives_removing_the_highest_id(self):
        self.assertEqual(self.ok("remove", "3"), "removed 3\n")
        self.assertEqual(self.read_json(), self.migrated(item(1, "Buy milk", done=True), next_id=4))
        self.assertEqual(self.ok("add", "Pay rent"), "added 4\n")
        self.assertEqual(self.read_bytes(self.backup), V1_BYTES)

    def test_tag_on_a_version_1_file_migrates_it(self):
        self.assertEqual(self.ok("tag", "1", "Errands"), "[x] 1 Buy milk #errands\n")
        self.assertEqual(self.read_bytes(self.backup), V1_BYTES)
        self.assertEqual(self.read_json(), self.migrated(
            item(1, "Buy milk", done=True, tags=["errands"]),
            item(3, "Renew passport"),
            next_id=4,
        ))

    def test_later_saves_keep_the_first_backup(self):
        self.ok("done", "3")
        self.ok("add", "Pay rent")
        self.ok("remove", "1")
        self.assertEqual(self.read_bytes(self.backup), V1_BYTES)
        self.assertEqual(self.files(), ["todo.json", "todo.json.v1.bak"])
        self.assertEqual(self.read_json(), self.migrated(
            item(3, "Renew passport", done=True),
            item(4, "Pay rent"),
            next_id=5,
        ))

    def test_an_existing_backup_is_left_alone(self):
        self.write_bytes(b"an older backup\n", self.backup)
        self.assertEqual(self.ok("add", "Pay rent"), "added 4\n")
        self.assertEqual(self.read_bytes(self.backup), b"an older backup\n")
        self.assertEqual(self.read_json()["version"], 2)
        self.assertEqual(self.read_json()["next_id"], 5)

    def test_failed_commands_write_nothing(self):
        for argv, status in (
            (("done", "7"), 1),
            (("remove", "7"), 1),
            (("tag", "7", "home"), 1),
            (("untag", "7", "home"), 1),
            (("add", "Pay rent", "--due", "2026-02-30"), 2),
            (("add", "Pay rent", "--tag", "bad tag"), 2),
        ):
            with self.subTest(argv=argv):
                self.write_bytes(V1_BYTES)
                if os.path.exists(self.backup):
                    os.remove(self.backup)
                code, out, _ = self.run_todo(*argv)
                self.assertEqual(code, status)
                self.assertEqual(out, "")
                self.assertEqual(self.read_bytes(), V1_BYTES)
                self.assertEqual(self.files(), ["todo.json"])

    def test_empty_version_1_file(self):
        original = b'{"version": 1, "items": []}\n'
        self.write_bytes(original)
        self.assertEqual(self.ok("list", "--all"), "")
        self.assertEqual(self.read_bytes(), original)
        self.assertEqual(self.ok("add", "Buy milk"), "added 1\n")
        self.assertEqual(self.read_bytes(self.backup), original)
        self.assertEqual(self.read_json(), self.migrated(item(1, "Buy milk"), next_id=2))


class UnsupportedVersionTests(TodoCliCase):
    def test_unknown_versions_exit_with_status_3(self):
        for version in (42, 3, 0):
            with self.subTest(version=version):
                original = json.dumps({
                    "version": version,
                    "next_id": 2,
                    "items": [item(1, "Buy milk")],
                }).encode("utf-8")
                self.write_bytes(original)
                for argv in (("list",), ("add", "Pay rent"), ("done", "1"), ("tag", "1", "home")):
                    code, out, err = self.run_todo(*argv)
                    self.assertEqual(code, 3, argv)
                    self.assertEqual(out, "")
                    self.assertIn(str(version), err)
                    self.assertEqual(self.read_bytes(), original)
                    self.assertEqual(self.files(), ["todo.json"])

    def test_broken_files_still_exit_with_status_2(self):
        self.write_bytes(b"{not json")
        code, out, err = self.run_todo("list")
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertNotEqual(err.strip(), "")

    def test_a_missing_or_non_integer_version_is_a_broken_file(self):
        cases = {
            "missing": {"items": [{"id": 1, "text": "Buy milk", "done": False}]},
            "not a number": {"version": "two", "items": [item(1, "Buy milk")]},
        }
        for name, data in cases.items():
            with self.subTest(name):
                original = json.dumps(data).encode("utf-8")
                self.write_bytes(original)
                code, out, err = self.run_todo("add", "Pay rent")
                self.assertEqual(code, 2)
                self.assertEqual(out, "")
                self.assertNotEqual(err.strip(), "")
                self.assertEqual(self.read_bytes(), original)
                self.assertEqual(self.files(), ["todo.json"])


if __name__ == "__main__":
    unittest.main()
