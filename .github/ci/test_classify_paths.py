#!/usr/bin/env python3
"""Tests for classify-paths.py. Run: python3 .github/ci/test_classify_paths.py"""

import importlib.util
import sys
import pathlib
import unittest

sys.dont_write_bytecode = True  # do not leave __pycache__ in the repository
_spec = importlib.util.spec_from_file_location(
    "classify_paths", pathlib.Path(__file__).with_name("classify-paths.py")
)
cp = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(cp)


def flags(paths, event="pull_request"):
    return cp.classify(paths, event)


def expect(rust=False, desk=False, e2e=False, docs_only=False, all_=False):
    return {"rust": rust, "desk": desk, "e2e": e2e, "docs_only": docs_only, "all": all_}


class ClassifyTest(unittest.TestCase):
    def test_docs_only(self):
        paths = ["docs/blueprint/vision.md", "README.md", "AGENTS.md", "CHANGELOG.md", "LICENSE",
                 "CONTRIBUTING.md"]
        self.assertEqual(flags(paths), expect(docs_only=True))

    def test_desk_only(self):
        self.assertEqual(flags(["desk/src/main.tsx", "desk/package.json"]),
                         expect(desk=True, e2e=True))

    def test_desk_markdown_is_docs(self):
        self.assertEqual(flags(["desk/README.md"]), expect(docs_only=True))

    def test_desk_package_json_is_desk(self):
        self.assertEqual(flags(["desk/package.json"]), expect(desk=True, e2e=True))

    def test_crates_only(self):
        self.assertEqual(flags(["crates/forgebase-core/src/lib.rs"]), expect(rust=True, e2e=True))

    def test_markdown_inside_crates_is_rust(self):
        # include_str! compiles this file into the binary.
        self.assertEqual(flags(["crates/forgebase/templates/app/README.md"]),
                         expect(rust=True, e2e=True))

    def test_examples_only(self):
        self.assertEqual(flags(["examples/sites/ci.sh"]), expect(rust=True, e2e=True))

    def test_workflow_changed(self):
        self.assertEqual(flags([".github/workflows/ci.yml"]), expect(all_=True))
        self.assertEqual(flags([".github/ci/linkcheck.py", "README.md"]), expect(all_=True))

    def test_mixed(self):
        self.assertEqual(flags(["docs/a.md", "desk/src/a.ts", "crates/x/src/lib.rs"]),
                         expect(rust=True, desk=True, e2e=True))
        self.assertEqual(flags(["docs/a.md", "desk/src/a.ts"]), expect(desk=True, e2e=True))

    def test_unknown_file_counts_as_rust(self):
        self.assertEqual(flags(["scripts/foo.sh"]), expect(rust=True, e2e=True))
        self.assertEqual(flags([".gitignore"]), expect(rust=True, e2e=True))

    def test_cargo_lock_and_root_rust_files(self):
        for path in ["Cargo.lock", "Cargo.toml", "rust-toolchain.toml", "deny.toml",
                     ".cargo/config.toml"]:
            self.assertEqual(flags([path]), expect(rust=True, e2e=True), path)

    def test_empty_diff(self):
        self.assertEqual(flags([]), expect())
        self.assertEqual(flags(["", "  "]), expect())

    def test_leading_dot_slash(self):
        self.assertEqual(flags(["./docs/a.md"]), expect(docs_only=True))

    def test_schedule_and_dispatch_run_everything(self):
        everything = expect(rust=True, desk=True, e2e=True, all_=True)
        self.assertEqual(flags([], "schedule"), everything)
        self.assertEqual(flags([], "workflow_dispatch"), everything)
        self.assertEqual(flags(["README.md"], "workflow_dispatch"), everything)

    def test_render(self):
        self.assertEqual(
            cp.render(flags(["README.md"])),
            "rust=false\ndesk=false\ne2e=false\ndocs_only=true\nall=false\n",
        )


if __name__ == "__main__":
    unittest.main()
