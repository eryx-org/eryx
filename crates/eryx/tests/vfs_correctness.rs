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
