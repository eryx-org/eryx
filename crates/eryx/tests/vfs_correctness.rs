//! End-to-end checks that sandboxed Python sees POSIX-like behaviour from the
//! in-memory VFS mounted at `/data`.
#![cfg(all(feature = "embedded", feature = "unstable-vfs"))]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, OnceLock};

use eryx::PythonExecutor;
use eryx::SessionExecutor;
use eryx::vfs::{ArcStorage, InMemoryStorage};

static EXECUTOR: OnceLock<Arc<PythonExecutor>> = OnceLock::new();

fn executor() -> Arc<PythonExecutor> {
    EXECUTOR
        .get_or_init(|| {
            let resources = eryx::embedded::EmbeddedResources::get().unwrap();
            #[allow(unsafe_code)]
            Arc::new(
                unsafe { PythonExecutor::from_precompiled_file(resources.runtime()) }
                    .unwrap()
                    .with_python_stdlib(resources.stdlib()),
            )
        })
        .clone()
}

async fn run(storage: InMemoryStorage, code: &str) -> String {
    let storage = ArcStorage::new(Arc::new(storage));
    let mut session = SessionExecutor::new_with_vfs(executor(), &[], storage)
        .await
        .expect("session construction");
    session
        .execute(code)
        .run()
        .await
        .expect("execution")
        .stdout_text()
}

#[tokio::test]
async fn utime_and_copy2_preserve_times() {
    let out = run(
        InMemoryStorage::new(),
        r#"
import os, shutil
with open('/data/a', 'w') as f:
    f.write('x')
os.utime('/data/a', (1_000_000, 2_000_000))
shutil.copy2('/data/a', '/data/b')
print(int(os.stat('/data/a').st_mtime), int(os.stat('/data/b').st_mtime))
"#,
    )
    .await;
    assert_eq!(out.trim(), "2000000 2000000");
}

#[tokio::test]
async fn invalid_directory_renames_fail_with_posix_errors() {
    let out = run(
        InMemoryStorage::new(),
        r#"
import errno, os
os.makedirs('/data/a/inner')
os.makedirs('/data/b')
open('/data/b/keep', 'w').close()
for src, dst in [('/data/a', '/data/a/inner/x'), ('/data/a', '/data/b')]:
    try:
        os.rename(src, dst)
        print('renamed')
    except OSError as e:
        print(errno.errorcode[e.errno])
print(sorted(os.listdir('/data/a')), sorted(os.listdir('/data/b')))
"#,
    )
    .await;
    assert_eq!(
        out.trim(),
        "EINVAL\nENOTEMPTY\n['inner'] ['keep']",
        "unexpected output: {out}"
    );
}

#[tokio::test]
async fn tmp_is_writable_and_separate_from_data() {
    let out = run(
        InMemoryStorage::new(),
        r#"
import os, tempfile
with tempfile.NamedTemporaryFile('w') as f:
    f.write('scratch')
    f.flush()
    print(os.path.dirname(f.name))
with open('/tmp/x', 'w') as f:
    f.write('tmp')
with open('/data/x', 'w') as f:
    f.write('data')
print(open('/tmp/x').read(), open('/data/x').read())
"#,
    )
    .await;
    assert_eq!(out.trim(), "/tmp\ntmp data");
}

#[tokio::test]
async fn tmp_and_data_share_one_quota() {
    // Room for the two mounts' entries and a little more, not for 8 KiB.
    let out = run(
        InMemoryStorage::with_max_bytes(4096),
        r#"
with open('/data/a', 'wb') as f:
    f.write(b'x' * 3000)
with open('/tmp/b', 'wb') as f:
    f.write(b'x' * 100)
print('small fits')
try:
    with open('/tmp/c', 'wb') as f:
        f.write(b'x' * 3000)
    print('fit')
except OSError:
    print('full')
"#,
    )
    .await;
    assert_eq!(out.trim(), "small fits\nfull");
}

#[tokio::test]
async fn stateless_sandbox_has_tmp() {
    let sandbox = eryx::Sandbox::embedded()
        .build()
        .expect("should build sandbox");
    let result = sandbox
        .execute("import tempfile\nprint(tempfile.gettempdir())")
        .await
        .expect("execution");
    assert_eq!(result.stdout_text().trim(), "/tmp");
}

/// Over-quota writes fail with `ENOSPC` when positional, but the guest's libc
/// flattens stream write errors to `EIO` (eryx-org/eryx#762). Tighten the
/// stream cases to `ENOSPC` once libc translates them.
#[tokio::test]
async fn write_over_quota_errnos() {
    let out = run(
        InMemoryStorage::with_max_bytes(64 * 1024),
        r#"
import errno, os
for name, write in [
    ('write', lambda f: f.write(b'x' * 200_000)),
    ('os.write', lambda f: os.write(f.fileno(), b'x' * 200_000)),
    ('os.pwrite', lambda f: os.pwrite(f.fileno(), b'x' * 200_000, 0)),
]:
    with open('/data/big_' + name, 'wb', buffering=0) as f:
        try:
            write(f)
            print(name, 'ok')
        except OSError as e:
            print(name, errno.errorcode[e.errno])
"#,
    )
    .await;
    assert_eq!(
        out.trim(),
        "write EIO\nos.write EIO\nos.pwrite ENOSPC",
        "unexpected output: {out}"
    );
}
