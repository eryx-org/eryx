//! The JavaScript runtime is pre-initialized (#826): every instance starts
//! from one snapshot of a QuickJS context, so state QuickJS seeds when it
//! creates a context must be reseeded per instance.
//!
//! ```sh
//! mise run precompile-runtime-javascript
//! cargo nextest run -p eryx --features embedded,javascript --test js_preinit
//! ```
#![cfg(all(feature = "embedded", feature = "javascript"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use eryx::{Sandbox, SessionExecutor};

const PROBE: &str = "console.log(JSON.stringify({ \
    random: Math.random(), \
    now: performance.now(), \
    skew: Math.abs(performance.timeOrigin + performance.now() - Date.now()), \
}))";

fn probe(stdout: &[u8]) -> serde_json::Value {
    serde_json::from_slice(stdout).unwrap()
}

#[tokio::test]
async fn fresh_instances_get_their_own_random_seed_and_clock() {
    let sandbox = Sandbox::embedded().javascript().build().unwrap();
    let a = probe(&sandbox.execute(PROBE).await.unwrap().stdout);
    let b = probe(&sandbox.execute(PROBE).await.unwrap().stdout);

    assert_ne!(
        a["random"], b["random"],
        "Math.random() replays the snapshot"
    );
    for out in [&a, &b] {
        let random = out["random"].as_f64().unwrap();
        assert!((0.0..1.0).contains(&random), "{out}");
        // Measured from this instance's first execute, not the snapshot's.
        let now = out["now"].as_f64().unwrap();
        assert!((0.0..1000.0).contains(&now), "{out}");
        assert!(out["skew"].as_f64().unwrap() < 1000.0, "{out}");
    }
}

#[tokio::test]
async fn a_session_keeps_its_sequence() {
    let executor = Sandbox::embedded().javascript().build().unwrap().executor();
    let mut session = SessionExecutor::new(executor, &[]).await.unwrap();
    let mut draws = Vec::new();
    for _ in 0..2 {
        let out = session.execute(PROBE).run().await.unwrap();
        draws.push(probe(&out.stdout)["random"].as_f64().unwrap());
    }
    assert_ne!(draws[0], draws[1], "Math.random() repeats across executes");
}
