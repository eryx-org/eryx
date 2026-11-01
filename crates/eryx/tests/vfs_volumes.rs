//! End-to-end checks of host volume mounts as sandboxed Python sees them.
#![cfg(all(feature = "embedded", feature = "unstable-vfs"))]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, OnceLock};

use eryx::vfs::{ArcStorage, InMemoryStorage};
use eryx::{PythonExecutor, SessionExecutor, VfsConfig, VolumeMount};

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

async fn run_with_volume(volume: VolumeMount, code: &str) -> String {
    let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
    let config = VfsConfig::default().with_volume(volume);
    let mut session = SessionExecutor::new_with_vfs_config(executor(), &[], storage, config)
        .await
        .expect("session construction");
    session
        .execute(code)
        .run()
        .await
        .expect("execution")
        .stdout_text()
}

/// A writable single-file mount gives access to that file and nothing else
/// in the host directory holding it.
#[tokio::test]
async fn single_file_volume_exposes_only_its_file() {
    let host = tempfile::tempdir().unwrap();
    std::fs::write(host.path().join("mounted.txt"), b"mounted").unwrap();
    std::fs::write(host.path().join("sibling.txt"), b"sibling").unwrap();

    let stdout = run_with_volume(
        VolumeMount::new(host.path().join("mounted.txt"), "/mnt/file.txt"),
        r#"
import os

def attempt(name, f):
    try:
        print(f"{name}: allowed {f()!r}")
    except OSError as e:
        print(f"{name}: refused {type(e).__name__}")

print(open('/mnt/file.txt').read())
with open('/mnt/file.txt', 'w') as f:
    f.write('updated')
attempt('listdir', lambda: sorted(os.listdir('/mnt')))
attempt('sibling', lambda: open('/mnt/sibling.txt').read())
attempt('symlink', lambda: os.symlink('.', '/mnt/file.txt.link'))
os.remove('/mnt/file.txt')
attempt('symlink-over', lambda: os.symlink('.', '/mnt/file.txt'))
attempt('mkdir-over', lambda: os.mkdir('/mnt/file.txt'))
attempt('listdir-through', lambda: os.listdir('/mnt/file.txt'))
"#,
    )
    .await;

    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "mounted", "{stdout}");
    // The mount point itself can't be listed.
    assert!(!lines[1].contains("sibling"), "{stdout}");
    for line in &lines[2..] {
        assert!(line.contains("refused"), "{stdout}");
    }
    assert!(!host.path().join("mounted.txt").exists());
    assert_eq!(
        std::fs::read(host.path().join("sibling.txt")).unwrap(),
        b"sibling"
    );
    assert_eq!(std::fs::read_dir(host.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn directory_volume_refuses_symlinks() {
    let host = tempfile::tempdir().unwrap();
    let stdout = run_with_volume(
        VolumeMount::new(host.path(), "/mnt"),
        r#"
import os
for target in ['/etc/passwd', '../outside', 'inside']:
    try:
        os.symlink(target, '/mnt/link')
        print('created')
    except OSError as e:
        print(type(e).__name__)
"#,
    )
    .await;
    assert!(!stdout.contains("created"), "{stdout}");
    assert_eq!(std::fs::read_dir(host.path()).unwrap().count(), 0);
}

/// Grows `path` past an 8-byte limit and prints whether the write went through.
fn grow_file(path: &str) -> String {
    // The guest's libc reports a refused stream write as EIO rather than
    // EFBIG, so only check that it fails.
    format!(
        r#"
with open('{path}', 'wb', buffering=0) as f:
    f.write(b'12345678')
    try:
        f.write(b'9')
        print('grew')
    except OSError:
        print('refused')
"#
    )
}

#[tokio::test]
async fn volume_max_file_size_is_enforced() {
    let host = tempfile::tempdir().unwrap();
    std::fs::write(host.path().join("single.txt"), b"").unwrap();

    // A single-file mount, through a session.
    let stdout = run_with_volume(
        VolumeMount::new(host.path().join("single.txt"), "/mnt/single.txt").with_max_file_size(8),
        &grow_file("/mnt/single.txt"),
    )
    .await;
    assert_eq!(stdout.trim(), "refused");
    assert_eq!(
        std::fs::metadata(host.path().join("single.txt"))
            .unwrap()
            .len(),
        8
    );

    // A directory mount, through a stateless sandbox.
    let sandbox = eryx::Sandbox::embedded()
        .with_volume(VolumeMount::new(host.path(), "/mnt").with_max_file_size(8))
        .build()
        .unwrap();
    let result = sandbox.execute(&grow_file("/mnt/dir.txt")).await.unwrap();
    assert_eq!(result.stdout_text().trim(), "refused");
    assert_eq!(
        std::fs::metadata(host.path().join("dir.txt"))
            .unwrap()
            .len(),
        8
    );
}

/// Holds host files open until one is refused and prints how many succeeded.
const HOLD_OPEN_FILES: &str = r#"
import errno
held = []
try:
    for _ in range(50):
        held.append(open('/python-stdlib/os.py', 'rb'))
except OSError as e:
    print(len(held), errno.errorcode[e.errno])
"#;

#[tokio::test]
async fn open_host_file_limit_is_enforced() {
    let limits = eryx::ResourceLimits::default().with_max_open_host_files(5);

    let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
    let mut session = SessionExecutor::new_with_vfs_config_and_limits(
        executor(),
        &[],
        storage,
        VfsConfig::default(),
        &limits,
    )
    .await
    .unwrap();
    let stdout = session
        .execute(HOLD_OPEN_FILES)
        .run()
        .await
        .unwrap()
        .stdout_text();
    assert_eq!(stdout.trim(), "5 EDQUOT");

    let sandbox = eryx::Sandbox::embedded()
        .with_resource_limits(limits)
        .build()
        .unwrap();
    let result = sandbox.execute(HOLD_OPEN_FILES).await.unwrap();
    assert_eq!(result.stdout_text().trim(), "5 EDQUOT");
}
