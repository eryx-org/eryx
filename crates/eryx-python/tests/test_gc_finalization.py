"""Guards CPython object finalization inside the sandbox.

These tests look like they test CPython itself, but they exist to catch a
specific vendoring mistake: `crates/eryx-runtime/libs/libpython3.14.so` and
`libc.so` must be built together from the same wasi-libc revision. Mixing
vintages links cleanly and boots fine, but silently breaks refcount-based
finalization — `__del__` never fires, so buffered writes never flush and files
appear to vanish with no error. See docs/wasi-sdk-wasip2-migration.md.

Both tests below were confirmed to fail against a deliberately mismatched
libc/libpython pair. The cycle collector keeps working in that state, so only
the refcount path is a reliable detector.
"""

import eryx


def test_del_fires_on_refcount_drop():
    sandbox = eryx.Sandbox()
    result = sandbox.execute("""
log = []


class Probe:
    def __del__(self):
        log.append("deleted")


for _ in range(5):
    p = Probe()
    del p

result = log
""")
    assert result.result == ["deleted"] * 5


def test_unclosed_file_is_flushed_on_finalization():
    storage = eryx.VfsStorage()
    session = eryx.Session(vfs=storage)

    session.execute("open('/data/unclosed.txt', 'w').write('flushed')")
    result = session.execute("print(open('/data/unclosed.txt').read())")

    assert result.stdout == "flushed"
