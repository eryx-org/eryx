"""Benchmark: SandboxPool vs fresh factory.create_sandbox().

Compares per-operation latency and throughput for pooled vs fresh sandbox
creation, both using the same SandboxFactory (shared Tokio runtime + cached
precompiled artifact).

Usage:
    python benchmarks/pool_vs_fresh.py [--iterations N] [--workers W]
"""

import argparse
import statistics
import time
from concurrent.futures import ThreadPoolExecutor, as_completed

import eryx

WORKLOADS = {
    "pass": "pass",
    "print": 'print("hello")',
    "json": "import json; json.dumps(list(range(100)))",
}

WARMUP = 5


def bench_fresh(factory, code, n):
    times = []
    for _ in range(WARMUP):
        factory.create_sandbox().execute(code)
    for _ in range(n):
        t0 = time.perf_counter_ns()
        sandbox = factory.create_sandbox()
        sandbox.execute(code)
        times.append(time.perf_counter_ns() - t0)
    return times


def bench_pool(pool, code, n):
    times = []
    for _ in range(WARMUP):
        with pool.acquire() as sb:
            sb.execute(code)
    for _ in range(n):
        t0 = time.perf_counter_ns()
        with pool.acquire() as sb:
            sb.execute(code)
        times.append(time.perf_counter_ns() - t0)
    return times


def bench_concurrent_fresh(factory, code, n, workers):
    def worker(_):
        t0 = time.perf_counter_ns()
        sandbox = factory.create_sandbox()
        sandbox.execute(code)
        return time.perf_counter_ns() - t0

    # warmup
    for _ in range(WARMUP):
        factory.create_sandbox().execute(code)

    wall_start = time.perf_counter_ns()
    times = []
    with ThreadPoolExecutor(max_workers=workers) as ex:
        futs = [ex.submit(worker, i) for i in range(n)]
        for f in as_completed(futs):
            times.append(f.result())
    wall_ns = time.perf_counter_ns() - wall_start
    return times, wall_ns


def bench_concurrent_pool(pool, code, n, workers):
    def worker(_):
        t0 = time.perf_counter_ns()
        with pool.acquire() as sb:
            sb.execute(code)
        return time.perf_counter_ns() - t0

    # warmup
    for _ in range(WARMUP):
        with pool.acquire() as sb:
            sb.execute(code)

    wall_start = time.perf_counter_ns()
    times = []
    with ThreadPoolExecutor(max_workers=workers) as ex:
        futs = [ex.submit(worker, i) for i in range(n)]
        for f in as_completed(futs):
            times.append(f.result())
    wall_ns = time.perf_counter_ns() - wall_start
    return times, wall_ns


def fmt_ms(ns):
    return f"{ns / 1_000_000:.2f}"


def summarize(times_ns):
    times_ns.sort()
    n = len(times_ns)
    p99_idx = min(int(n * 0.99), n - 1)
    return {
        "min": times_ns[0],
        "med": statistics.median(times_ns),
        "p99": times_ns[p99_idx],
        "max": times_ns[-1],
        "ops": n / (sum(times_ns) / 1_000_000_000) if sum(times_ns) > 0 else 0,
    }


def print_header(title):
    print(f"\n{'=' * 72}")
    print(f"  {title}")
    print(f"{'=' * 72}")


def print_table(rows):
    headers = ["workload", "method", "min ms", "med ms", "p99 ms", "max ms", "ops/s"]
    widths = [max(len(h), max(len(r[i]) for r in rows)) for i, h in enumerate(headers)]
    fmt = "  ".join(f"{{:<{w}}}" for w in widths)
    print(fmt.format(*headers))
    print(fmt.format(*["-" * w for w in widths]))
    for row in rows:
        print(fmt.format(*row))


def main():
    parser = argparse.ArgumentParser(description="Pool vs fresh sandbox benchmarks")
    parser.add_argument("--iterations", "-n", type=int, default=100)
    parser.add_argument("--workers", "-w", type=int, default=4)
    args = parser.parse_args()
    n = args.iterations
    workers = args.workers

    factory = eryx.SandboxFactory(cache=True)
    pool = factory.create_pool(max_size=workers, min_idle=workers)

    # --- Sequential ---
    print_header(f"Sequential  ({n} iterations, {WARMUP} warmup)")
    rows = []
    for name, code in WORKLOADS.items():
        fresh = summarize(bench_fresh(factory, code, n))
        pooled = summarize(bench_pool(pool, code, n))
        rows.append((name, "fresh", fmt_ms(fresh["min"]), fmt_ms(fresh["med"]),
                      fmt_ms(fresh["p99"]), fmt_ms(fresh["max"]), f"{fresh['ops']:.0f}"))
        rows.append(("", "pool", fmt_ms(pooled["min"]), fmt_ms(pooled["med"]),
                      fmt_ms(pooled["p99"]), fmt_ms(pooled["max"]), f"{pooled['ops']:.0f}"))
        speedup = fresh["med"] / pooled["med"] if pooled["med"] > 0 else float("inf")
        rows.append(("", f"  -> {speedup:.2f}x", "", "", "", "", ""))
    print_table(rows)

    # --- Concurrent ---
    print_header(f"Concurrent  ({n} tasks, {workers} workers, {WARMUP} warmup)")
    rows = []
    for name, code in WORKLOADS.items():
        ft, fw = bench_concurrent_fresh(factory, code, n, workers)
        pt, pw = bench_concurrent_pool(pool, code, n, workers)
        fs, ps = summarize(ft), summarize(pt)
        wall_fresh = fw / 1_000_000_000
        wall_pool = pw / 1_000_000_000
        rows.append((name, "fresh", fmt_ms(fs["min"]), fmt_ms(fs["med"]),
                      fmt_ms(fs["p99"]), fmt_ms(fs["max"]),
                      f"{n / wall_fresh:.0f}"))
        rows.append(("", "pool", fmt_ms(ps["min"]), fmt_ms(ps["med"]),
                      fmt_ms(ps["p99"]), fmt_ms(ps["max"]),
                      f"{n / wall_pool:.0f}"))
        speedup = fs["med"] / ps["med"] if ps["med"] > 0 else float("inf")
        rows.append(("", f"  -> {speedup:.2f}x", "", "", "", "", ""))
    print_table(rows)

    pool.close()
    print(f"\nPool stats: {pool.stats()}")


if __name__ == "__main__":
    main()
