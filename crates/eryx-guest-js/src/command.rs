//! The `run-command` export: `node ARGS...` (or `js`) on a fresh instance,
//! reading and writing the real fds 0/1/2, as a shell runs it.
//!
//! - `node -e CODE` / `--eval`, `node -p CODE` / `--print` (prints the
//!   value), `node FILE [ARGS...]`, and `node -` or no program (the code
//!   from stdin); `-v` / `--version` and `-h` / `--help`. Any other option
//!   exits 9, as Node does.
//! - `process.argv` (`[execPath, script, ...args]`), `process.env`,
//!   `process.cwd()`, `process.exit(n)` and `process.exitCode`,
//!   `process.stdout.write`; and the main module's CommonJS names
//!   (`require`, `module`, `__filename`, ...) with `require` of the
//!   sandbox's files (`src/command.js`).
//! - The program runs as an execute's code does (global code, leading
//!   `import`s and top-level `await`), and the command waits for its timers
//!   and un-awaited work in the same event loop (`js::poll`). An uncaught
//!   error or unhandled rejection prints to stderr and exits 1.

use std::cell::Cell;
use std::io::Read;

use rquickjs::context::EvalOptions;
use rquickjs::{Ctx, Exception, Function, Object, Value};

const COMMAND_JS: &str = include_str!("command.js");

/// Node's exit status for a bad command line.
const USAGE: i32 = 9;

thread_local! {
    /// The status `process.exit` asked for, once it has.
    static EXIT: Cell<Option<i32>> = const { Cell::new(None) };
}

/// The status `process.exit` asked for, if it has: the event loop stops.
pub(crate) fn exit_requested() -> Option<i32> {
    EXIT.get()
}

/// What `node ARGS` runs.
#[derive(Debug, PartialEq, Eq)]
enum Program {
    /// `-e CODE`, or with `print`, `-p CODE`.
    Eval {
        code: String,
        print: bool,
    },
    /// A script file.
    File(String),
    /// The code on stdin (`-`, or no program).
    Stdin,
    Version,
    Help,
}

/// `node ARGS` (`args[0]` is the command's name) as the program and
/// `process.argv` after `execPath`. Like Node, options end at the first
/// argument that isn't one, and a later `-e`/`-p` replaces an earlier one.
fn parse(args: &[String]) -> Result<(Program, Vec<String>), String> {
    let mut eval: Option<(String, bool)> = None;
    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-e" | "--eval" | "-p" | "--print" => {
                let code = args
                    .get(i + 1)
                    .ok_or_else(|| format!("{arg} requires an argument"))?;
                let print = matches!(arg, "-p" | "--print");
                // `-e CODE -p` would print too; only `-p CODE` here.
                let print = print || eval.as_ref().is_some_and(|(_, p)| *p);
                eval = Some((code.clone(), print));
                i += 2;
            }
            "-v" | "--version" => return Ok((Program::Version, Vec::new())),
            "-h" | "--help" => return Ok((Program::Help, Vec::new())),
            "--" => {
                i += 1;
                break;
            }
            "-" => break,
            _ if arg.starts_with('-') => return Err(format!("bad option: {arg}")),
            _ => break,
        }
    }
    let rest = args[i.min(args.len())..].to_vec();
    if let Some((code, print)) = eval {
        return Ok((Program::Eval { code, print }, rest));
    }
    match rest.first().map(String::as_str) {
        None => Ok((Program::Stdin, rest)),
        Some("-") => Ok((Program::Stdin, rest)),
        Some(_) => Ok((Program::File(rest[0].clone()), rest)),
    }
}

/// `path` made absolute against `cwd`, with `.` and `..` resolved.
fn absolute(cwd: &str, path: &str) -> String {
    let mut parts: Vec<&str> = if path.starts_with('/') {
        Vec::new()
    } else {
        cwd.split('/').filter(|p| !p.is_empty()).collect()
    };
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

/// What the event loop runs: the code, its file name, whether to print its
/// value (`-p`), and the main script's absolute path.
pub(crate) struct Run {
    pub(crate) code: String,
    pub(crate) filename: String,
    pub(crate) print: bool,
    main: Option<String>,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    cwd: String,
}

/// Work out what `node ARGS` runs, or the status it exits with at once
/// (after writing its output or error).
pub(crate) fn prepare(
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: String,
) -> Result<Run, i32> {
    let name = args.first().map_or("node", String::as_str).to_owned();
    if let Err(e) = std::env::set_current_dir(&cwd) {
        eprintln!("{name}: can't change to {cwd:?}: {e}");
        return Err(1);
    }
    let (program, rest) = parse(&args).map_err(|e| {
        eprintln!("{name}: {e}");
        USAGE
    })?;
    let exec_path = format!("/eryx/bin/{}", name.rsplit('/').next().unwrap_or(&name));
    let mut argv = vec![exec_path];
    let (code, filename, print, main) = match program {
        Program::Version => {
            println!("{}", version());
            return Err(0);
        }
        Program::Help => {
            println!(
                "Usage: {name} [options] [ script.js ] [arguments]\n\n\
                 eryx's JavaScript ({}), run as a command.\n\n\
                 Options:\n  \
                 -e, --eval CODE   run CODE\n  \
                 -p, --print CODE  run CODE and print its value\n  \
                 -                 read the program from stdin (the default)\n  \
                 -v, --version     print the runtime's version\n  \
                 -h, --help        print this help",
                version()
            );
            return Err(0);
        }
        Program::Eval { code, print } => {
            argv.extend(rest);
            (code, "[eval]".to_owned(), print, None)
        }
        Program::Stdin => {
            argv.extend(rest);
            let mut code = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut code) {
                eprintln!("{name}: reading the program from stdin: {e}");
                return Err(1);
            }
            (code, "[stdin]".to_owned(), false, None)
        }
        Program::File(path) => {
            let script = absolute(&cwd, &path);
            let code = match std::fs::read_to_string(&script) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("Error: Cannot find module '{script}' ({e})");
                    return Err(1);
                }
            };
            argv.push(script.clone());
            argv.extend(rest.into_iter().skip(1));
            // A `#!` line becomes a comment, keeping the line numbers (the
            // import rewrite doesn't skip it).
            let code = match code.strip_prefix("#!") {
                Some(rest) => format!("//{rest}"),
                None => code,
            };
            (code, script.clone(), false, Some(script))
        }
    };
    Ok(Run {
        code,
        filename,
        print,
        main,
        argv,
        env,
        cwd,
    })
}

/// `node --version`: this isn't Node, so it says what it is.
fn version() -> String {
    format!("quickjs-ng {}", quickjs_version())
}

fn quickjs_version() -> String {
    // SAFETY: a static NUL-terminated version string.
    let version = unsafe { std::ffi::CStr::from_ptr(rquickjs::qjs::JS_GetVersion()) };
    version.to_string_lossy().into_owned()
}

/// Set up `process` and the main module's globals for `run`
/// (`src/command.js`).
pub(crate) fn install(ctx: &Ctx<'_>, run: &Run) -> rquickjs::Result<()> {
    let native = Object::new(ctx.clone())?;
    native.set("write", Function::new(ctx.clone(), write)?)?;
    native.set("exit", Function::new(ctx.clone(), exit)?)?;
    native.set("compile", Function::new(ctx.clone(), compile)?)?;
    native.set("quickjsVersion", quickjs_version())?;
    let env = Object::new(ctx.clone())?;
    for (key, value) in &run.env {
        env.set(key.as_str(), value.as_str())?;
    }
    let install: Function<'_> = ctx.eval(COMMAND_JS)?;
    install.call((
        native,
        run.argv.clone(),
        env,
        run.cwd.as_str(),
        run.main.as_deref(),
    ))
}

/// `write(fd, text)`: straight to stdout (1) or stderr (2).
pub(crate) fn write(fd: u32, text: String) {
    use std::io::Write;
    // A closed pipe (`node ... | head -1`) isn't the script's error.
    let _ = if fd == 2 {
        std::io::stderr().write_all(text.as_bytes())
    } else {
        let mut out = std::io::stdout();
        out.write_all(text.as_bytes()).and_then(|()| out.flush())
    };
}

/// `exit(code)`: record the status and throw, to unwind the script.
fn exit<'js>(ctx: Ctx<'js>, code: Value<'js>) -> rquickjs::Result<()> {
    let status = if code.is_undefined() || code.is_null() {
        0
    } else {
        // `process.exit("3")` is 3, as in Node; anything else not a number, 1.
        let number = ctx
            .globals()
            .get::<_, Function<'_>>("Number")?
            .call::<_, f64>((code,))?;
        if number.is_finite() { number as i32 } else { 1 }
    };
    EXIT.set(Some(status & 0xff));
    Err(Exception::throw_message(&ctx, "process.exit()"))
}

/// `compile(source, filename)`: evaluate a CommonJS module's wrapper.
fn compile<'js>(ctx: Ctx<'js>, source: String, filename: String) -> rquickjs::Result<Value<'js>> {
    let mut options = EvalOptions::default();
    options.strict = false;
    options.filename = Some(filename);
    ctx.eval_with_options(source, options)
}

#[cfg(test)]
mod tests {
    use super::{Program, absolute, parse};

    fn args(list: &[&str]) -> Vec<String> {
        std::iter::once("node")
            .chain(list.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn parses_like_node() {
        let eval = |code: &str, print| Program::Eval {
            code: code.to_owned(),
            print,
        };
        assert_eq!(
            parse(&args(&["-e", "1", "a", "--foo"])).unwrap(),
            (eval("1", false), vec!["a".into(), "--foo".into()])
        );
        assert_eq!(
            parse(&args(&["-e", "1", "-p", "2"])).unwrap(),
            (eval("2", true), vec![])
        );
        assert_eq!(
            parse(&args(&["-e", "1", "--", "--foo"])).unwrap(),
            (eval("1", false), vec!["--foo".into()])
        );
        assert_eq!(parse(&args(&[])).unwrap(), (Program::Stdin, vec![]));
        assert_eq!(
            parse(&args(&["-", "a"])).unwrap(),
            (Program::Stdin, vec!["-".into(), "a".into()])
        );
        assert_eq!(
            parse(&args(&["s.js", "-e"])).unwrap(),
            (
                Program::File("s.js".into()),
                vec!["s.js".into(), "-e".into()]
            )
        );
        assert_eq!(parse(&args(&["-v"])).unwrap().0, Program::Version);
        assert_eq!(
            parse(&args(&["--bogus"])).unwrap_err(),
            "bad option: --bogus"
        );
        assert_eq!(
            parse(&args(&["-e"])).unwrap_err(),
            "-e requires an argument"
        );
    }

    #[test]
    fn absolute_paths() {
        assert_eq!(absolute("/tmp", "s.js"), "/tmp/s.js");
        assert_eq!(absolute("/tmp/a", "../b/./s.js"), "/tmp/b/s.js");
        assert_eq!(absolute("/tmp", "/x.js"), "/x.js");
    }
}
