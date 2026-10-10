import itertools
import os
import signal
import unittest

from pkgresolve import PackageIndex, Requirement, ResolutionError, Version, resolve
from pkgresolve import resolver as resolver_module

MIRROR = os.path.join(os.path.dirname(__file__), "data", "mirror.json")


class _OutOfTime(BaseException):
    pass


def _on_alarm(signum, frame):
    raise _OutOfTime()


def lock(**versions):
    """lock(kiwi="1.2", ...) -> {"kiwi": Version(1, 2, 0), ...}"""
    return {name.replace("_", "-"): Version.parse(text) for name, text in versions.items()}


def problems(index, requirements, resolution):
    """Why `resolution` is not a valid resolution of `requirements` ([] if it is)."""
    found = []
    if not isinstance(resolution, dict):
        return [f"expected a dict, got {type(resolution).__name__}"]
    for name, version in resolution.items():
        if not isinstance(version, Version):
            found.append(f"{name}: {version!r} is not a Version")
        elif not index.has_version(name, version):
            found.append(f"{name} {version} is not in the index")
    if found:
        return found
    queue = [(Requirement.parse(text), "root") for text in requirements]
    reached = set()
    while queue:
        req, who = queue.pop(0)
        if req.name not in resolution:
            found.append(f"{who} requires {req}, but {req.name} is missing")
            continue
        version = resolution[req.name]
        if not req.allows(version):
            found.append(f"{who} requires {req}, but {req.name} {version} was chosen")
        if req.name not in reached:
            reached.add(req.name)
            queue.extend((dep, f"{req.name} {version}") for dep in index.dependencies(req.name, version))
    for name in sorted(set(resolution) - reached):
        found.append(f"{name} is in the result but nothing requires it")
    return found


class ResolverCase(unittest.TestCase):
    def assertResolves(self, index, requirements, expected):
        result = resolve(index, list(requirements))
        self.assertEqual(problems(index, requirements, result), [])
        self.assertEqual(result, expected)
        return result

    def assertValid(self, index, requirements):
        result = resolve(index, list(requirements))
        self.assertEqual(problems(index, requirements, result), [])
        return result

    def assertUnsatisfiable(self, index, requirements, involved):
        with self.assertRaises(ResolutionError) as ctx:
            resolve(index, list(requirements))
        message = str(ctx.exception)
        self.assertTrue(
            any(name in message for name in involved),
            f"message {message!r} names none of {sorted(involved)}",
        )

    def resolve_within(self, seconds, index, requirements):
        previous = signal.signal(signal.SIGALRM, _on_alarm)
        signal.setitimer(signal.ITIMER_REAL, seconds)
        try:
            return resolve(index, list(requirements))
        except _OutOfTime:
            self.fail(f"resolve({requirements!r}) took longer than {seconds} seconds")
        finally:
            signal.setitimer(signal.ITIMER_REAL, 0)
            signal.signal(signal.SIGALRM, previous)


class BasicsTests(ResolverCase):
    def test_empty_requirements(self):
        index = PackageIndex({"kiwi": {"1.0.0": []}})
        self.assertEqual(resolve(index, []), {})

    def test_error_class_is_the_exported_one(self):
        self.assertIs(resolver_module.ResolutionError, ResolutionError)
        self.assertTrue(issubclass(ResolutionError, Exception))

    def test_newest_versions_when_they_fit(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["mango^1"], "1.4.0": ["mango^1", "papaya"], "2.0.0": ["mango>=1.2", "papaya>=0.3"]},
            "mango": {"1.0.0": [], "1.2.0": ["guava"], "1.9.1": ["guava<4"]},
            "papaya": {"0.2.0": [], "0.3.0": [], "0.3.7": [], "0.4.0": []},
            "guava": {"1.0.0": [], "2.5.0": [], "3.0.0": []},
            "durian": {"5.0.0": ["kiwi"]},
        })
        self.assertResolves(index, ["kiwi"], lock(kiwi="2.0", mango="1.9.1", papaya="0.4", guava="3.0"))

    def test_root_requirements_on_one_package_all_apply(self):
        index = PackageIndex({"tlsx": {v: [] for v in ["1.0.0", "1.1.0", "1.2.0", "1.3.0", "2.0.0"]}})
        self.assertResolves(index, ["tlsx>=1.0", "tlsx<1.3", "tlsx!=1.2"], lock(tlsx="1.1"))
        self.assertResolves(index, ["tlsx!=2.0", "tlsx"], lock(tlsx="1.3"))

    def test_root_pin_below_what_newest_dependent_wants(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["guava>=1.0"], "1.5.0": ["guava>=1.2"], "2.0.0": ["guava>=1.3"]},
            "guava": {"1.0.0": [], "1.2.0": [], "1.3.0": [], "1.4.0": []},
        })
        self.assertResolves(index, ["kiwi", "guava==1.2"], lock(kiwi="1.5", guava="1.2"))
        self.assertResolves(index, ["guava<1.2", "kiwi"], lock(kiwi="1.0", guava="1.0"))


class BacktrackingTests(ResolverCase):
    DIAMOND = {
        "kiwi": {"1.0.0": ["papaya^1"], "2.0.0": ["papaya^2"]},
        "mango": {"0.9.0": ["papaya^1"], "1.0.0": ["papaya>=1.1,<2"]},
        "papaya": {"1.0.0": [], "1.1.0": [], "1.2.0": [], "2.0.0": [], "2.1.0": []},
    }

    def test_shared_dependency_backs_off(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["papaya>=1"]},
            "mango": {"1.0.0": ["papaya<2"]},
            "papaya": {"1.0.0": [], "1.5.0": [], "2.0.0": []},
        })
        self.assertResolves(index, ["kiwi", "mango"], lock(kiwi="1.0", mango="1.0", papaya="1.5"))

    def test_requester_backs_off_in_a_diamond(self):
        index = PackageIndex(self.DIAMOND)
        expected = lock(kiwi="1.0", mango="1.0", papaya="1.2")
        self.assertResolves(index, ["kiwi", "mango"], expected)

    def test_order_of_root_requirements_does_not_matter(self):
        index = PackageIndex(self.DIAMOND)
        expected = lock(kiwi="1.0", mango="1.0", papaya="1.2")
        for order in itertools.permutations(["kiwi", "mango", "papaya>=1.0"]):
            with self.subTest(order=order):
                self.assertResolves(index, order, expected)

    def test_unknown_dependency_makes_a_version_a_dead_end(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["mango"], "2.0.0": ["mango", "phantom>=1"]},
            "mango": {"1.0.0": [], "1.1.0": []},
        })
        self.assertResolves(index, ["kiwi"], lock(kiwi="1.0", mango="1.1"))

    def test_missing_dependency_version_makes_a_version_a_dead_end(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["mango^1"], "1.1.0": ["mango^1"], "1.2.0": ["mango^1", "guava>=9"]},
            "mango": {"1.0.0": [], "1.3.0": []},
            "guava": {"1.0.0": [], "2.0.0": []},
        })
        self.assertResolves(index, ["kiwi"], lock(kiwi="1.1", mango="1.3"))

    def test_deep_backtracking(self):
        # app 3 -> lib 3 -> core 3 -> base 2 does not exist, so the failure four
        # levels down has to undo the choice of app.
        index = PackageIndex({
            "app": {"1.0.0": ["lib^1"], "2.0.0": ["lib^2"], "3.0.0": ["lib^3"], "3.1.0": ["lib>=3.1,<4"]},
            "lib": {"1.0.0": ["core^1"], "2.0.0": ["core^2"], "2.1.0": ["core>=2.0,<3"],
                    "3.0.0": ["core^3"], "3.1.0": ["core^3"]},
            "core": {"1.0.0": ["base^1"], "2.0.0": ["base^1"], "2.2.0": ["base>=1.1,<2"],
                     "3.0.0": ["base^2"], "3.5.0": ["base^2"]},
            "base": {"1.0.0": [], "1.1.0": [], "1.2.0": []},
        })
        self.assertResolves(index, ["app"], lock(app="2.0", lib="2.1", core="2.2", base="1.2"))

    def test_deep_backtracking_past_unrelated_choices(self):
        # The clash is between what app 2 needs and what zeta (decided late)
        # allows; the packages in between are fine at any version.
        data = {
            "app": {"1.0.0": ["mid1", "mid2", "mid3", "mid4", "zeta"],
                    "2.0.0": ["mid1", "mid2", "mid3", "mid4", "zeta", "omega^2"]},
            "zeta": {"1.0.0": ["omega^1"], "1.1.0": ["omega>=1.1,<2"]},
            "omega": {"1.0.0": [], "1.1.0": [], "2.0.0": []},
        }
        for i in range(1, 5):
            data[f"mid{i}"] = {f"1.{k}.0": ([f"mid{i + 1}"] if i < 4 else []) for k in range(4)}
        index = PackageIndex(data)
        expected = lock(app="1.0", zeta="1.1", omega="1.1", mid1="1.3", mid2="1.3", mid3="1.3", mid4="1.3")
        self.assertResolves(index, ["app"], expected)

    def test_two_packages_back_off_together(self):
        # kiwi 2 and mango 2 both need papaya 2, which needs guava 2, but the
        # root pins guava 1: both have to go back to their 1.x line.
        index = PackageIndex({
            "kiwi": {"1.0.0": ["papaya^1"], "2.0.0": ["papaya^2"]},
            "mango": {"1.0.0": ["papaya^1"], "2.0.0": ["papaya^2"]},
            "papaya": {"1.0.0": ["guava^1"], "1.1.0": ["guava^1"], "2.0.0": ["guava^2"]},
            "guava": {"1.0.0": [], "2.0.0": []},
        })
        expected = lock(kiwi="1.0", mango="1.0", papaya="1.1", guava="1.0")
        self.assertResolves(index, ["kiwi", "mango", "guava^1"], expected)
        self.assertResolves(index, ["guava^1", "mango", "kiwi"], expected)

    def test_version_ruled_out_in_one_branch_is_retried_in_another(self):
        # Under kiwi 3, papaya 2 clashes with guava; under kiwi 2 it is the answer.
        index = PackageIndex({
            "kiwi": {"1.0.0": [], "2.0.0": ["papaya", "guava^1"], "3.0.0": ["papaya", "guava^3"]},
            "papaya": {"1.0.0": ["guava^3", "lychee>=9"], "2.0.0": ["guava^1"]},
            "guava": {"1.0.0": [], "3.0.0": []},
            "lychee": {"1.0.0": []},
        })
        self.assertResolves(index, ["kiwi"], lock(kiwi="2.0", papaya="2.0", guava="1.0"))

    def test_newest_first_picks_the_best_of_several_resolutions(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": [], "2.0.0": ["mango>=1"], "3.0.0": ["mango^3"]},
            "mango": {"1.0.0": [], "2.0.0": [], "2.1.0": ["guava"], "3.0.0": ["phantom"]},
            "guava": {"0.1.0": [], "0.2.0": ["mango<2.1"]},
        })
        self.assertResolves(index, ["kiwi"], lock(kiwi="2.0", mango="2.1", guava="0.1"))

    def test_any_valid_resolution_when_none_is_best(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["papaya^1"], "2.0.0": ["papaya^2"]},
            "mango": {"1.0.0": ["papaya^2"], "2.0.0": ["papaya^1"]},
            "papaya": {"1.0.0": [], "2.0.0": []},
        })
        result = self.assertValid(index, ["kiwi", "mango"])
        self.assertIn(result, [lock(kiwi="2.0", mango="1.0", papaya="2.0"),
                               lock(kiwi="1.0", mango="2.0", papaya="1.0")])


class OnlyWhatIsNeededTests(ResolverCase):
    def test_abandoned_version_leaves_nothing_behind(self):
        # kiwi 2 needs mango, and mango clashes with kiwi 2's own guava^2.
        index = PackageIndex({
            "kiwi": {"1.0.0": ["guava^2"], "2.0.0": ["mango^1", "guava^2"]},
            "mango": {"1.0.0": ["guava^1", "lychee"]},
            "guava": {"1.0.0": [], "2.0.0": [], "2.3.0": []},
            "lychee": {"1.0.0": []},
        })
        self.assertResolves(index, ["kiwi"], lock(kiwi="1.0", guava="2.3"))

    def test_requirements_of_abandoned_versions_stop_counting(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["guava"], "2.0.0": ["guava<2", "lychee>=5"]},
            "guava": {"1.0.0": [], "1.5.0": [], "2.0.0": [], "2.2.0": []},
            "lychee": {"1.0.0": [], "4.0.0": []},
        })
        self.assertResolves(index, ["kiwi"], lock(kiwi="1.0", guava="2.2"))
        self.assertResolves(index, ["guava", "kiwi"], lock(kiwi="1.0", guava="2.2"))

    def test_dependency_dropped_by_older_version_is_not_kept(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["mango^1"], "2.0.0": ["mango^1", "papaya^1"]},
            "mango": {"1.0.0": [], "1.1.0": []},
            "papaya": {"1.0.0": ["durian", "mango<1.1"]},
            "durian": {"1.0.0": []},
            "unused": {"1.0.0": ["kiwi"]},
        })
        # With mango pinned to 1.1, kiwi 2 is out (its papaya wants an older
        # mango), and papaya and durian go with it.
        self.assertResolves(index, ["kiwi", "mango==1.1"], lock(kiwi="1.0", mango="1.1"))
        self.assertResolves(index, ["mango==1.1", "kiwi"], lock(kiwi="1.0", mango="1.1"))


class CycleTests(ResolverCase):
    def test_two_package_cycle(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["mango"], "2.0.0": ["mango^3"]},
            "mango": {"2.0.0": ["kiwi"], "3.0.0": ["kiwi^2"], "3.1.0": ["kiwi>=2"]},
        })
        self.assertResolves(index, ["kiwi"], lock(kiwi="2.0", mango="3.1"))
        self.assertResolves(index, ["mango"], lock(kiwi="2.0", mango="3.1"))

    def test_cycle_that_needs_backtracking(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["mango"], "2.0.0": ["mango^2"]},
            "mango": {"1.0.0": ["kiwi^2"], "2.0.0": ["kiwi^1"]},
        })
        self.assertResolves(index, ["kiwi"], lock(kiwi="1.0", mango="2.0"))

    def test_three_package_cycle_that_needs_backtracking(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["mango^1"], "2.0.0": ["mango^2"]},
            "mango": {"1.0.0": ["papaya^1"], "2.0.0": ["papaya^2"]},
            "papaya": {"1.0.0": ["kiwi^2"], "1.1.0": ["kiwi"], "2.0.0": ["kiwi^1"]},
        })
        self.assertResolves(index, ["kiwi"], lock(kiwi="1.0", mango="1.0", papaya="1.1"))

    def test_package_that_requires_itself(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["kiwi^1", "mango"], "2.0.0": ["kiwi<2"]},
            "mango": {"1.0.0": ["kiwi"]},
        })
        self.assertResolves(index, ["kiwi"], lock(kiwi="1.0", mango="1.0"))

    def test_unsatisfiable_cycle(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["mango^2"], "2.0.0": ["mango^1"]},
            "mango": {"1.0.0": ["kiwi^1"], "2.0.0": ["kiwi^2"]},
        })
        self.assertUnsatisfiable(index, ["kiwi"], {"kiwi", "mango"})


class FailureTests(ResolverCase):
    def test_unknown_root_package(self):
        index = PackageIndex({"kiwi": {"1.0.0": []}})
        self.assertUnsatisfiable(index, ["phantom"], {"phantom"})
        self.assertUnsatisfiable(index, ["kiwi", "phantom>=1.0"], {"phantom"})

    def test_unknown_dependency_in_every_version(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["phantom"], "2.0.0": ["mango", "phantom^2"]},
            "mango": {"1.0.0": []},
        })
        self.assertUnsatisfiable(index, ["kiwi"], {"kiwi", "phantom"})

    def test_no_version_matches(self):
        index = PackageIndex({"kiwi": {"1.0.0": [], "1.5.0": []}})
        self.assertUnsatisfiable(index, ["kiwi>=2"], {"kiwi"})
        self.assertUnsatisfiable(index, ["kiwi==1.0", "kiwi==1.5"], {"kiwi"})

    def test_unsatisfiable_diamond(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["papaya^1"], "2.0.0": ["papaya^1"]},
            "mango": {"1.0.0": ["papaya^2"], "1.1.0": ["papaya>=2.1,<3"]},
            "papaya": {"1.0.0": [], "2.0.0": [], "2.1.0": []},
        })
        self.assertUnsatisfiable(index, ["kiwi", "mango"], {"kiwi", "mango", "papaya"})

    def test_unsatisfiable_deep_chain(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["mango^1"], "2.0.0": ["mango^2"]},
            "mango": {"1.0.0": ["papaya^1"], "2.0.0": ["papaya^2"]},
            "papaya": {"1.0.0": ["guava>=5"], "2.0.0": ["guava<1"]},
            "guava": {"1.0.0": [], "2.0.0": []},
        })
        self.assertUnsatisfiable(index, ["kiwi"], {"kiwi", "mango", "papaya", "guava"})

    def test_root_pin_conflicts_with_every_version(self):
        index = PackageIndex({
            "kiwi": {"1.0.0": ["guava^1"], "2.0.0": ["guava^2"]},
            "guava": {"1.0.0": [], "2.0.0": [], "3.0.0": []},
        })
        self.assertUnsatisfiable(index, ["kiwi", "guava==3.0"], {"kiwi", "guava"})


class ScaleTests(ResolverCase):
    def test_staging_mirror(self):
        index = PackageIndex.load(MIRROR)
        result = self.resolve_within(2.0, index, ["app"])
        self.assertEqual(problems(index, ["app"], result), [])
        expected = lock(
            app="3.2", auth="2.4", base64="3.4", cache="3.4", cli="2.4", codec="3.3",
            config="3.4", crypto="2.6", dates="3.4", db_driver="2.4", http="2.4", i18n="3.4",
            jobs="3.4", json="3.4", jwt="2.4", log="3.4", mail="3.3", metrics="3.4", orm="2.4",
            pool="2.4", queue="3.4", retry="3.4", router="3.4", smtp="3.4", template="3.4",
            tls="2.4", unicode="3.4", web="2.4", yaml="3.4", zlib="3.4",
        )
        self.assertEqual(result, expected)

    def test_large_index_where_newest_versions_fit(self):
        names = [f"pkg{i:02d}" for i in range(30)]
        data = {}
        for i, name in enumerate(names):
            data[name] = {}
            for major in range(1, 4):
                for minor in range(5):
                    deps = [f"{names[j]}>={major}.0,<{major + 1}" for j in (i + 1, i + 2) if j < 30]
                    deps += [f"{names[j]}>=1.{minor}" for j in (i + 5,) if j < 30]
                    data[name][f"{major}.{minor}.0"] = deps
        index = PackageIndex(data)
        result = self.resolve_within(2.0, index, ["pkg00"])
        self.assertEqual(result, {name: Version(3, 4) for name in names})


if __name__ == "__main__":
    unittest.main()
