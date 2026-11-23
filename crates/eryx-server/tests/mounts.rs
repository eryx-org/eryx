//! End-to-end tests for client-served mounts (`ExecuteRequest.mounts`): the
//! test plays the client, answering the server's `FsRequest`s from a small
//! in-memory tree.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use eryx::{PoolConfig, Sandbox, SandboxPool};
use eryx_server::proto::eryx::v1::eryx_client::EryxClient;
use eryx_server::proto::eryx::v1::{
    ClientMessage, ExecuteRequest, ExecuteResult, FileKind, FsEntry, FsError, FsErrorCode,
    FsListing, FsMetadata, FsResponse, Language, Mount, SupportingFile, client_message, fs_request,
    fs_response, server_message,
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
        }],
        ..Default::default()
    };
    run_request(channel, request).await
}

/// Send `request`, answering the server's file operations from `tree`.
async fn run_request(channel: Channel, request: ExecuteRequest) -> (ExecuteResult, usize) {
    let mut client = EryxClient::new(channel);
    let (tx, rx) = mpsc::channel(16);
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
    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::FsRequest(req)) => {
                ops += 1;
                assert_eq!(req.mount, "/mnt/repo");
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
            Some(server_message::Message::ExecuteResult(result)) => return (*result, ops),
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
    let two = ["/mnt/a", "/mnt/b"].map(|path| Mount { path: path.into() });
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
