//! End-to-end tests for client-served mounts (`ExecuteRequest.mounts`): the
//! test plays the client, answering the server's `FsRequest`s from a small
//! in-memory tree.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use eryx::{PoolConfig, Sandbox, SandboxPool};
use eryx_server::proto::eryx::v1::eryx_client::EryxClient;
use eryx_server::proto::eryx::v1::{
    ClientMessage, ExecuteRequest, ExecuteResult, FileKind, FsChange, FsEntry, FsError,
    FsErrorCode, FsListing, FsMetadata, FsResponse, Language, Mount, SupportingFile,
    client_message, fs_change, fs_request, fs_response, server_message,
};
use eryx_server::service::EryxService;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Server};

/// What the client serves at `/mnt/repo`: path (relative to the mount) and
/// contents. `private/key` is refused: its stat fails.
fn tree() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("README.md", b"hello mounts\n".to_vec()),
        ("src/main.py", b"print('hi')\n".to_vec()),
        // Bigger than a block, so it's read in several
        (
            "big.bin",
            (0..300_000u32).map(|i| (i % 251) as u8).collect(),
        ),
        ("private/key", b"nope".to_vec()),
    ]
}

/// Answer one `FsRequest` from `tree`.
fn answer(op: fs_request::Op, path: &str) -> fs_response::Result {
    let files = tree();
    let file = files.iter().find(|(p, _)| *p == path);
    let is_dir = path.is_empty()
        || files
            .iter()
            .any(|(p, _)| p.starts_with(&format!("{path}/")));
    let meta = |c: &[u8]| FsMetadata {
        is_dir: false,
        size: c.len() as u64,
        modified: Some(1_700_000_000.0),
    };
    let dir = FsMetadata {
        is_dir: true,
        ..FsMetadata::default()
    };
    let error = |code: FsErrorCode| {
        fs_response::Result::Error(FsError {
            code: code.into(),
            message: String::new(),
        })
    };
    match (op, file) {
        (_, Some(("private/key", _))) => error(FsErrorCode::PermissionDenied),
        (fs_request::Op::Stat(_), Some((_, c))) => fs_response::Result::Stat(meta(c)),
        (fs_request::Op::Stat(_), None) if is_dir => fs_response::Result::Stat(dir),
        (fs_request::Op::List(_), None) if is_dir => {
            let prefix = if path.is_empty() {
                String::new()
            } else {
                format!("{path}/")
            };
            let mut entries: Vec<FsEntry> = Vec::new();
            for (p, c) in &files {
                let Some(rest) = p.strip_prefix(&prefix) else {
                    continue;
                };
                match rest.split_once('/') {
                    Some((sub, _)) if !entries.iter().any(|e| e.name == sub) => {
                        entries.push(FsEntry {
                            name: sub.into(),
                            metadata: Some(dir),
                            content: None,
                        });
                    }
                    Some(_) => {}
                    None => entries.push(FsEntry {
                        name: rest.into(),
                        metadata: Some(meta(c)),
                        // Small files come with the listing
                        content: (c.len() < 64).then(|| c.clone()),
                    }),
                }
            }
            fs_response::Result::Listing(FsListing { entries })
        }
        (fs_request::Op::List(_), Some(_)) => error(FsErrorCode::NotADirectory),
        (fs_request::Op::Read(read), Some((_, c))) => {
            let from = (read.offset as usize).min(c.len());
            let to = (from + read.len as usize).min(c.len());
            fs_response::Result::Data(c[from..to].to_vec())
        }
        (fs_request::Op::Read(_), None) if is_dir => error(FsErrorCode::IsADirectory),
        _ => error(FsErrorCode::NotFound),
    }
}

async fn start_server() -> Channel {
    start_server_with(eryx_server::service::ServerLimits::default()).await
}

async fn start_server_with(limits: eryx_server::service::ServerLimits) -> Channel {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let pool = SandboxPool::new(Sandbox::embedded(), PoolConfig::default().with_max_size(2))
        .await
        .unwrap();
    let service = EryxService::new(Arc::new(pool)).with_limits(limits);
    #[cfg(feature = "shell")]
    let service = {
        let create: eryx_server::service::PoolFactory = Arc::new(|| {
            Box::pin(async {
                let config = PoolConfig::default().with_max_size(2);
                Ok(Arc::new(
                    SandboxPool::new(Sandbox::embedded().shell(), config).await?,
                ))
            })
        });
        service.with_lazy_pool(eryx::Language::Shell, create)
    };
    #[cfg(feature = "javascript")]
    let service = {
        let create: eryx_server::service::PoolFactory = Arc::new(|| {
            Box::pin(async {
                let config = PoolConfig::default().with_max_size(2);
                Ok(Arc::new(
                    SandboxPool::new(Sandbox::embedded().javascript(), config).await?,
                ))
            })
        });
        service.with_lazy_pool(eryx::Language::JavaScript, create)
    };
    let service = service.into_server();
    tokio::spawn(async move {
        Server::builder()
            .add_service(service)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

/// Run `code` with `tree` mounted at `/mnt/repo`, answering the server's
/// file operations, and return the result and how many it asked for.
async fn run(channel: Channel, language: Language, code: &str) -> (ExecuteResult, usize) {
    let request = ExecuteRequest {
        code: code.to_string(),
        language: language.into(),
        mounts: vec![Mount {
            path: "/mnt/repo".into(),
            writable: false,
            max_write_bytes: None,
        }],
        ..Default::default()
    };
    run_request(channel, request).await
}

/// Send `request`, answering the server's file operations from `tree`.
async fn run_request(channel: Channel, request: ExecuteRequest) -> (ExecuteResult, usize) {
    let (result, ops, changes) = exchange(channel, request).await;
    assert!(changes.is_empty(), "{changes:?}");
    (result, ops)
}

/// Send `request`, answering the server's file operations from `tree`, and
/// return the result, how many operations it asked for, and the changes it
/// sent for writable mounts.
async fn exchange(
    channel: Channel,
    request: ExecuteRequest,
) -> (ExecuteResult, usize, Vec<FsChange>) {
    let mut client = EryxClient::new(channel);
    let (tx, rx) = mpsc::channel(16);
    let mount = request.mounts.first().map(|m| m.path.clone());
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(request))),
    })
    .await
    .unwrap();
    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    let mut ops = 0;
    let mut changes = Vec::new();
    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::FsRequest(req)) => {
                ops += 1;
                assert_eq!(Some(&req.mount), mount.as_ref());
                let result = answer(req.op.unwrap(), &req.path);
                tx.send(ClientMessage {
                    message: Some(client_message::Message::FsResponse(FsResponse {
                        op_id: req.op_id,
                        result: Some(result),
                    })),
                })
                .await
                .unwrap();
            }
            Some(server_message::Message::FsChange(change)) => changes.push(change),
            Some(server_message::Message::ExecuteResult(result)) => {
                assert_eq!(result.fs_changes as usize, changes.len());
                return (*result, ops, changes);
            }
            _ => {}
        }
    }
    panic!("the stream ended without an ExecuteResult");
}

fn stdout(result: &ExecuteResult) -> String {
    assert!(
        result.success,
        "{}\n{}",
        result.error,
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8_lossy(&result.stdout).into_owned()
}

#[tokio::test]
async fn python_reads_a_mount() {
    let channel = start_server().await;
    let code = r"
import os
print(sorted(os.listdir('/mnt/repo')))
print(open('/mnt/repo/README.md').read(), end='')
print(open('/mnt/repo/src/main.py').read(), end='')
data = open('/mnt/repo/big.bin', 'rb').read()
print(len(data), data == bytes(i % 251 for i in range(300_000)))
print(os.path.isdir('/mnt/repo/src'), os.path.getsize('/mnt/repo/README.md'))
for path, error in [('/mnt/repo/missing', FileNotFoundError), ('/mnt/repo/private/key', PermissionError)]:
    try:
        open(path).read()
    except error:
        print('refused', path)
try:
    open('/mnt/repo/new.txt', 'w')
except OSError:
    print('read-only')
";
    let (result, ops) = run(channel.clone(), Language::Python, code).await;
    assert_eq!(
        stdout(&result),
        "['README.md', 'big.bin', 'private', 'src']\n\
         hello mounts\n\
         print('hi')\n\
         300000 True\n\
         True 13\n\
         refused /mnt/repo/missing\n\
         refused /mnt/repo/private/key\n\
         read-only\n"
    );
    // The listings bring the small files: the rest is big.bin's blocks and
    // a few stats
    assert!(ops < 15, "{ops} file operations");

    // Supporting files and mounts together: the files' VFS and the mounts
    // are both there
    let request = ExecuteRequest {
        code: "import helper\nprint(helper.X, open('/mnt/repo/README.md').read(), end='')".into(),
        files: vec![SupportingFile {
            name: "helper.py".into(),
            content: "X = 42".into(),
            kind: FileKind::Module.into(),
        }],
        mounts: vec![Mount {
            path: "/mnt/repo".into(),
            writable: false,
            max_write_bytes: None,
        }],
        ..Default::default()
    };
    let (result, _) = run_request(channel.clone(), request).await;
    assert_eq!(stdout(&result), "42 hello mounts\n");

    // A request without mounts has none, and never asks
    let request = ExecuteRequest {
        code: "import os\nprint(os.path.exists('/mnt/repo'))".into(),
        ..Default::default()
    };
    let (result, ops) = run_request(channel, request).await;
    assert_eq!(stdout(&result), "False\n");
    assert_eq!(ops, 0);
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn the_shell_reads_a_mount() {
    let channel = start_server().await;
    let code = r#"
for f in /mnt/repo/*; do echo "$f"; done
read -r line < /mnt/repo/README.md; echo "$line"
cat /mnt/repo/src/main.py
[ -d /mnt/repo/src ] && echo dir
echo x > /mnt/repo/new.txt 2>/dev/null || echo read-only
"#;
    let (result, _) = run(channel, Language::Shell, code).await;
    assert_eq!(
        stdout(&result),
        "/mnt/repo/README.md\n/mnt/repo/big.bin\n/mnt/repo/private\n/mnt/repo/src\n\
         hello mounts\n\
         print('hi')\n\
         dir\n\
         read-only\n"
    );
}

#[cfg(feature = "javascript")]
#[tokio::test]
async fn javascript_reads_a_mount() {
    let channel = start_server().await;
    let code = r"
        const fs = require('fs');
        console.log(fs.readdirSync('/mnt/repo').sort().join(','));
        console.log(fs.readFileSync('/mnt/repo/src/main.py', 'utf8').trim());
        console.log(fs.readFileSync('/mnt/repo/big.bin').length);
        try { fs.writeFileSync('/mnt/repo/new.txt', 'x') } catch (e) { console.log('read-only') }
    ";
    let (result, _) = run(channel, Language::Javascript, code).await;
    assert_eq!(
        stdout(&result),
        "README.md,big.bin,private,src\nprint('hi')\n300000\nread-only\n"
    );
}

/// With `--mount-root /workspace`, a mount can be at `/workspace` itself, and
/// `/mnt/` no longer takes mounts
#[tokio::test]
async fn the_operators_mount_root_applies() {
    let limits = eryx_server::service::ServerLimits {
        mount_roots: eryx_server::mounts::MountRoots::new(["/workspace".to_string()]).unwrap(),
        ..Default::default()
    };
    let channel = start_server_with(limits).await;
    let request = |path: &str| ExecuteRequest {
        code: "import os\nprint(sorted(os.listdir('/workspace')))\n\
               print(open('/workspace/README.md').read(), end='')"
            .into(),
        mounts: vec![Mount {
            path: path.into(),
            writable: false,
            max_write_bytes: None,
        }],
        ..Default::default()
    };
    let (result, _) = run_request(channel.clone(), request("/workspace")).await;
    assert_eq!(
        stdout(&result),
        "['README.md', 'big.bin', 'private', 'src']\nhello mounts\n"
    );

    // A writable mount at the root: its changes are relative to it
    let writable = ExecuteRequest {
        code: "open('/workspace/new.txt', 'w').write('hi')".into(),
        mounts: vec![Mount {
            path: "/workspace".into(),
            writable: true,
            max_write_bytes: None,
        }],
        ..Default::default()
    };
    let (result, _, changes) = exchange(channel.clone(), writable).await;
    assert!(result.success, "{}", result.error);
    let changes: Vec<_> = changes
        .into_iter()
        .map(|c| (c.mount, c.path, c.change))
        .collect();
    assert!(
        matches!(
            &changes[..],
            [(mount, path, Some(fs_change::Change::Write(w)))]
                if mount == "/workspace" && path == "new.txt" && w.data == b"hi"
        ),
        "{changes:?}"
    );

    let mut client = EryxClient::new(channel);
    let (tx, rx) = mpsc::channel(1);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(request(
            "/mnt/repo",
        )))),
    })
    .await
    .unwrap();
    let status = client.execute(ReceiverStream::new(rx)).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("/workspace"), "{status:?}");
}

/// The operator's limits apply: more mounts than `max_mounts` are refused,
/// and file operations past `max_ops` fail
#[tokio::test]
async fn the_operators_mount_limits_apply() {
    let limits = eryx_server::service::ServerLimits {
        mounts: eryx_server::mounts::MountLimits {
            max_mounts: 1,
            max_ops: 2,
            ..Default::default()
        },
        ..Default::default()
    };
    let channel = start_server_with(limits).await;

    let mut client = EryxClient::new(channel.clone());
    let (tx, rx) = mpsc::channel(1);
    let two = ["/mnt/a", "/mnt/b"].map(|path| Mount {
        path: path.into(),
        writable: false,
        max_write_bytes: None,
    });
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "print(1)".into(),
                mounts: two.to_vec(),
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();
    let status = client.execute(ReceiverStream::new(rx)).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("at most 1"), "{status:?}");

    // Reading the tree needs more than two operations: the ones past the
    // limit fail without asking the client
    let code = r"
import os
try:
    for root, _, files in os.walk('/mnt/repo'):
        for name in files:
            open(os.path.join(root, name), 'rb').read()
except OSError:
    print('limited')
";
    let (result, ops) = run(channel, Language::Python, code).await;
    assert_eq!(stdout(&result), "limited\n");
    assert_eq!(ops, 2);
}

/// A run with `tree` mounted writable at `/mnt/repo`: its result and its
/// changes, as `(path, change)` with each file's chunks joined
async fn run_writable(
    channel: Channel,
    language: Language,
    code: &str,
) -> (ExecuteResult, Vec<(String, String)>) {
    let request = ExecuteRequest {
        code: code.to_string(),
        language: language.into(),
        mounts: vec![Mount {
            path: "/mnt/repo".into(),
            writable: true,
            max_write_bytes: None,
        }],
        ..Default::default()
    };
    let (result, _, changes) = exchange(channel, request).await;
    let mut out: Vec<(String, String)> = Vec::new();
    for change in changes {
        assert_eq!(change.mount, "/mnt/repo");
        match change.change.unwrap() {
            fs_change::Change::Remove(_) => out.push((change.path, "remove".into())),
            fs_change::Change::Mkdir(_) => out.push((change.path, "mkdir".into())),
            fs_change::Change::Write(write) => {
                let data = String::from_utf8_lossy(&write.data).into_owned();
                match out.last_mut() {
                    Some((path, text)) if write.offset > 0 && *path == change.path => {
                        text.push_str(&data);
                    }
                    _ => out.push((change.path, format!("write {data}"))),
                }
            }
        }
    }
    (result, out)
}

#[tokio::test]
async fn python_changes_a_writable_mount() {
    let channel = start_server().await;
    let code = r"
import errno, os, shutil
with open('/mnt/repo/README.md', 'a') as f:
    f.write('more\n')
open('/mnt/repo/src/main.py', 'w').write('print(2)\n')
os.makedirs('/mnt/repo/out/logs')
open('/mnt/repo/out/logs/run.txt', 'w').write('ok')
os.remove('/mnt/repo/big.bin')
os.rename('/mnt/repo/src/main.py', '/mnt/repo/main.py')
try:
    os.rename('/mnt/repo/src', '/mnt/repo/lib')
except OSError as e:
    print('exdev', e.errno == errno.EXDEV)
shutil.move('/mnt/repo/src', '/mnt/repo/lib')
print(sorted(os.listdir('/mnt/repo')))
print(open('/mnt/repo/README.md').read(), end='')
";
    let (result, changes) = run_writable(channel, Language::Python, code).await;
    assert_eq!(
        stdout(&result),
        "exdev True\n\
         ['README.md', 'lib', 'main.py', 'out', 'private']\n\
         hello mounts\nmore\n"
    );
    let changes: Vec<(&str, &str)> = changes
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    assert_eq!(
        changes,
        [
            ("src/main.py", "remove"),
            ("big.bin", "remove"),
            ("src", "remove"),
            ("lib", "mkdir"),
            ("out", "mkdir"),
            ("out/logs", "mkdir"),
            ("README.md", "write hello mounts\nmore\n"),
            ("main.py", "write print(2)\n"),
            ("out/logs/run.txt", "write ok"),
        ]
    );
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn the_shell_changes_a_writable_mount() {
    let channel = start_server().await;
    let code =
        "echo hi > /mnt/repo/new.txt; rm /mnt/repo/README.md; mkdir /mnt/repo/d; ls /mnt/repo";
    let (result, changes) = run_writable(channel, Language::Shell, code).await;
    assert_eq!(stdout(&result), "big.bin\nd\nnew.txt\nprivate\nsrc\n");
    assert_eq!(
        changes,
        [
            ("README.md".into(), "remove".into()),
            ("d".into(), "mkdir".into()),
            ("new.txt".into(), "write hi\n".into()),
        ]
    );
}

/// A failed run's changes still come, as far as it got
#[tokio::test]
async fn a_failed_runs_changes_still_come() {
    let channel = start_server().await;
    let code = "open('/mnt/repo/partial.txt', 'w').write('half')\nraise SystemExit(1)";
    let (result, changes) = run_writable(channel, Language::Python, code).await;
    assert!(!result.success);
    assert_eq!(changes, [("partial.txt".into(), "write half".into())]);
}

#[tokio::test]
async fn bad_mounts_are_refused() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);
    let (tx, rx) = mpsc::channel(1);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "print(1)".into(),
                mounts: vec![Mount {
                    path: "/etc".into(),
                    writable: false,
                    max_write_bytes: None,
                }],
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();
    let status = client.execute(ReceiverStream::new(rx)).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("/mnt/"), "{status:?}");
}

/// Run `code` with `tree` mounted writable at `/mnt/repo` with a write
/// budget of `budget`, returning its output and changes
async fn run_budgeted(channel: Channel, budget: u64, code: &str) -> (ExecuteResult, Vec<FsChange>) {
    let request = ExecuteRequest {
        code: code.to_string(),
        mounts: vec![Mount {
            path: "/mnt/repo".into(),
            writable: true,
            max_write_bytes: Some(budget),
        }],
        ..Default::default()
    };
    let (result, _, changes) = exchange(channel, request).await;
    (result, changes)
}

/// A mount's write budget fails writes past it when they're made, but its
/// files stay readable and deletable. As with the server's limit, a
/// positional write fails with ENOSPC and a write through a stream with EIO
/// (see `vfs_correctness`).
#[tokio::test]
async fn a_mounts_write_budget_applies() {
    let channel = start_server().await;
    let code = r"
import errno, os
def write(path, data, mode='w'):
    try:
        with open(path, mode) as f:
            f.write(data)
        print('wrote', path)
    except OSError as e:
        print('full', path, errno.errorcode[e.errno])
write('/mnt/repo/a.txt', 'x' * 600)
write('/mnt/repo/b.txt', 'x' * 600)
fd = os.open('/mnt/repo/c.bin', os.O_CREAT | os.O_WRONLY)
try:
    os.pwrite(fd, b'x' * 600, 0)
except OSError as e:
    print('full', errno.errorcode[e.errno])
os.close(fd)
os.remove('/mnt/repo/c.bin')
# Writing part of a file the client served copies it whole
write('/mnt/repo/big.bin', b'y', 'r+b')
print(open('/mnt/repo/README.md').read(), end='')
# Deleting what the run wrote gives its space back
os.remove('/mnt/repo/a.txt')
write('/mnt/repo/b.txt', 'x' * 600)
os.remove('/mnt/repo/src/main.py')
";
    let (result, changes) = run_budgeted(channel.clone(), 1000, code).await;
    assert_eq!(
        stdout(&result),
        "wrote /mnt/repo/a.txt\n\
         full /mnt/repo/b.txt EIO\n\
         full ENOSPC\n\
         full /mnt/repo/big.bin EIO\n\
         hello mounts\n\
         wrote /mnt/repo/b.txt\n"
    );
    let paths: Vec<_> = changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["src/main.py", "b.txt"]);

    // With nothing left, the code still reads and deletes, and deleting what
    // it wrote gives the space back
    let code = r"
import errno, os
print(open('/mnt/repo/README.md').read(), end='')
for write in [lambda: open('/mnt/repo/new.txt', 'w'), lambda: os.mkdir('/mnt/repo/d')]:
    try:
        write()
    except OSError as e:
        print('full', errno.errorcode[e.errno])
os.remove('/mnt/repo/big.bin')
";
    let (result, changes) = run_budgeted(channel.clone(), 0, code).await;
    assert_eq!(stdout(&result), "hello mounts\nfull ENOSPC\nfull ENOSPC\n");
    let paths: Vec<_> = changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["big.bin"]);
}

/// A budget can't raise the server's limit
#[tokio::test]
async fn a_write_budget_only_lowers_the_servers_limit() {
    let limits = eryx_server::service::ServerLimits {
        mounts: eryx_server::mounts::MountLimits {
            max_write_bytes: 4096,
            ..Default::default()
        },
        ..Default::default()
    };
    let channel = start_server_with(limits).await;
    let code = r"
import errno
try:
    with open('/mnt/repo/large.txt', 'w') as f:
        f.write('x' * 8192)
except OSError as e:
    print('full', errno.errorcode[e.errno])
";
    let (result, _) = run_budgeted(channel.clone(), 1 << 30, code).await;
    assert_eq!(stdout(&result), "full EIO\n");

    // And it's only for writable mounts
    let mut client = EryxClient::new(channel);
    let (tx, rx) = mpsc::channel(1);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "print(1)".into(),
                mounts: vec![Mount {
                    path: "/mnt/repo".into(),
                    writable: false,
                    max_write_bytes: Some(1),
                }],
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();
    let status = client.execute(ReceiverStream::new(rx)).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("writable"), "{status:?}");
}
