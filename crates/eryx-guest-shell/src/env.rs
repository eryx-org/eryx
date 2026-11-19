//! The `env` builtin. uutils' `env` runs its command with `std::process`,
//! which WASI doesn't have, so the shell does it: `cmd` goes through the
//! shell's own lookup (builtins, host commands, `sh`/`bash`, scripts) in a
//! copy of the shell with the environment changed.

use std::io::Write as _;

use brush_core::commands::{ShellForCommand, SimpleCommand};
use brush_core::{
    CommandArg, ExecutionContext, ExecutionResult, ShellExtensions, ShellVariable, builtins, error,
};

const USAGE: &str = "usage: env [-i] [-u NAME]... [NAME=VALUE]... [COMMAND [ARG]...]";

/// What `env` was asked to do.
#[derive(Debug, Default, PartialEq, Eq)]
struct Request {
    /// `-i`: start from an empty environment.
    clear: bool,
    /// `-u NAME`s.
    unset: Vec<String>,
    /// `NAME=VALUE`s, in order.
    set: Vec<(String, String)>,
    /// The command and its arguments; empty to print the environment.
    command: Vec<String>,
}

/// Parse `env`'s arguments, GNU style but only `-i`, `-u` and `--`.
fn parse(args: &[String]) -> Result<Request, String> {
    let mut request = Request::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-i" | "-" | "--ignore-environment" => request.clear = true,
            "-u" | "--unset" => {
                let name = args.next().ok_or("option requires an argument -- 'u'")?;
                request.unset.push(name.clone());
            }
            "--" => break,
            _ if arg.starts_with("-u") => request.unset.push(arg[2..].to_owned()),
            _ if arg.starts_with('-') => return Err(format!("invalid option -- '{arg}'")),
            _ => match arg.split_once('=') {
                Some((name, value)) if !name.is_empty() => {
                    request.set.push((name.to_owned(), value.to_owned()));
                }
                _ => {
                    request.command.push(arg.clone());
                    break;
                }
            },
        }
    }
    // After `--` or the command name, everything is the command's (but
    // assignments still come before it).
    for arg in args {
        match arg.split_once('=') {
            Some((name, value)) if request.command.is_empty() && !name.is_empty() => {
                request.set.push((name.to_owned(), value.to_owned()));
            }
            _ => request.command.push(arg.clone()),
        }
    }
    Ok(request)
}

pub(crate) struct EnvCommand {
    args: Vec<String>,
}

impl builtins::ParseArgs for EnvCommand {
    fn content(
        name: &str,
        content_type: builtins::ContentType,
        _options: &builtins::ContentOptions,
    ) -> Result<String, brush_core::Error> {
        match content_type {
            builtins::ContentType::ManPage => error::unimp("no man page"),
            _ => Ok(format!("{name} - run a command in a changed environment")),
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

impl<SE: ShellExtensions> builtins::Builtin<SE> for EnvCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let request = match parse(&self.args) {
            Ok(request) => request,
            Err(e) => {
                writeln!(context.stderr(), "env: {e}\n{USAGE}")?;
                return Ok(ExecutionResult::new(125));
            }
        };
        // A copy, so nothing changed here reaches the caller.
        let mut shell = Box::new(context.shell.clone());
        let mut unset = request.unset;
        if request.clear {
            unset.extend(shell.env().iter_exported().map(|(name, _)| name.clone()));
        }
        for name in &unset {
            shell.env_mut().unset(name)?;
        }
        for (name, value) in request.set {
            let mut var = ShellVariable::new(value);
            var.export();
            shell.env_mut().set_global(name, var)?;
        }
        let Some(name) = request.command.first().cloned() else {
            let mut vars: Vec<String> = shell
                .env()
                .iter_exported()
                .map(|(name, var)| format!("{name}={}", var.value().to_cow_str(&shell)))
                .collect();
            vars.sort();
            let mut out = context.stdout();
            for var in vars {
                writeln!(out, "{var}")?;
            }
            return Ok(ExecutionResult::success());
        };
        let args = request.command.iter().map(|a| CommandArg::from(a.as_str()));
        let mut cmd = SimpleCommand::new(
            ShellForCommand::OwnedShell {
                target: shell,
                parent: context.shell,
            },
            context.params,
            name,
            args,
        );
        // As a real `env`: it runs programs, not functions.
        cmd.use_functions = false;
        Ok(cmd.execute().await?.wait().await?.into())
    }
}

#[cfg(test)]
mod tests {
    use super::{Request, parse};

    fn parse_strs(args: &[&str]) -> Result<Request, String> {
        parse(&args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_options_assignments_and_the_command() {
        let request = parse_strs(&["-i", "-u", "A", "-uB", "X=1", "cmd", "Y=2", "-i"]).unwrap();
        assert_eq!(
            request,
            Request {
                clear: true,
                unset: vec!["A".into(), "B".into()],
                set: vec![("X".into(), "1".into())],
                command: vec!["cmd".into(), "Y=2".into(), "-i".into()],
            }
        );
        assert_eq!(
            parse_strs(&["--", "-x"]).unwrap().command,
            vec!["-x".to_owned()]
        );
        assert_eq!(parse_strs(&[]).unwrap(), Request::default());
        assert!(parse_strs(&["-S", "a b"]).is_err());
        assert!(parse_strs(&["-u"]).is_err());
    }
}
