//! The `chmod` builtin. WASI has no permission bits, and the shell runs any
//! file by its `#!` line, so there is nothing to change: `chmod` checks its
//! mode and that each file exists, and succeeds, so `chmod +x x && ./x`
//! works. uutils' `chmod` is unix-only (`std::os::unix::fs`, uucore's
//! `perms`), so it isn't in the embedded coreutils.

use std::io::Write as _;

use brush_core::{ExecutionContext, ExecutionResult, ShellExtensions, builtins, error};

const TRY: &str = "Try 'chmod --help' for more information.";

/// What `chmod` was asked to do: `files`, and the mode unless `--reference`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Request {
    quiet: bool,
    reference: Option<String>,
    operands: Vec<String>,
}

/// Parse GNU `chmod`'s options. `-x`, `-w` and the like are modes, not
/// options: only `-R`, `-c`, `-f` and `-v` (bundled or not) are flags.
fn parse(args: &[String]) -> Result<Request, String> {
    let mut request = Request::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--" => {
                request.operands.extend(args.by_ref().cloned());
                break;
            }
            "--silent" | "--quiet" => request.quiet = true,
            "--recursive" | "--changes" | "--verbose" | "--preserve-root"
            | "--no-preserve-root" => {}
            "--reference" => {
                let file = args
                    .next()
                    .ok_or("option '--reference' requires an argument")?;
                request.reference = Some(file.clone());
            }
            _ if arg.starts_with("--reference=") => {
                request.reference = Some(arg["--reference=".len()..].to_owned());
            }
            _ if arg.starts_with("--") => return Err(format!("unrecognized option '{arg}'")),
            _ if arg.len() > 1 && arg[1..].chars().all(|c| "Rcfv".contains(c)) => {
                request.quiet |= arg.contains('f');
            }
            _ => request.operands.push(arg.clone()),
        }
    }
    Ok(request)
}

/// Whether `mode` is a mode GNU `chmod` accepts: octal, or comma-separated
/// `[ugoa]*([-+=]([rwxXst]*|[ugo]))+` clauses.
fn valid_mode(mode: &str) -> bool {
    if !mode.is_empty() && mode.len() <= 4 && mode.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return true;
    }
    mode.split(',').all(|clause| {
        let ops = clause.trim_start_matches(['u', 'g', 'o', 'a']);
        if !ops.starts_with(['-', '+', '=']) {
            return false;
        }
        // Starts with an operator, so the first piece is empty.
        ops.split(['-', '+', '=']).skip(1).all(|perms| {
            perms.chars().all(|c| "rwxXst".contains(c)) || matches!(perms, "u" | "g" | "o")
        })
    })
}

/// An I/O error as GNU prints it: the message without Rust's `(os error N)`.
fn describe(e: &std::io::Error) -> String {
    let text = e.to_string();
    match text.find(" (os error") {
        Some(end) => text[..end].to_owned(),
        None => text,
    }
}

pub(crate) struct ChmodCommand {
    args: Vec<String>,
}

impl builtins::ParseArgs for ChmodCommand {
    fn content(
        name: &str,
        content_type: builtins::ContentType,
        _options: &builtins::ContentOptions,
    ) -> Result<String, brush_core::Error> {
        match content_type {
            builtins::ContentType::ManPage => error::unimp("no man page"),
            _ => Ok(format!(
                "{name} - accept a file mode change (the sandbox has no permission bits)"
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

impl<SE: ShellExtensions> builtins::Builtin<SE> for ChmodCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let mut request = match parse(&self.args) {
            Ok(request) => request,
            Err(e) => {
                writeln!(context.stderr(), "chmod: {e}\n{TRY}")?;
                return Ok(ExecutionResult::new(1));
            }
        };
        let exists = |file: &str| std::fs::metadata(context.shell.absolute_path(file)).map(|_| ());
        if let Some(reference) = &request.reference {
            if let Err(e) = exists(reference) {
                let e = describe(&e);
                writeln!(
                    context.stderr(),
                    "chmod: failed to get attributes of '{reference}': {e}"
                )?;
                return Ok(ExecutionResult::new(1));
            }
        } else if !request.operands.is_empty() {
            let mode = request.operands.remove(0);
            if !valid_mode(&mode) {
                writeln!(context.stderr(), "chmod: invalid mode: '{mode}'\n{TRY}")?;
                return Ok(ExecutionResult::new(1));
            }
            if request.operands.is_empty() {
                writeln!(
                    context.stderr(),
                    "chmod: missing operand after '{mode}'\n{TRY}"
                )?;
                return Ok(ExecutionResult::new(1));
            }
        }
        if request.operands.is_empty() {
            writeln!(context.stderr(), "chmod: missing operand\n{TRY}")?;
            return Ok(ExecutionResult::new(1));
        }
        let mut code = 0;
        for file in &request.operands {
            if let Err(e) = exists(file) {
                code = 1;
                if !request.quiet {
                    let e = describe(&e);
                    writeln!(context.stderr(), "chmod: cannot access '{file}': {e}")?;
                }
            }
        }
        Ok(ExecutionResult::new(code))
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, valid_mode};

    fn operands(args: &[&str]) -> Vec<String> {
        parse(&args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
            .unwrap()
            .operands
    }

    #[test]
    fn negative_modes_are_operands_and_flags_are_not() {
        assert_eq!(operands(&["-R", "-x", "a"]), ["-x", "a"]);
        assert_eq!(operands(&["-Rf", "755", "a", "-v"]), ["755", "a"]);
        assert_eq!(operands(&["--", "-R", "a"]), ["-R", "a"]);
        assert!(parse(&["-fv".to_owned()]).unwrap().quiet);
        assert!(parse(&["--nope".to_owned()]).is_err());
    }

    #[test]
    fn modes_are_octal_or_symbolic() {
        for mode in [
            "755",
            "0644",
            "+x",
            "u+x",
            "a-w",
            "go=",
            "u=rwx,g=rx,o=",
            "u+x-w",
            "g=u",
        ] {
            assert!(valid_mode(mode), "{mode}");
        }
        for mode in ["", "8", "12345", "x", "u", "u+q", "+x,", "u+ug"] {
            assert!(!valid_mode(mode), "{mode}");
        }
    }
}
