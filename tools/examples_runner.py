#!/usr/bin/env python3
"""Docs/example runner (M2, REQ-EV-0212): examples used for evaluation must
be executable and verified, not illustrative placeholders. This runner
executes every declared example in docs/examples/examples.json and fails
release on drift: wrong exit code or missing expected output.

Usage:
  python3 tools/examples_runner.py                 # run all declared examples
  python3 tools/examples_runner.py --self-test     # verify the runner itself
"""
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "docs" / "examples" / "examples.json"


def run_example(example: dict) -> tuple[bool, str]:
    """Executes one declared example; returns (ok, detail)."""
    name = example.get("name", "<unnamed>")
    cmd = example.get("cmd")
    if not name or not cmd:
        return False, f"{name}: manifest entry missing name/cmd (placeholder detected)"
    expect_exit = example.get("expect_exit", 0)
    expects = example.get("expect_stdout_contains", [])
    if not expects:
        return False, f"{name}: no expectation declared — an unverifiable example is a placeholder"

    proc = subprocess.run(
        cmd, shell=True, cwd=ROOT,
        capture_output=True, text=True, timeout=300,
    )
    if proc.returncode != expect_exit:
        return False, f"{name}: exit {proc.returncode} != expected {expect_exit}: {proc.stderr[:300]}"
    for needle in expects:
        if needle not in proc.stdout:
            return False, f"{name}: drift — expected {needle!r} in stdout (got {proc.stdout[:200]!r})"
    return True, f"{name}: ok ({len(expects)} expectation(s) verified)"


def run_all() -> int:
    manifest = json.loads(MANIFEST.read_text())
    examples = manifest.get("examples", [])
    if not examples:
        print("examples-runner: FAIL — manifest declares no examples")
        return 1
    failures = 0
    for example in examples:
        ok, detail = run_example(example)
        print(f"examples-runner: {'PASS' if ok else 'FAIL'} {detail}")
        failures += 0 if ok else 1
    if failures:
        print(f"examples-runner: {failures} drift failure(s) — release blocked")
        return 1
    print(f"examples-runner: OK ({len(examples)} examples executed, no drift)")
    return 0


def self_test() -> int:
    """The runner must catch drift: a wrong expectation must FAIL."""
    cases = [
        {"name": "self-ok", "cmd": "echo hello-examples", "expect_exit": 0,
         "expect_stdout_contains": ["hello-examples"]},
        {"name": "self-drift", "cmd": "echo something-else", "expect_exit": 0,
         "expect_stdout_contains": ["this-will-not-appear"]},
        {"name": "self-badexit", "cmd": "echo nope", "expect_exit": 3,
         "expect_stdout_contains": ["nope"]},
        {"name": "self-placeholder", "cmd": "echo x", "expect_exit": 0,
         "expect_stdout_contains": []},
    ]
    expected = [True, False, False, False]
    failures = 0
    for case, want_ok in zip(cases, expected):
        ok, _ = run_example(case)
        if ok != want_ok:
            print(f"examples-runner self-test: FAIL {case['name']} (got ok={ok}, want {want_ok})")
            failures += 1
    if failures:
        print(f"examples-runner self-test: {failures} failure(s)")
        return 1
    print("examples-runner self-test: OK (drift detection verified)")
    return 0


if __name__ == "__main__":
    sys.exit(self_test() if "--self-test" in sys.argv else run_all())
