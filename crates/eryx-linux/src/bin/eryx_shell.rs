//! The same workloads through eryx's own shell guest (brush + coreutils),
//! for comparison with the unkernel host. Prints ms per workload and peak RSS.
use std::time::Instant;

use eryx::Sandbox;

pub const WORKLOADS: &[(&str, &str)] = &[
    ("echo", "echo hi"),
    (
        "loop-20k",
        "i=0; while [ $i -lt 20000 ]; do i=$((i+1)); done; echo $i",
    ),
    ("sort-100k", "seq 100000 | sort -r | wc -l"),
    ("sha256-200k", "seq 200000 | sha256sum"),
];

#[tokio::main]
async fn main() {
    let t0 = Instant::now();
    let sandbox = Sandbox::embedded().shell().build().expect("shell sandbox");
    println!("build-sandbox {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
    for round in 1..=3 {
        for (name, code) in WORKLOADS {
            let t = Instant::now();
            let out = sandbox.execute(code).await.expect(name);
            let ms = t.elapsed().as_secs_f64() * 1e3;
            println!(
                "round{round} {name} {ms:.1} ms -> {}",
                out.stdout_utf8().unwrap_or("?").trim()
            );
        }
    }
    println!("total {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for l in status
        .lines()
        .filter(|l| l.starts_with("VmHWM") || l.starts_with("VmRSS"))
    {
        println!("{l}");
    }
}
