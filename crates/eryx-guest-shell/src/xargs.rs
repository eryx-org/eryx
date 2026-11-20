//! The `xargs` builtin. uutils' xargs runs its command with `std::process`,
//! which WASI doesn't have, so the shell does it, as for `env`: each command
//! line goes through the shell's own lookup (builtins such as `grep`, host
//! commands, `sh`/`bash`, scripts), in a copy of the shell.
//!
//! GNU-compatible: `-0`, `-d`, `-a`, `-n`, `-L`/`-l`, `-I`/`-i`, `-r`, `-s`,
//! `-x`, `-t`, `-E`/`-e` and `-P`, with GNU's quoting in the default
//! (blank-separated) mode and its exit statuses. `-p` is refused: there's no
//! terminal to ask on.

use std::future::Future;
use std::io::Write as _;
use std::pin::Pin;

use brush_core::commands::ShellForCommand;
use brush_core::{
    ExecutionContext, ExecutionExitCode, ExecutionResult, ShellExtensions, builtins, error,
};

const USAGE: &str = "usage: xargs [-0rtx] [-a FILE] [-d DELIM] [-E EOF] [-I REPLACE] [-L MAX] \
                     [-n MAX] [-P MAX] [-s MAX] [COMMAND [ARG]...]";

/// GNU's default for `-s`: the most bytes of command line (each argument
/// and its terminating NUL) one run gets.
const DEFAULT_MAX_CHARS: usize = 128 * 1024;
/// The host's limit on a child's argv and env (`MAX_ARGS_BYTES` in
/// crates/eryx/src/process.rs), and what it charges per string on top of
/// its text (a 64-bit `String`'s header). Command lines are kept under it
/// as well as under `-s`, so a host command never fails to spawn for it.
const HOST_MAX_ARGS_BYTES: usize = 2 * 1024 * 1024;
const HOST_ENTRY_OVERHEAD: usize = 24;
/// Bytes kept free under the host's limit, as GNU keeps 2048 under
/// `ARG_MAX`: the shell adds to a command's environment (`_`, its path).
const HOST_HEADROOM: usize = 2048;
/// Most commands `-P` runs at once: half the host's 64 live children per
/// execution, leaving room for the commands' own children and the rest of
/// the script.
const MAX_PROCS: usize = 32;
/// What `-P 0` ("as many as possible") means. Running commands share the
/// execution's child memory pool, so this stays well under [`MAX_PROCS`].
const DEFAULT_PROCS: usize = 8;

/// How input is split into items.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Split {
    /// At blanks and newlines, with quotes and backslashes (the default).
    Blanks,
    /// At this character only, taken literally (`-0`, `-d`).
    Delimiter(char),
}

/// How many items one command line takes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Group {
    /// As many as fit.
    All,
    /// `-n`: at most this many items.
    Args(usize),
    /// `-L`: at most this many non-blank input lines.
    Lines(usize),
    /// `-I`: one input line per run, put in place of this string.
    Replace(String),
}

/// What `xargs` was asked to do.
#[derive(Debug, PartialEq, Eq)]
struct Request {
    split: Split,
    group: Group,
    /// `-E`: input ends at an item that is this.
    eof: Option<String>,
    /// `-a`: read items from this file, not stdin.
    arg_file: Option<String>,
    /// `-r`: don't run the command when there are no items.
    no_run_if_empty: bool,
    /// `-s`.
    max_chars: usize,
    /// `-x`: fail when a group of `-n`/`-L` items doesn't fit in `-s`.
    exit_on_size: bool,
    /// `-t`: print each command line to stderr before running it.
    trace: bool,
    /// `-P`: how many commands run at once.
    procs: usize,
    command: Vec<String>,
}

impl Default for Request {
    fn default() -> Self {
        Self {
            split: Split::Blanks,
            group: Group::All,
            eof: None,
            arg_file: None,
            no_run_if_empty: false,
            max_chars: DEFAULT_MAX_CHARS,
            exit_on_size: false,
            trace: false,
            procs: 1,
            command: Vec::new(),
        }
    }
}

fn number(opt: &str, value: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| format!("invalid number \"{value}\" for -{opt} option"))
}

fn positive(opt: &str, value: &str) -> Result<usize, String> {
    match number(opt, value)? {
        0 => Err(format!("value {value} for -{opt} option should be >= 1")),
        n => Ok(n),
    }
}

/// A `-d` delimiter: one character, or a C escape (`\n`, `\t`, `\0`, `\\`,
/// `\xHH`, `\NNN` octal) as GNU takes it.
fn delimiter(spec: &str) -> Result<char, String> {
    let invalid = || format!("invalid input delimiter specification {spec}");
    let mut chars = spec.chars();
    let (Some(first), rest) = (chars.next(), chars.as_str()) else {
        return Err(invalid());
    };
    if first != '\\' || rest.is_empty() {
        return if rest.is_empty() {
            Ok(first)
        } else {
            Err(invalid())
        };
    }
    let code = match rest {
        "a" => 7,
        "b" => 8,
        "f" => 12,
        "n" => 10,
        "r" => 13,
        "t" => 9,
        "v" => 11,
        "\\" => 92,
        _ => match rest.strip_prefix('x') {
            Some(hex) => u32::from_str_radix(hex, 16).map_err(|_| invalid())?,
            None => u32::from_str_radix(rest, 8).map_err(|_| invalid())?,
        },
    };
    u8::try_from(code).map(char::from).map_err(|_| invalid())
}

/// Parse `xargs`' arguments as GNU's getopt does: options cluster (`-0rn1`),
/// a required value is the rest of the cluster or the next argument, and
/// `-i`, `-e` and `-l` take theirs only attached. Options end at the
/// command.
fn parse(args: &[String]) -> Result<Request, String> {
    let mut r = Request::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            let mut required = || -> Result<String, String> {
                match value {
                    Some(v) => Ok(v.to_owned()),
                    None => args
                        .next()
                        .cloned()
                        .ok_or(format!("option '--{name}' requires an argument")),
                }
            };
            match name {
                "null" => r.split = Split::Delimiter('\0'),
                "no-run-if-empty" => r.no_run_if_empty = true,
                "verbose" => r.trace = true,
                "exit" => r.exit_on_size = true,
                "interactive" => return Err("-p (--interactive) isn't supported".into()),
                "delimiter" => r.split = Split::Delimiter(delimiter(&required()?)?),
                "arg-file" => r.arg_file = Some(required()?),
                "max-args" => r.group = Group::Args(positive("n", &required()?)?),
                "max-chars" => r.max_chars = positive("s", &required()?)?,
                "max-procs" => r.procs = number("P", &required()?)?,
                "max-lines" => r.group = Group::Lines(positive("L", value.unwrap_or("1"))?),
                "replace" => r.group = Group::Replace(value.unwrap_or("{}").to_owned()),
                "eof" => r.eof = value.filter(|v| !v.is_empty()).map(str::to_owned),
                _ => return Err(format!("unrecognized option '{arg}'")),
            }
            continue;
        }
        let Some(cluster) = arg.strip_prefix('-').filter(|c| !c.is_empty()) else {
            r.command.push(arg.clone());
            break;
        };
        for (i, opt) in cluster.char_indices() {
            let attached = &cluster[i + opt.len_utf8()..];
            // Options with a value take the rest of the cluster.
            let mut required = || -> Result<String, String> {
                if attached.is_empty() {
                    args.next()
                        .cloned()
                        .ok_or(format!("option requires an argument -- '{opt}'"))
                } else {
                    Ok(attached.to_owned())
                }
            };
            match opt {
                '0' => r.split = Split::Delimiter('\0'),
                'r' => r.no_run_if_empty = true,
                't' => r.trace = true,
                'x' => r.exit_on_size = true,
                'p' => return Err("-p (--interactive) isn't supported".into()),
                'd' => r.split = Split::Delimiter(delimiter(&required()?)?),
                'a' => r.arg_file = Some(required()?),
                'n' => r.group = Group::Args(positive("n", &required()?)?),
                'L' => r.group = Group::Lines(positive("L", &required()?)?),
                'I' => r.group = Group::Replace(required()?),
                's' => r.max_chars = positive("s", &required()?)?,
                'P' => r.procs = number("P", &required()?)?,
                'E' => r.eof = Some(required()?).filter(|v| !v.is_empty()),
                'l' => {
                    let n = if attached.is_empty() { "1" } else { attached };
                    r.group = Group::Lines(positive("l", n)?);
                }
                'i' => {
                    let rep = if attached.is_empty() { "{}" } else { attached };
                    r.group = Group::Replace(rep.to_owned());
                }
                'e' => r.eof = Some(attached.to_owned()).filter(|v| !v.is_empty()),
                _ => return Err(format!("invalid option -- '{opt}'")),
            }
            if matches!(
                opt,
                'd' | 'a' | 'n' | 'L' | 'I' | 's' | 'P' | 'E' | 'l' | 'i' | 'e'
            ) {
                break;
            }
        }
    }
    r.command.extend(args.cloned());
    if r.command.is_empty() {
        r.command.push("echo".into());
    }
    r.procs = match r.procs {
        0 => DEFAULT_PROCS,
        n => n.min(MAX_PROCS),
    };
    Ok(r)
}

/// Split input into lines of items. With [`Split::Blanks`], items end at
/// unquoted blanks and newlines, `'...'`, `"..."` and `\x` quote, and a line
/// ending in a blank goes on into the next (for `-L`); with `-I`
/// (`whole_lines`) blanks don't end an item, but leading ones are dropped.
/// With a delimiter, each item is a line of its own. Input ends early at an
/// item that is `eof` (not with a delimiter, as GNU).
fn split(text: &str, r: &Request) -> Result<Vec<Vec<String>>, String> {
    if let Split::Delimiter(d) = r.split {
        let mut items: Vec<&str> = text.split(d).collect();
        // A final delimiter ends the last item; it doesn't start an empty one.
        if items.last() == Some(&"") {
            items.pop();
        }
        return Ok(items.into_iter().map(|i| vec![i.to_owned()]).collect());
    }
    let whole_lines = matches!(r.group, Group::Replace(_));
    let mut lines = Vec::new();
    let mut line = Vec::new();
    let mut item = String::new();
    // Whether `item` has started (a quoted empty string is an item).
    let mut have = false;
    // Whether the last character was an unquoted blank.
    let mut after_blank = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        let blank = c == ' ' || c == '\t';
        if c == '\n' || (blank && !whole_lines) {
            if have {
                if r.eof.as_deref() == Some(item.as_str()) {
                    break;
                }
                line.push(std::mem::take(&mut item));
                have = false;
            }
            if c == '\n' && !line.is_empty() && !after_blank {
                lines.push(std::mem::take(&mut line));
            }
            after_blank = blank;
            continue;
        }
        after_blank = false;
        match c {
            _ if blank && !have => {}
            '\'' | '"' => {
                have = true;
                loop {
                    match chars.next() {
                        Some(q) if q == c => break,
                        Some('\n') | None => {
                            let kind = if c == '"' { "double" } else { "single" };
                            return Err(format!(
                                "unmatched {kind} quote; by default quotes are special to \
                                 xargs unless you use the -0 option"
                            ));
                        }
                        Some(x) => item.push(x),
                    }
                }
            }
            '\\' => {
                have = true;
                if let Some(x) = chars.next() {
                    item.push(x);
                }
            }
            c => {
                have = true;
                item.push(c);
            }
        }
    }
    if have && r.eof.as_deref() != Some(item.as_str()) {
        line.push(item);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    Ok(lines)
}

/// Lay the items out as command lines, each under both `-s` (GNU's measure:
/// each argument plus a NUL) and the host's argv limit, less `env_bytes`.
fn command_lines(
    lines: Vec<Vec<String>>,
    r: &Request,
    env_bytes: usize,
) -> Result<Vec<Vec<String>>, String> {
    let too_long = || "argument line too long".to_owned();
    let gnu = |a: &String| a.len() + 1;
    let host = |a: &String| a.len() + HOST_ENTRY_OVERHEAD;
    let host_max = HOST_MAX_ARGS_BYTES.saturating_sub(env_bytes + HOST_HEADROOM);
    let fits = |argv: &[String]| {
        argv.iter().map(gnu).sum::<usize>() <= r.max_chars
            && argv.iter().map(host).sum::<usize>() <= host_max
    };
    if let Group::Replace(rep) = &r.group {
        // One run per line, in place of `rep` (and `-x` as GNU implies).
        return lines
            .into_iter()
            .map(|line| {
                let item = line.concat();
                let argv: Vec<String> = r.command.iter().map(|a| a.replace(rep, &item)).collect();
                if fits(&argv) {
                    Ok(argv)
                } else {
                    Err(too_long())
                }
            })
            .collect();
    }
    if !fits(&r.command) {
        return Err(too_long());
    }
    let (units, per): (Vec<Vec<String>>, usize) = match r.group {
        Group::Lines(n) => (lines, n),
        Group::Args(n) => (lines.into_iter().flatten().map(|i| vec![i]).collect(), n),
        _ => (
            lines.into_iter().flatten().map(|i| vec![i]).collect(),
            usize::MAX,
        ),
    };
    if units.is_empty() {
        return Ok(if r.no_run_if_empty {
            vec![]
        } else {
            vec![r.command.clone()]
        });
    }
    let base = r.command.len();
    let (base_gnu, base_host) = (
        r.command.iter().map(gnu).sum::<usize>(),
        r.command.iter().map(host).sum::<usize>(),
    );
    let mut out = Vec::new();
    let mut argv = r.command.clone();
    let (mut used_gnu, mut used_host, mut taken) = (base_gnu, base_host, 0);
    for unit in units {
        for item in unit {
            if used_gnu + gnu(&item) > r.max_chars || used_host + host(&item) > host_max {
                if argv.len() == base || (r.exit_on_size && per != usize::MAX) {
                    return Err(too_long());
                }
                out.push(std::mem::replace(&mut argv, r.command.clone()));
                (used_gnu, used_host, taken) = (base_gnu, base_host, 0);
            }
            used_gnu += gnu(&item);
            used_host += host(&item);
            argv.push(item);
        }
        taken += 1;
        if taken == per {
            out.push(std::mem::replace(&mut argv, r.command.clone()));
            (used_gnu, used_host, taken) = (base_gnu, base_host, 0);
        }
    }
    if argv.len() > base {
        out.push(argv);
    }
    Ok(out)
}

/// A started command line: its command's name and its result.
type Running =
    Pin<Box<dyn Future<Output = (String, Result<ExecutionResult, brush_core::Error>)> + Send>>;

/// Wait for the first of `running` to finish, and take it out.
async fn next_done(
    running: &mut Vec<Running>,
) -> (String, Result<ExecutionResult, brush_core::Error>) {
    std::future::poll_fn(|cx| {
        let done =
            running
                .iter_mut()
                .enumerate()
                .find_map(|(i, run)| match run.as_mut().poll(cx) {
                    std::task::Poll::Ready(done) => Some((i, done)),
                    std::task::Poll::Pending => None,
                });
        done.map_or(std::task::Poll::Pending, |(i, done)| {
            drop(running.swap_remove(i));
            std::task::Poll::Ready(done)
        })
    })
    .await
}

pub(crate) struct XargsCommand {
    args: Vec<String>,
}

impl builtins::ParseArgs for XargsCommand {
    fn content(
        name: &str,
        content_type: builtins::ContentType,
        _options: &builtins::ContentOptions,
    ) -> Result<String, brush_core::Error> {
        match content_type {
            builtins::ContentType::ManPage => error::unimp("no man page"),
            _ => Ok(format!(
                "{name} - build and run commands from standard input"
            )),
        }
    }

    fn parse(invocation: builtins::Invocation<'_>) -> Result<Self, builtins::UsageError> {
        let args = invocation
            .into_args()
            .iter()
            .map(ToString::to_string)
            .collect();
        Ok(Self { args })
    }
}

impl<SE: ShellExtensions> builtins::Builtin<SE> for XargsCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let fail = |context: &ExecutionContext<'_, SE>,
                    e: &str|
         -> Result<ExecutionResult, brush_core::Error> {
            writeln!(context.stderr(), "xargs: {e}")?;
            Ok(ExecutionResult::new(1))
        };
        let r = match parse(&self.args) {
            Ok(r) => r,
            Err(e) => return fail(&context, &format!("{e}\n{USAGE}")),
        };
        // ponytail: reads all input before the first run, so an endless
        // producer (`yes | xargs`) never starts one; read per command line
        // if that matters.
        let input = match &r.arg_file {
            Some(path) => match crate::input::read_file(&context, path) {
                Ok(input) => input,
                Err(e) => return fail(&context, &format!("{path}: {e}")),
            },
            None => crate::input::read_stdin(&context).await?,
        };
        let shell = &*context.shell;
        let env_bytes = shell
            .env()
            .iter_exported()
            .map(|(name, var)| {
                name.len() + var.value().to_cow_str(shell).len() + 2 * HOST_ENTRY_OVERHEAD
            })
            .sum();
        let lines = match split(&String::from_utf8_lossy(&input), &r) {
            Ok(lines) => lines,
            Err(e) => return fail(&context, &e),
        };
        let argvs = match command_lines(lines, &r, env_bytes) {
            Ok(argvs) => argvs,
            Err(e) => return fail(&context, &e),
        };
        run(context, argvs, &r).await
    }
}

/// Run each command line, `r.procs` at a time, and give GNU's status: 0, or
/// 123 if any exited 1-125, or stop at the first that exited 255 (124), was
/// killed (125: brush reports a command ended by a signal as 128 plus its
/// number) or couldn't be found or run (127, 126). Once stopping, the runs
/// already started finish first.
async fn run<SE: ShellExtensions>(
    context: ExecutionContext<'_, SE>,
    argvs: Vec<Vec<String>>,
    r: &Request,
) -> Result<ExecutionResult, brush_core::Error> {
    let mut argvs = argvs.into_iter();
    let mut running: Vec<Running> = Vec::new();
    let mut failed = false;
    let mut stop = None;
    loop {
        while stop.is_none() && running.len() < r.procs {
            let Some(argv) = argvs.next() else { break };
            if r.trace {
                writeln!(context.stderr(), "{}", argv.join(" "))?;
            }
            let name = argv[0].clone();
            // Children get our stdin: read to its end, or untouched with
            // `-a`, as GNU leaves it.
            let shell = ShellForCommand::OwnedShell {
                target: Box::new(context.shell.clone()),
                parent: &mut *context.shell,
            };
            let started = crate::env::start_program(shell, context.params.clone(), &argv).await;
            running.push(Box::pin(async move {
                let result = match started {
                    Ok(started) => started.wait().await.map(Into::into),
                    Err(e) => Err(e),
                };
                (name, result)
            }));
        }
        if running.is_empty() {
            break;
        }
        let (name, result) = next_done(&mut running).await;
        let code = match result {
            Ok(result) => u8::from(result.exit_code),
            Err(e) => {
                writeln!(context.stderr(), "xargs: {e}")?;
                u8::from(ExecutionExitCode::from(&e))
            }
        };
        match code {
            0 => {}
            1..=125 | 128 => failed = true,
            126 | 127 => {
                stop.get_or_insert(code);
            }
            255 => {
                writeln!(
                    context.stderr(),
                    "xargs: {name}: exited with status 255; aborting"
                )?;
                stop.get_or_insert(124);
            }
            signal => {
                writeln!(
                    context.stderr(),
                    "xargs: {name}: terminated by signal {}",
                    signal - 128
                )?;
                stop.get_or_insert(125);
            }
        }
    }
    Ok(ExecutionResult::new(stop.unwrap_or(if failed {
        123
    } else {
        0
    })))
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_MAX_CHARS, DEFAULT_PROCS, Group, HOST_ENTRY_OVERHEAD, HOST_HEADROOM,
        HOST_MAX_ARGS_BYTES, MAX_PROCS, Request, Split, command_lines, delimiter, parse, split,
    };

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    fn strs(lines: &[&[&str]]) -> Vec<Vec<String>> {
        lines
            .iter()
            .map(|l| l.iter().map(|s| (*s).to_owned()).collect())
            .collect()
    }

    #[test]
    fn parses_gnu_options() {
        let r = parse(&args("-0rtx -n2 -s 100 -P0 grep -n x")).unwrap();
        assert_eq!(
            r,
            Request {
                split: Split::Delimiter('\0'),
                group: Group::Args(2),
                no_run_if_empty: true,
                trace: true,
                exit_on_size: true,
                max_chars: 100,
                procs: DEFAULT_PROCS,
                command: args("grep -n x"),
                ..Request::default()
            }
        );
        assert_eq!(parse(&[]).unwrap().command, args("echo"));
        assert_eq!(
            parse(&args("-i")).unwrap().group,
            Group::Replace("{}".into())
        );
        assert_eq!(
            parse(&args("-I X")).unwrap().group,
            Group::Replace("X".into())
        );
        assert_eq!(
            parse(&args("-IX")).unwrap().group,
            Group::Replace("X".into())
        );
        assert_eq!(parse(&args("-l")).unwrap().group, Group::Lines(1));
        assert_eq!(parse(&args("-L 3")).unwrap().group, Group::Lines(3));
        assert_eq!(parse(&args("-E END")).unwrap().eof.as_deref(), Some("END"));
        assert_eq!(parse(&args("-a f")).unwrap().arg_file.as_deref(), Some("f"));
        assert_eq!(parse(&args("-d ,")).unwrap().split, Split::Delimiter(','));
        assert_eq!(parse(&args("-P 4")).unwrap().procs, 4);
        assert_eq!(parse(&args("-P 99")).unwrap().procs, MAX_PROCS);
        let long = parse(&args("--null --max-args=3 --replace --max-procs 2 -- -x")).unwrap();
        assert_eq!(long.split, Split::Delimiter('\0'));
        assert_eq!(long.group, Group::Replace("{}".into()));
        assert_eq!(long.procs, 2);
        assert_eq!(long.command, args("-x"));
        for bad in ["-n", "-n 0", "-n x", "-q", "-p", "-d ab", "--nope"] {
            assert!(parse(&args(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn delimiters_take_c_escapes() {
        assert_eq!(delimiter("\\n"), Ok('\n'));
        assert_eq!(delimiter("\\0"), Ok('\0'));
        assert_eq!(delimiter("\\x41"), Ok('A'));
        assert_eq!(delimiter("\\101"), Ok('A'));
        assert_eq!(delimiter("\\"), Ok('\\'));
        assert!(delimiter("").is_err());
        assert!(delimiter("\\q").is_err());
    }

    #[test]
    fn blanks_split_with_gnu_quoting() {
        let r = Request::default();
        assert_eq!(
            split("a 'b c' \"d e\" f\\ g\n\n  h\t''\n", &r).unwrap(),
            strs(&[&["a", "b c", "d e", "f g"], &["h", ""]])
        );
        assert!(split("'open\n", &r).is_err());
        // A trailing blank continues the line (for -L).
        assert_eq!(split("a \nb\nc", &r).unwrap(), strs(&[&["a", "b"], &["c"]]));
        let eof = Request {
            eof: Some("END".into()),
            ..Request::default()
        };
        assert_eq!(split("a END b\nc", &eof).unwrap(), strs(&[&["a"]]));
    }

    #[test]
    fn replace_mode_takes_whole_lines() {
        let r = Request {
            group: Group::Replace("{}".into()),
            ..Request::default()
        };
        assert_eq!(
            split("  a b \n'c  d'\n\n", &r).unwrap(),
            strs(&[&["a b "], &["c  d"]])
        );
    }

    #[test]
    fn delimiters_keep_empty_items() {
        let r = Request {
            split: Split::Delimiter('\0'),
            ..Request::default()
        };
        assert_eq!(
            split("a b\0\0c\0", &r).unwrap(),
            strs(&[&["a b"], &[""], &["c"]])
        );
        assert!(split("", &r).unwrap().is_empty());
    }

    fn lines_for(r: &Request, input: &str) -> Result<Vec<Vec<String>>, String> {
        command_lines(split(input, r).unwrap(), r, 0)
    }

    #[test]
    fn groups_and_empty_input() {
        let mut r = Request {
            command: args("cmd"),
            ..Request::default()
        };
        assert_eq!(
            lines_for(&r, "a b\nc").unwrap(),
            strs(&[&["cmd", "a", "b", "c"]])
        );
        assert_eq!(lines_for(&r, "").unwrap(), strs(&[&["cmd"]]));
        r.no_run_if_empty = true;
        assert!(lines_for(&r, "").unwrap().is_empty());
        r.group = Group::Args(2);
        assert_eq!(
            lines_for(&r, "a b\nc").unwrap(),
            strs(&[&["cmd", "a", "b"], &["cmd", "c"]])
        );
        r.group = Group::Lines(1);
        assert_eq!(
            lines_for(&r, "a b\nc").unwrap(),
            strs(&[&["cmd", "a", "b"], &["cmd", "c"]])
        );
        r.group = Group::Replace("{}".into());
        r.command = args("cp {} {}.bak");
        r.no_run_if_empty = false;
        assert_eq!(
            lines_for(&r, "x y\nz").unwrap(),
            strs(&[&["cp", "x y", "x y.bak"], &["cp", "z", "z.bak"]])
        );
        assert!(lines_for(&r, "").unwrap().is_empty());
    }

    #[test]
    fn command_lines_split_at_the_size_limits() {
        // "cmd\0" + 3 * "aaa\0" = 16 bytes.
        let mut r = Request {
            command: args("cmd"),
            max_chars: 16,
            ..Request::default()
        };
        assert_eq!(
            lines_for(&r, "aaa aaa aaa aaa").unwrap(),
            strs(&[&["cmd", "aaa", "aaa", "aaa"], &["cmd", "aaa"]])
        );
        assert!(lines_for(&r, "aaaaaaaaaaaaaaaa").is_err());
        // -x: a whole -n group must fit.
        r.group = Group::Args(4);
        assert_eq!(lines_for(&r, "aaa aaa aaa aaa").unwrap().len(), 2);
        r.exit_on_size = true;
        assert!(lines_for(&r, "aaa aaa aaa aaa").is_err());

        // The host's limit, counting each entry's overhead and the env.
        let r = Request {
            command: args("cmd"),
            max_chars: usize::MAX,
            ..Request::default()
        };
        let item = "x".repeat(1000);
        let n = 5000;
        let input = vec![item.as_str(); n].join("\n");
        let env = 1 << 20;
        let argvs = command_lines(split(&input, &r).unwrap(), &r, env).unwrap();
        assert!(argvs.len() > 1);
        assert_eq!(argvs.iter().map(|a| a.len() - 1).sum::<usize>(), n);
        for argv in &argvs {
            let host: usize = argv.iter().map(|a| a.len() + HOST_ENTRY_OVERHEAD).sum();
            assert!(host + env + HOST_HEADROOM <= HOST_MAX_ARGS_BYTES);
        }
        assert_eq!(DEFAULT_MAX_CHARS, Request::default().max_chars);
    }
}
