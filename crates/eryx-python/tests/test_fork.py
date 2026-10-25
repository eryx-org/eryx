"""A forked child can create and use new sandboxes."""

import os
import time

import eryx
import pytest


@pytest.mark.skipif(not hasattr(os, "fork"), reason="needs os.fork")
@pytest.mark.filterwarnings("ignore::DeprecationWarning")
def test_sandbox_works_in_forked_child():
    # Start the parent's runtime before forking.
    assert eryx.Sandbox().execute("print(1)").stdout.strip() == b"1"

    pid = os.fork()
    if pid == 0:
        code = 1
        try:
            out = eryx.Sandbox().execute("print(2)").stdout.strip()
            code = 0 if out == b"2" else 1
        finally:
            os._exit(code)

    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        done, status = os.waitpid(pid, os.WNOHANG)
        if done:
            assert os.waitstatus_to_exitcode(status) == 0
            return
        time.sleep(0.1)
    os.kill(pid, 9)
    os.waitpid(pid, 0)
    pytest.fail("sandbox in forked child hung")
