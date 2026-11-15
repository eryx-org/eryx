//! End-to-end tests for the WASI 0.3 hybrid VFS: a real `wasm32-wasip3`
//! command (tests/fixtures/p3-fs-probe) runs under a p3 linker whose
//! `wasi:filesystem@0.3.0` is shadowed by `add_hybrid_vfs_to_linker_p3`.
//!
//! The probe is built on first use, which needs the `wasm32-wasip3` target:
//! `rustup target add wasm32-wasip3`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use eryx_vfs::{
    ArcStorage, DirPerms, FilePerms, HybridVfsCtx, HybridVfsState, HybridVfsView, InMemoryStorage,
    VfsStorage, add_hybrid_vfs_to_linker_p3,
};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{I32Exit, WasiCtx, WasiCtxView, WasiView};

struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    vfs: HybridVfsCtx<ArcStorage>,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl HybridVfsView for State {
    type Storage = ArcStorage;

    fn hybrid_vfs(&mut self) -> HybridVfsState<'_, ArcStorage> {
        HybridVfsState::new(&mut self.vfs, &mut self.table)
    }
}

/// Build the probe component once per test process.
fn probe() -> &'static PathBuf {
    static PROBE: OnceLock<PathBuf> = OnceLock::new();
    PROBE.get_or_init(|| {
        let manifest =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/p3-fs-probe/Cargo.toml");
        let target_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("p3-fs-probe");
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--quiet", "--release", "--locked"])
            .args(["--target", "wasm32-wasip3", "--manifest-path"])
            .arg(&manifest)
            .env("CARGO_TARGET_DIR", &target_dir)
            .status()
            .expect("running cargo");
        assert!(
            status.success(),
            "building the p3 probe failed; is the wasm32-wasip3 target installed \
             (`rustup target add wasm32-wasip3`)?"
        );
        target_dir.join("wasm32-wasip3/release/p3-fs-probe.wasm")
    })
}

fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.wasm_component_model_async(true);
        // Go's wasip3 runtime needs these too (see clis/smoke.sh).
        config.wasm_component_model_more_async_builtins(true);
        config.async_stack_size(16 << 20);
        config.max_wasm_stack(8 << 20);
        Engine::new(&config).unwrap()
    })
}

/// Run the probe with `args` against `vfs`, returning its exit code and stdout.
async fn run(vfs: HybridVfsCtx<ArcStorage>, args: &[&str]) -> (i32, String) {
    run_component(vfs, probe(), &[&["probe"], args].concat(), &[]).await
}

/// Run the command component at `path` with `argv` and `env` against `vfs`,
/// returning its exit code and stdout.
async fn run_component(
    vfs: HybridVfsCtx<ArcStorage>,
    path: &std::path::Path,
    argv: &[&str],
    env: &[(&str, &str)],
) -> (i32, String) {
    let engine = engine();
    let component = Component::from_file(engine, path).unwrap();
    let mut linker = Linker::<State>::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker).unwrap();
    linker.allow_shadowing(true);
    add_hybrid_vfs_to_linker_p3(&mut linker).unwrap();

    let stdout = MemoryOutputPipe::new(1 << 20);
    let wasi = WasiCtx::builder()
        .args(argv)
        .envs(env)
        .stdout(stdout.clone())
        .stderr(stdout.clone())
        .build();
    let mut store = Store::new(
        engine,
        State {
            wasi,
            table: ResourceTable::new(),
            vfs,
        },
    );
    let command =
        wasmtime_wasi::p3::bindings::Command::instantiate_async(&mut store, &component, &linker)
            .await
            .unwrap();
    let result = store
        .run_concurrent(async move |accessor| command.wasi_cli_run().call_run(accessor).await)
        .await
        .and_then(|r| r);
    let code = match result {
        Ok(Ok(())) => 0,
        Ok(Err(())) => 1,
        Err(err) => err
            .downcast_ref::<I32Exit>()
            .map(|exit| exit.0)
            .unwrap_or_else(|| panic!("probe trapped: {err:?}")),
    };
    let out = String::from_utf8(stdout.contents().to_vec()).unwrap();
    (code, out.trim_end().to_string())
}

fn storage(max_bytes: Option<u64>) -> ArcStorage {
    let inner = match max_bytes {
        Some(max) => InMemoryStorage::with_max_bytes(max),
        None => InMemoryStorage::new(),
    };
    ArcStorage::new(Arc::new(inner))
}

fn ctx(storage: &ArcStorage) -> HybridVfsCtx<ArcStorage> {
    let mut ctx = HybridVfsCtx::new(storage.clone());
    ctx.add_vfs_preopen("/data", DirPerms::all(), FilePerms::all());
    ctx
}

#[tokio::test]
async fn p3_guest_reads_and_writes_vfs_files() {
    let storage = storage(None);

    assert_eq!(
        run(ctx(&storage), &["write", "/data/a.txt", "hello"]).await,
        (0, "ok".into())
    );
    assert_eq!(storage.read("/data/a.txt").await.unwrap(), b"hello");

    storage
        .write("/data/b.txt", b"from the host")
        .await
        .unwrap();
    assert_eq!(
        run(ctx(&storage), &["read", "/data/b.txt"]).await,
        (0, "from the host".into())
    );

    assert_eq!(
        run(ctx(&storage), &["append", "/data/a.txt", " world"]).await,
        (0, "ok".into())
    );
    assert_eq!(storage.read("/data/a.txt").await.unwrap(), b"hello world");

    assert_eq!(
        run(ctx(&storage), &["mkdir", "/data/sub"]).await,
        (0, "ok".into())
    );
    assert_eq!(
        run(ctx(&storage), &["rename", "/data/b.txt", "/data/sub/c.txt"]).await,
        (0, "ok".into())
    );
    assert_eq!(
        run(ctx(&storage), &["ls", "/data"]).await,
        (0, "a.txt\nsub".into())
    );
    assert_eq!(
        run(ctx(&storage), &["rm", "/data/sub/c.txt"]).await,
        (0, "ok".into())
    );
    assert_eq!(
        run(ctx(&storage), &["ls", "/data/sub"]).await,
        (0, "".into())
    );
}

#[tokio::test]
async fn p3_guest_cannot_reach_paths_outside_its_mounts() {
    let storage = storage(None);
    storage.write("/outside", b"secret").await.unwrap();
    storage.mkdir("/data").await.unwrap();
    storage.mkdir("/data/sub").await.unwrap();

    for path in [
        "/outside",
        "/etc/passwd",
        "/data/../outside",
        "/data/sub/../../outside",
    ] {
        let (code, out) = run(ctx(&storage), &["read", path]).await;
        assert_eq!(code, 1, "{path}: {out}");
        assert!(out.starts_with("error:"), "{path}: {out}");
    }
    let (code, out) = run(ctx(&storage), &["write", "/data/sub/../../escaped", "x"]).await;
    assert_eq!(code, 1, "{out}");
    assert!(!storage.exists("/escaped").await.unwrap());
}

#[tokio::test]
async fn p3_guest_writes_stop_at_the_storage_quota() {
    // Each write is checked against the quota, so a 1 MiB file can't fit.
    let storage = storage(Some(256 * 1024));
    let (code, out) = run(ctx(&storage), &["fill", "/data/big", "1048576"]).await;
    assert_eq!((code, out.as_str()), (1, "error: StorageFull"));
    let size = storage.stat("/data/big").await.unwrap().size;
    assert!(size <= 256 * 1024, "{size}");
}

#[tokio::test]
async fn p3_guest_cannot_write_to_a_read_only_mount() {
    let storage = storage(None);
    storage.mkdir("/ro").await.unwrap();
    storage.write("/ro/f", b"original").await.unwrap();
    let mut vfs = HybridVfsCtx::new(storage.clone());
    vfs.add_vfs_preopen("/ro", DirPerms::READ, FilePerms::READ);

    assert_eq!(run(vfs, &["read", "/ro/f"]).await, (0, "original".into()));
    for args in [
        ["write", "/ro/f", "changed"],
        ["append", "/ro/f", "changed"],
        ["write", "/ro/new", "x"],
    ] {
        let mut vfs = HybridVfsCtx::new(storage.clone());
        vfs.add_vfs_preopen("/ro", DirPerms::READ, FilePerms::READ);
        let (code, out) = run(vfs, &args).await;
        assert_eq!(code, 1, "{args:?}: {out}");
    }
    let mut vfs = HybridVfsCtx::new(storage.clone());
    vfs.add_vfs_preopen("/ro", DirPerms::READ, FilePerms::READ);
    assert_eq!(run(vfs, &["mkdir", "/ro/d"]).await.0, 1);
    assert_eq!(storage.read("/ro/f").await.unwrap(), b"original");
    assert!(!storage.exists("/ro/new").await.unwrap());
}

/// Opt-in: run a Go-built `gh` (wasip3) against the VFS. Set ERYX_TEST_GH_WASM
/// to its component, e.g. from the `CLI components` workflow's gh artifact.
#[tokio::test]
#[ignore = "needs ERYX_TEST_GH_WASM"]
async fn p3_gh_keeps_its_config_on_the_vfs() {
    let gh = std::env::var_os("ERYX_TEST_GH_WASM").expect("ERYX_TEST_GH_WASM");
    let storage = storage(None);
    let mut vfs = HybridVfsCtx::new(storage.clone());
    vfs.add_vfs_preopen("/home", DirPerms::all(), FilePerms::all());
    let (code, out) = run_component(
        vfs,
        &PathBuf::from(gh),
        &["gh", "config", "set", "editor", "vim"],
        &[("HOME", "/home"), ("GH_NO_UPDATE_NOTIFIER", "1")],
    )
    .await;
    assert_eq!(code, 0, "{out}");
    let config = storage.read("/home/.config/gh/config.yml").await.unwrap();
    assert!(String::from_utf8_lossy(&config).contains("editor: vim"));
}
