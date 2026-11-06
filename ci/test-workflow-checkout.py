#!/usr/bin/env python3
"""Regression checks for the CI checkout step (LOCK-10).

Runs without a workflow run: it parses .forgejo/workflows/ci.yml and asserts
that the checkout fails closed on the event commit in a job-owned directory.
"""

from pathlib import Path
import sys

import yaml

WORKFLOW = Path(__file__).resolve().parents[1] / ".forgejo" / "workflows" / "ci.yml"


def fail(message):
    print(f"FAIL: {message}", file=sys.stderr)
    sys.exit(1)


if not WORKFLOW.is_file():
    fail(f"missing workflow: {WORKFLOW}")

text = WORKFLOW.read_text(encoding="utf-8")
data = yaml.safe_load(text)

try:
    steps = data["jobs"]["check"]["steps"]
except (KeyError, TypeError):
    fail("ci.yml has no jobs.check.steps")
if not isinstance(steps, list):
    fail("jobs.check.steps is not a list")

checkout = next((step for step in steps if step.get("name") == "Checkout"), None)
if checkout is None:
    fail("ci.yml has no Checkout step")
run = checkout.get("run") or ""
env = checkout.get("env") or {}

if "||" in run:
    fail("Checkout still contains a shell || fallback")
if "git clone --branch" in run or "--branch" in run:
    fail("Checkout still clones a branch name instead of the event commit")
if "github.sha" not in " ".join(str(value) for value in env.values()):
    fail("Checkout does not take the event SHA from github.sha")
if "mktemp -d" not in run:
    fail("Checkout does not create a per-job temporary directory")
if "RUNNER_TEMP" not in run:
    fail("Checkout does not create its directory under RUNNER_TEMP")
if "SRC_DIR" not in run or "GITHUB_ENV" not in run:
    fail("Checkout does not export SRC_DIR for later steps")
if "GITHUB_REPOSITORY" not in run:
    fail("Checkout does not clone the event repository")
if "--depth 1" not in run:
    fail("Checkout lost the shallow depth")
if "checkout" not in run or "--detach" not in run:
    fail("Checkout does not detach at the event commit")
if "rev-parse HEAD" not in run:
    fail("Checkout does not verify the checked out HEAD")
if "exit 1" not in run:
    fail("Checkout does not fail on a mismatch")

for step in steps:
    if step.get("working-directory") == "/src":
        fail(f"step {step.get('name')!r} still uses the shared /src directory")
    body = step.get("run") or ""
    code = "\n".join(
        line for line in body.splitlines() if not line.lstrip().startswith("#")
    )
    if "/src" in code:
        fail(f"step {step.get('name')!r} still references /src")

cargo_steps = [
    step
    for step in steps
    if any(
        line.strip().startswith("cargo ")
        for line in (step.get("run") or "").splitlines()
    )
]
if len(cargo_steps) < 4:
    fail("expected the fmt, clippy, test and release build steps")
for step in cargo_steps:
    if 'cd "$SRC_DIR"' not in (step.get("run") or ""):
        fail(f"step {step.get('name')!r} does not enter SRC_DIR")

print("ok: CI checkout is fail-closed on the event SHA in a per-job directory")
