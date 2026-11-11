#!/usr/bin/env python3
"""Reads the ledger with pandas, then makes a small git repository, clones it and reads the log."""
import os
import subprocess
import tempfile

import pandas as pd

here = os.path.dirname(os.path.abspath(__file__))
frame = pd.read_csv(os.path.join(here, "ledger.csv"))
# The answer is assembled here, so no mock script or command text carries it.
print("toolchain-total-" + format(frame["amount"].sum(), ".2f"))

work = tempfile.mkdtemp(prefix="toolchain-", dir="/tmp")
origin = os.path.join(work, "origin")
clone = os.path.join(work, "clone")
ident = ["-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid"]


def git(*args, cwd=None):
    out = subprocess.run(["git", *ident, *args], cwd=cwd, check=True, capture_output=True, text=True)
    return out.stdout.strip()


os.makedirs(origin)
git("init", "-q", cwd=origin)
for subject in ("Add ledger", "Fix rounding in ledger"):
    with open(os.path.join(origin, "ledger.txt"), "a") as f:
        f.write(subject + "\n")
    git("add", ".", cwd=origin)
    git("commit", "-q", "-m", subject, cwd=origin)
git("clone", "-q", origin, clone)
print("toolchain-commit: " + git("log", "-1", "--format=%s", cwd=clone))
