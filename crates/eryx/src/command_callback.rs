//! Command callbacks: host callbacks the shell runs as real commands (#1241).
//!
//! An ordinary callback run as a shell command has its arguments parsed into
//! JSON parameters (see the shell guest's `call` module). A command callback
//! instead gets its command line as given and its stdin, and returns stdout,
//! stderr and an exit code, which the shell treats like any other command's:
//! pipes, redirects, `$(...)`, `$?`.
//!
//! On the wire it is an ordinary callback with a fixed schema, marked with
//! [`COMMAND_SCHEMA_KEY`], so the guests, the callback handler, limits,
//! timeouts and replay journals need nothing of their own for it. Its
//! arguments are `{"args": [...], "stdin": "<base64>"}` and its result is
//! `{"stdout": "<base64>", "stderr": "<base64>", "exit_code": N}`. Python and
//! JavaScript see it as a callback taking `args` and `stdin` and returning
//! that object.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::callback::{Callback, CallbackError};
use crate::schema::Schema;

/// The schema key that marks a callback as a command callback: the shell
/// guest runs a callback whose parameters schema has it set to `true` as a
/// command.
pub const COMMAND_SCHEMA_KEY: &str = "x-eryx-command";

/// Most bytes of stdin a command callback is given. The shell stops reading
/// past this and fails the command (exit 1) without calling it. Must match the
/// shell guest's `MAX_COMMAND_STDIN`.
pub const MAX_COMMAND_STDIN_BYTES: usize = 8 * 1024 * 1024;

/// Most bytes of stdout and stderr together a command callback may return; a
/// larger result fails the command (exit 1) with nothing written.
pub const MAX_COMMAND_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// What a command callback is run with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandInput {
    /// The arguments after the command's name, exactly as the shell expanded
    /// them.
    #[serde(default)]
    pub args: Vec<String>,
    /// Everything the command read from its stdin: empty when nothing is
    /// piped or redirected into it.
    #[serde(default, with = "b64")]
    pub stdin: Vec<u8>,
}

/// How a command callback ended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandOutput {
    /// Bytes for the command's stdout.
    #[serde(default, with = "b64")]
    pub stdout: Vec<u8>,
    /// Bytes for the command's stderr.
    #[serde(default, with = "b64")]
    pub stderr: Vec<u8>,
    /// The exit status; the shell keeps its low 8 bits, as `$?` does.
    #[serde(default)]
    pub exit_code: i32,
}

impl CommandInput {
    /// Decode a command callback's arguments.
    ///
    /// # Errors
    ///
    /// [`CallbackError::InvalidArguments`] if `args` isn't a command
    /// callback's arguments object, or its stdin is over
    /// [`MAX_COMMAND_STDIN_BYTES`].
    pub fn from_value(args: serde_json::Value) -> Result<Self, CallbackError> {
        let input: Self = serde_json::from_value(args).map_err(|e| {
            CallbackError::InvalidArguments(format!(
                "a command callback takes {{\"args\": [strings], \"stdin\": base64}}: {e}"
            ))
        })?;
        if input.stdin.len() > MAX_COMMAND_STDIN_BYTES {
            return Err(CallbackError::InvalidArguments(format!(
                "stdin is over the {} MiB limit",
                MAX_COMMAND_STDIN_BYTES >> 20
            )));
        }
        Ok(input)
    }

    /// The arguments object, as a guest sends it.
    #[must_use]
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }
}

impl CommandOutput {
    /// Decode a command callback's result, e.g. a remote handler's JSON.
    ///
    /// # Errors
    ///
    /// [`CallbackError::ExecutionFailed`] if `value` isn't a command
    /// callback's result object.
    pub fn from_value(value: serde_json::Value) -> Result<Self, CallbackError> {
        serde_json::from_value(value).map_err(|e| {
            CallbackError::ExecutionFailed(format!(
                "a command callback returns {{\"stdout\": base64, \"stderr\": base64, \
                 \"exit_code\": integer}}: {e}"
            ))
        })
    }

    /// The result object the guest receives.
    ///
    /// # Errors
    ///
    /// [`CallbackError::ExecutionFailed`] if stdout and stderr together are
    /// over [`MAX_COMMAND_OUTPUT_BYTES`].
    pub fn into_value(self) -> Result<serde_json::Value, CallbackError> {
        let len = self.stdout.len() + self.stderr.len();
        if len > MAX_COMMAND_OUTPUT_BYTES {
            return Err(CallbackError::ExecutionFailed(format!(
                "output is over the {} MiB limit ({len} bytes)",
                MAX_COMMAND_OUTPUT_BYTES >> 20
            )));
        }
        serde_json::to_value(self).map_err(|e| CallbackError::ExecutionFailed(e.to_string()))
    }
}

/// The parameters schema of every command callback.
#[must_use]
pub fn command_schema() -> Schema {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "args": {
                "type": "array",
                "items": {"type": "string"},
                "description": "The command's arguments, after its name"
            },
            "stdin": {
                "type": "string",
                "contentEncoding": "base64",
                "description": "The command's stdin, base64-encoded"
            }
        },
        COMMAND_SCHEMA_KEY: true
    });
    // A literal object is always a valid schema.
    Schema::try_from_value(schema).unwrap_or_else(|_| Schema::empty())
}

/// Whether `callback` is a command callback: its schema has
/// [`COMMAND_SCHEMA_KEY`] set.
pub(crate) fn is_command(callback: &dyn Callback) -> bool {
    callback.parameters_schema().0.get(COMMAND_SCHEMA_KEY) == Some(&serde_json::Value::Bool(true))
}

/// A boxed async handler for a [`CommandCallback`].
pub type CommandHandler = Arc<
    dyn Fn(
            CommandInput,
        ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CallbackError>> + Send>>
        + Send
        + Sync,
>;

/// A callback the shell runs as a command, with its arguments as given and
/// its stdin, returning stdout, stderr and an exit code.
///
/// In the shell, `NAME`, `invoke NAME` and, under a callback namespace,
/// `ns.NAME` all run it, wherever a command can run: in pipelines, `$(...)`,
/// nested `sh -c`/`bash -c`, `xargs`, `env` and `timeout`. It may take the
/// name of a registered command, which it replaces. Commands a host command
/// spawns itself (git's hooks, say) can't reach it.
///
/// It reads all of its stdin before it is called (up to
/// [`MAX_COMMAND_STDIN_BYTES`]), so, like `ssh`, it consumes input it may not
/// need: in a `while read` loop, give it `< /dev/null`. Its output is written
/// once it returns, up to [`MAX_COMMAND_OUTPUT_BYTES`].
///
/// A [`CallbackError`] fails the command with exit status 1 and the error on
/// stderr, as for any callback; return a [`CommandOutput`] with a non-zero
/// `exit_code` for a command that ran and failed.
///
/// # Example
///
/// ```rust,ignore
/// use eryx::{CommandCallback, CommandOutput};
///
/// let upper = CommandCallback::new("upper", "Uppercases stdin", |input| {
///     Box::pin(async move {
///         Ok(CommandOutput {
///             stdout: input.stdin.to_ascii_uppercase(),
///             ..CommandOutput::default()
///         })
///     })
/// });
/// // In the shell: `echo hi | upper` prints `HI`.
/// ```
#[derive(Clone)]
pub struct CommandCallback {
    name: String,
    description: String,
    handler: CommandHandler,
}

impl std::fmt::Debug for CommandCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandCallback")
            .field("name", &self.name)
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

impl CommandCallback {
    /// A command callback named `name` that runs `handler`.
    #[must_use]
    pub fn new<F>(name: impl Into<String>, description: impl Into<String>, handler: F) -> Self
    where
        F: Fn(
                CommandInput,
            )
                -> Pin<Box<dyn Future<Output = Result<CommandOutput, CallbackError>> + Send>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            handler: Arc::new(handler),
        }
    }
}

impl Callback for CommandCallback {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Schema {
        command_schema()
    }

    fn invoke(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            let input = CommandInput::from_value(args)?;
            (self.handler)(input).await?.into_value()
        })
    }
}

/// Bytes as standard base64 with padding.
mod b64 {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use serde_json::json;

    #[test]
    fn input_and_output_round_trip_as_base64() {
        let input =
            CommandInput::from_value(json!({"args": ["a", "--b"], "stdin": "aGk="})).unwrap();
        assert_eq!(input.args, ["a", "--b"]);
        assert_eq!(input.stdin, b"hi");
        assert_eq!(
            input.to_value(),
            json!({"args": ["a", "--b"], "stdin": "aGk="})
        );
        assert_eq!(
            CommandInput::from_value(json!({})).unwrap(),
            CommandInput::default()
        );
        assert!(CommandInput::from_value(json!({"argv": []})).is_err());
        assert!(CommandInput::from_value(json!({"stdin": "!"})).is_err());

        let out = CommandOutput {
            stdout: b"o".to_vec(),
            stderr: vec![0xff],
            exit_code: 3,
        };
        let value = out.clone().into_value().unwrap();
        assert_eq!(
            value,
            json!({"stdout": "bw==", "stderr": "/w==", "exit_code": 3})
        );
        assert_eq!(CommandOutput::from_value(value).unwrap(), out);
        assert!(CommandOutput::from_value(json!({"exitCode": 1})).is_err());
    }

    #[test]
    fn limits_apply_both_ways() {
        let big =
            base64::engine::general_purpose::STANDARD.encode(vec![0; MAX_COMMAND_STDIN_BYTES + 1]);
        assert!(CommandInput::from_value(json!({"stdin": big})).is_err());
        let out = CommandOutput {
            stdout: vec![0; MAX_COMMAND_OUTPUT_BYTES],
            stderr: vec![0],
            exit_code: 0,
        };
        assert!(out.into_value().is_err());
    }

    #[test]
    fn the_schema_marks_it() {
        let cb = CommandCallback::new("x", "", |_| {
            Box::pin(async { Ok(CommandOutput::default()) })
        });
        assert!(is_command(&cb));
        let plain =
            crate::DynamicCallback::builder("y", "", |_| Box::pin(async { Ok(json!(1)) })).build();
        assert!(!is_command(&plain));
    }
}
