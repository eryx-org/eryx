//! The native side of `node:fs` (`src/node/fs.js`): the sandbox's files
//! through the guest's own `std::fs`, so JavaScript reaches exactly what the
//! runtime's WASI filesystem gives it (in a shell's `node`, the shell's
//! files), as Python's `open` does. Installed as the hidden global
//! `__eryx_fs`, which `fs.js` reads when it's first imported.
//!
//! Paths are strings; a number is a standard stream (`readFileSync(0)` reads
//! stdin, `writeFileSync(1, ...)` writes stdout). Failures throw an `Error`
//! with Node's `code`, `syscall` and `path` properties.

use std::io::{Read, Write};
use std::time::UNIX_EPOCH;

use rquickjs::{ArrayBuffer, Ctx, Exception, Function, Object, TypedArray, Value};

/// A path argument: a file, or one of the standard streams by number.
enum Target {
    Path(String),
    Fd(i32),
}

impl Target {
    fn of(value: &Value<'_>) -> rquickjs::Result<Self> {
        if let Some(n) = value.as_number() {
            return Ok(Self::Fd(n as i32));
        }
        match value.as_string() {
            Some(s) => Ok(Self::Path(s.to_string()?)),
            None => Err(Exception::throw_type(
                value.ctx(),
                "The \"path\" argument must be a string or a file descriptor",
            )),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Path(p) => p.clone(),
            Self::Fd(fd) => fd.to_string(),
        }
    }
}

/// Node's code and description for an I/O error.
fn code_of(e: &std::io::Error) -> (&'static str, String) {
    use std::io::ErrorKind as K;
    let known = match e.kind() {
        K::NotFound => ("ENOENT", "no such file or directory"),
        K::AlreadyExists => ("EEXIST", "file already exists"),
        K::PermissionDenied => ("EACCES", "permission denied"),
        K::NotADirectory => ("ENOTDIR", "not a directory"),
        K::IsADirectory => ("EISDIR", "illegal operation on a directory"),
        K::DirectoryNotEmpty => ("ENOTEMPTY", "directory not empty"),
        K::ReadOnlyFilesystem => ("EROFS", "read-only file system"),
        K::StorageFull => ("ENOSPC", "no space left on device"),
        K::InvalidInput => ("EINVAL", "invalid argument"),
        _ => return ("EIO", e.to_string()),
    };
    (known.0, known.1.to_string())
}

/// Throw `e` as Node does: `ENOENT: no such file or directory, open '/x'`.
fn throw_io(ctx: &Ctx<'_>, e: &std::io::Error, syscall: &str, path: &str) -> rquickjs::Error {
    let (code, description) = code_of(e);
    let message = format!("{code}: {description}, {syscall} '{path}'");
    let thrown = (|| {
        let error = Exception::from_message(ctx.clone(), &message)?;
        error.set("code", code)?;
        error.set("syscall", syscall)?;
        error.set("path", path)?;
        Ok::<_, rquickjs::Error>(ctx.throw(error.into_value()))
    })();
    thrown.unwrap_or_else(|e| e)
}

/// Only the standard streams have numbers: there's no `openSync`.
fn bad_fd(ctx: &Ctx<'_>, fd: i32, syscall: &str) -> rquickjs::Error {
    let message = format!("EBADF: bad file descriptor {fd}, {syscall}");
    Exception::throw_message(ctx, &message)
}

/// `readFile(path)`: the file's bytes, as an `ArrayBuffer`.
fn read_file<'js>(ctx: Ctx<'js>, target: Value<'js>) -> rquickjs::Result<ArrayBuffer<'js>> {
    let target = Target::of(&target)?;
    let bytes = match &target {
        Target::Fd(0) => {
            let mut bytes = Vec::new();
            std::io::stdin()
                .read_to_end(&mut bytes)
                .map_err(|e| throw_io(&ctx, &e, "read", "0"))?;
            bytes
        }
        Target::Fd(fd) => return Err(bad_fd(&ctx, *fd, "read")),
        Target::Path(path) => std::fs::read(path).map_err(|e| throw_io(&ctx, &e, "open", path))?,
    };
    ArrayBuffer::new(ctx, bytes)
}

/// `writeFile(path, bytes, append)`: write (or append) the bytes, creating
/// the file.
fn write_file<'js>(
    ctx: Ctx<'js>,
    target: Value<'js>,
    data: TypedArray<'js, u8>,
    append: bool,
) -> rquickjs::Result<()> {
    let target = Target::of(&target)?;
    // SAFETY: no JavaScript runs while the bytes are borrowed.
    let bytes = unsafe { data.as_bytes() }.unwrap_or_default();
    let written = match &target {
        Target::Fd(1) => std::io::stdout()
            .write_all(bytes)
            .and_then(|()| std::io::stdout().flush()),
        Target::Fd(2) => std::io::stderr().write_all(bytes),
        Target::Fd(fd) => return Err(bad_fd(&ctx, *fd, "write")),
        Target::Path(path) => std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .append(append)
            .truncate(!append)
            .open(path)
            .and_then(|mut file| file.write_all(bytes)),
    };
    written.map_err(|e| throw_io(&ctx, &e, "open", &target.label()))
}

/// `exists(path)`: whether the path exists (following symlinks).
fn exists(path: String) -> bool {
    std::fs::metadata(path).is_ok()
}

/// `stat(path, follow)`: `{kind, size, mtimeMs, atimeMs, birthtimeMs}`,
/// `kind` one of `file`, `dir`, `symlink` or `other`.
fn stat<'js>(ctx: Ctx<'js>, path: String, follow: bool) -> rquickjs::Result<Object<'js>> {
    let (meta, syscall) = if follow {
        (std::fs::metadata(&path), "stat")
    } else {
        (std::fs::symlink_metadata(&path), "lstat")
    };
    let meta = meta.map_err(|e| throw_io(&ctx, &e, syscall, &path))?;
    let kind = meta.file_type();
    let kind = if kind.is_symlink() {
        "symlink"
    } else if kind.is_dir() {
        "dir"
    } else if kind.is_file() {
        "file"
    } else {
        "other"
    };
    let ms = |time: std::io::Result<std::time::SystemTime>| {
        time.ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
    };
    let stats = Object::new(ctx)?;
    stats.set("kind", kind)?;
    stats.set("size", meta.len() as f64)?;
    stats.set("mtimeMs", ms(meta.modified()))?;
    stats.set("atimeMs", ms(meta.accessed()))?;
    stats.set("birthtimeMs", ms(meta.created()))?;
    Ok(stats)
}

/// `readdir(path)`: the entries' names, sorted.
fn readdir(ctx: Ctx<'_>, path: String) -> rquickjs::Result<Vec<String>> {
    let entries = std::fs::read_dir(&path).map_err(|e| throw_io(&ctx, &e, "scandir", &path))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| throw_io(&ctx, &e, "scandir", &path))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

/// `mkdir(path, recursive)`.
fn mkdir(ctx: Ctx<'_>, path: String, recursive: bool) -> rquickjs::Result<()> {
    let made = if recursive {
        std::fs::create_dir_all(&path)
    } else {
        std::fs::create_dir(&path)
    };
    made.map_err(|e| throw_io(&ctx, &e, "mkdir", &path))
}

/// `unlink(path)`: remove a file.
fn unlink(ctx: Ctx<'_>, path: String) -> rquickjs::Result<()> {
    std::fs::remove_file(&path).map_err(|e| throw_io(&ctx, &e, "unlink", &path))
}

/// `rmdir(path, recursive)`: remove a directory, with `recursive` and its
/// contents.
fn rmdir(ctx: Ctx<'_>, path: String, recursive: bool) -> rquickjs::Result<()> {
    let removed = if recursive {
        std::fs::remove_dir_all(&path)
    } else {
        std::fs::remove_dir(&path)
    };
    removed.map_err(|e| throw_io(&ctx, &e, "rmdir", &path))
}

/// `rename(from, to)`.
fn rename(ctx: Ctx<'_>, from: String, to: String) -> rquickjs::Result<()> {
    std::fs::rename(&from, &to).map_err(|e| throw_io(&ctx, &e, "rename", &from))
}

/// `copyFile(from, to)`.
fn copy_file(ctx: Ctx<'_>, from: String, to: String) -> rquickjs::Result<()> {
    std::fs::copy(&from, &to)
        .map(|_| ())
        .map_err(|e| throw_io(&ctx, &e, "copyfile", &from))
}

/// Install `__eryx_fs`, hidden like `require`.
pub(crate) fn install(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    let native = Object::new(ctx.clone())?;
    native.set("readFile", Function::new(ctx.clone(), read_file)?)?;
    native.set("writeFile", Function::new(ctx.clone(), write_file)?)?;
    native.set("exists", Function::new(ctx.clone(), exists)?)?;
    native.set("stat", Function::new(ctx.clone(), stat)?)?;
    native.set("readdir", Function::new(ctx.clone(), readdir)?)?;
    native.set("mkdir", Function::new(ctx.clone(), mkdir)?)?;
    native.set("unlink", Function::new(ctx.clone(), unlink)?)?;
    native.set("rmdir", Function::new(ctx.clone(), rmdir)?)?;
    native.set("rename", Function::new(ctx.clone(), rename)?)?;
    native.set("copyFile", Function::new(ctx.clone(), copy_file)?)?;
    ctx.globals().prop(
        "__eryx_fs",
        rquickjs::object::Property::from(native)
            .writable()
            .configurable(),
    )
}
