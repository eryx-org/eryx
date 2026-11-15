"""Processes that start together must all be able to use an empty cache (#850).

Each process extracts the embedded runtime and stdlib into the eryx cache
directory on first use. On Windows, several workers starting at once used to
fail ``eryx.Sandbox()`` with ``failed to open file``.

This is a cheap end-to-end smoke test: it never reproduced the race itself.
The threaded tests in ``crates/eryx/src/embedded.rs`` and ``cache.rs`` do.
"""

import os
import subprocess
import sys
import time

PROCESSES = 8
# Real workers don't start in lockstep: spread the launches so that some
# processes arrive while others are still extracting.
LAUNCH_INTERVAL = 0.02

CHILD = "import eryx; eryx.Sandbox().execute('pass')"


def test_concurrent_first_use_of_an_empty_cache(tmp_path):
    env = {**os.environ, "XDG_CACHE_HOME": str(tmp_path / "cache")}
    procs = []
    for _ in range(PROCESSES):
        procs.append(
            subprocess.Popen(
                [sys.executable, "-c", CHILD],
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                env=env,
            )
        )
        time.sleep(LAUNCH_INTERVAL)
    failures = []
    for proc in procs:
        out, _ = proc.communicate(timeout=300)
        if proc.returncode != 0:
            failures.append(out.strip().splitlines()[-1] if out.strip() else "")
    assert not failures, f"{len(failures)} failed: {failures}"
