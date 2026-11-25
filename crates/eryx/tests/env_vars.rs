//! Environment variables from `SandboxBuilder::with_env`, in each guest:
//! `os.environ` in Python, `process.env` in JavaScript, and exported in the
//! shell, so its commands inherit them. Values are set as given, so one
//! holding quotes, newlines or shell syntax can't run code.
//!
//! ```sh
//! mise run setup
//! cargo nextest run -p eryx --features embedded,javascript,shell --test env_vars
//! ```
#![cfg(feature = "embedded")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use eryx::{ErrorKind, Sandbox};

/// Ends each language's quoting and runs code, if it isn't escaped.
const HOSTILE: &str =
    "a'b\"c\\d\n$(echo pwned >/tmp/pwned)`echo pwned`'); open('/tmp/pwned','w') #\u{2028}é";

#[tokio::test]
async fn python_sees_env_in_os_environ() {
    let sandbox = Sandbox::embedded()
        .with_env("GIT_AUTHOR_NAME", "Eryx Bot")
        .with_env("HOSTILE", HOSTILE)
        .with_secret("TOKEN", "real-value", vec!["api.test".to_string()])
        .build()
        .unwrap();
    let out = sandbox
        .execute(&format!(
            "import os\n\
             print(os.environ['GIT_AUTHOR_NAME'])\n\
             print(os.environ['HOSTILE'] == {expected})\n\
             print(os.environ['TOKEN'].startswith('ERYX_SECRET_PLACEHOLDER_'))\n\
             print(os.path.exists('/tmp/pwned'))\n",
            expected = serde_json::to_string(HOSTILE).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "Eryx Bot\nTrue\nTrue\nFalse\n");
}

/// The values aren't secrets: printing one prints it.
#[tokio::test]
async fn env_values_are_not_scrubbed() {
    let sandbox = Sandbox::embedded()
        .with_env("NAME", "visible")
        .with_secret("TOKEN", "real-value", vec!["api.test".to_string()])
        .build()
        .unwrap();
    let out = sandbox
        .execute("import os; print(os.environ['NAME'], os.environ['TOKEN'])")
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "visible [REDACTED]\n");
}

#[test]
fn bad_env_fails_the_build() {
    let refused = |builder: eryx::SandboxBuilder<eryx::language::Python, eryx::state::Has>,
                   expected: &str| {
        let err = builder.build().unwrap_err();
        assert!(
            matches!(err.kind(), ErrorKind::Initialization(message) if message.contains(expected)),
            "{err}"
        );
    };
    refused(
        Sandbox::embedded().with_env("NOT-A-NAME", "x"),
        "invalid environment variable name",
    );
    refused(
        Sandbox::embedded().with_env("PATH", "/bin"),
        "set by the sandbox itself",
    );
    refused(
        Sandbox::embedded().with_env("TOKEN", "x").with_secret(
            "TOKEN",
            "v",
            vec!["api.test".to_string()],
        ),
        "collides with the secret TOKEN",
    );
    refused(Sandbox::embedded().with_env("A", "a\0b"), "NUL byte");
}

#[cfg(feature = "javascript")]
#[tokio::test]
async fn javascript_sees_env_in_process_env() {
    let sandbox = Sandbox::embedded()
        .javascript()
        .with_env("GIT_AUTHOR_NAME", "Eryx Bot")
        .with_env("HOSTILE", HOSTILE)
        .with_env("__proto__", "not the prototype")
        .with_secret("TOKEN", "real-value", vec!["api.test".to_string()])
        .build()
        .unwrap();
    let out = sandbox
        .execute(&format!(
            "console.log(process.env.GIT_AUTHOR_NAME);\n\
             console.log(process.env.HOSTILE === {expected});\n\
             console.log(process.env.TOKEN.startsWith('ERYX_SECRET_PLACEHOLDER_'));\n\
             console.log(Object.hasOwn(process.env, '__proto__'), \
                         Object.getPrototypeOf(process.env) === Object.prototype);\n\
             console.log(typeof globalThis.pwned);\n",
            expected = serde_json::to_string(HOSTILE).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "Eryx Bot\ntrue\ntrue\ntrue true\nundefined\n"
    );
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn the_shell_exports_env_to_its_commands() {
    let sandbox = Sandbox::embedded()
        .shell()
        .with_env("GIT_AUTHOR_NAME", "Eryx Bot")
        .with_env("HOSTILE", HOSTILE)
        .build()
        .unwrap();
    let out = sandbox
        .execute(
            r#"
echo "$GIT_AUTHOR_NAME"
printenv GIT_AUTHOR_NAME
sh -c 'echo "nested $GIT_AUTHOR_NAME"'
[ -e /tmp/pwned ] && echo pwned || echo safe
"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "Eryx Bot\nEryx Bot\nnested Eryx Bot\nsafe\n"
    );
    let out = sandbox.execute(r#"printf '%s' "$HOSTILE""#).await.unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), HOSTILE);
}
