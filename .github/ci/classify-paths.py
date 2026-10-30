#!/usr/bin/env python3
"""Classify the paths a pull request changes, to decide which CI jobs run.

Reads one changed path per line on stdin and prints `key=value` lines for $GITHUB_OUTPUT:

  rust=true|false       a Rust job reads one of the paths (anything unknown counts as Rust)
  desk=true|false       a path under desk/ other than a Markdown file, or a DESK_ALSO path
  e2e=true|false        rust or desk
  docs_only=true|false  at least one path, and every path is documentation
  all=true|false        .github/ changed, or the event is schedule or workflow_dispatch

`--event NAME` is the GitHub event name. For `schedule` and `workflow_dispatch` nothing is
diffed (stdin is not read) and everything runs. Standard library only; see
docs/blueprint/maintenance-policy.md (CI) and .github/ci/test_classify_paths.py.
"""

import sys

ALWAYS_ALL_EVENTS = {"schedule", "workflow_dispatch"}

# Read by the Rust jobs (check, sites, msrv-deny, app-template, the Rust build of e2e). Markdown
# inside these directories still counts as Rust: crates/forgebase/templates/ is compiled in with
# include_str!, so a README there is code.
RUST_PREFIXES = ("crates/", "examples/", ".cargo/")
RUST_FILES = {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "deny.toml"}
DOC_FILES = {"AGENTS.md", "CHANGELOG.md", "LICENSE"}
# Files outside desk/ that the desk reads (desk/src/icons.test.ts imports the first one). They
# stay Rust and also run the desk job. Add a path here when a desk file imports from outside desk/.
DESK_ALSO = {"crates/forgebase-core/src/doctype_icons.txt"}


def classify_path(path):
    """Return "all", "rust", "desk" or "docs" for one changed path."""
    path = path.strip()
    if path.startswith("./"):
        path = path[2:]
    if path.startswith(".github/"):
        return "all"
    if path in RUST_FILES or path.startswith(RUST_PREFIXES):
        return "rust"
    if path.startswith("desk/"):
        return "docs" if path.endswith(".md") else "desk"
    if path.startswith("docs/") or path.endswith(".md") or path in DOC_FILES:
        return "docs"
    # Unknown: the safe default is to run the Rust jobs.
    return "rust"


def classify(paths, event=""):
    """Return the outputs as a dict of bools."""
    if event in ALWAYS_ALL_EVENTS:
        return {"rust": True, "desk": True, "e2e": True, "docs_only": False, "all": True}
    kinds = [classify_path(p) for p in paths if p.strip()]
    rust = "rust" in kinds
    desk = "desk" in kinds or any(p.strip().removeprefix("./") in DESK_ALSO for p in paths)
    return {
        "rust": rust,
        "desk": desk,
        "e2e": rust or desk,
        "docs_only": bool(kinds) and all(k == "docs" for k in kinds),
        "all": "all" in kinds,
    }


def render(result):
    return "".join(f"{k}={'true' if v else 'false'}\n" for k, v in result.items())


def main(argv):
    event = ""
    if argv[:1] == ["--event"] and len(argv) == 2:
        event = argv[1]
    elif argv:
        print("usage: classify-paths.py [--event NAME] < paths", file=sys.stderr)
        return 2
    paths = [] if event in ALWAYS_ALL_EVENTS else sys.stdin.read().splitlines()
    sys.stdout.write(render(classify(paths, event)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
