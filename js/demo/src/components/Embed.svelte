<script lang="ts">
  import { onMount } from "svelte";
  import { getSandboxState, initSandbox, runCode } from "../lib/sandbox.svelte";
  import CodeEditor from "./CodeEditor.svelte";
  import OutputBox from "./OutputBox.svelte";

  // Compact view for embedding on eryx.run (?embed). This page plays the
  // host: it implements the callbacks below, with fake latency so the call
  // log shows `asyncio.gather` running them concurrently.
  const hostFunctions: Record<
    string,
    { description: string; latencyMs: number; fn: (args: any) => unknown }
  > = {
    get_user: {
      description: "Look up a user by id",
      latencyMs: 80,
      fn: ({ id }) => ({ id, name: "Alice", city: "Lisbon" }),
    },
    get_orders: {
      description: "List a user's orders",
      latencyMs: 300,
      fn: () => [
        { id: 1, total: 42.5 },
        { id: 2, total: 18.0 },
        { id: 3, total: 99.99 },
      ],
    },
    get_weather: {
      description: "Current weather for a city",
      latencyMs: 250,
      fn: ({ city }) => ({ city, temp_c: 21, sky: "clear" }),
    },
  };

  const examples = {
    callbacks: {
      label: "Callbacks",
      code: `import asyncio, time

# get_user, get_orders, get_weather: host functions,
# implemented in JavaScript by this page.
user = await get_user(id=123)
print(f"Hello {user['name']} from {user['city']}")

start = time.monotonic()
orders, weather = await asyncio.gather(
    get_orders(user_id=user["id"]),
    get_weather(city=user["city"]),
)
spent = sum(o["total"] for o in orders)
print(f"{len(orders)} orders, {spent:.2f} spent")
print(f"{weather['temp_c']}°C and {weather['sky']}")
took = (time.monotonic() - start) * 1000
print(f"both lookups: {took:.0f}ms")`,
    },
    escape: {
      label: "Try to escape",
      code: `import socket, subprocess
connect = socket.create_connection

attempts = {
    "read host files": lambda: open("/etc/passwd"),
    "run a shell": lambda: subprocess.run("id"),
    "dial localhost": lambda: connect(("127.0.0.1", 22)),
    "metadata": lambda: connect(("169.254.169.254", 80)),
    "native code": lambda: __import__("ctypes"),
}
for what, attempt in attempts.items():
    try:
        attempt()
        print(f"allowed  {what}")
    except Exception as e:
        print(f"blocked  {what}\\n  {type(e).__name__}: {e}")`,
    },
    python: {
      label: "Plain Python",
      code: `import sys

# Real CPython, compiled to WebAssembly
print(sys.version.split()[0], "on", sys.platform)
chars = " .:-=+*#%@"

for y in range(-10, 11):
    row = ""
    for x in range(-42, 14):
        c, z, n = complex(x / 19, y / 8.5), 0, 0
        while abs(z) < 2 and n < 60:
            z, n = z * z + c, n + 1
        row += chars[min(n // 3, 9)] if n < 60 else " "
    print(row)`,
    },
  };
  type ExampleKey = keyof typeof examples;

  type Call = { label: string; start: number; end: number | null };

  let active: ExampleKey = $state("callbacks");
  let code = $state(examples.callbacks.code);
  let calls: Call[] = $state([]);
  let runStart = 0;
  let root: HTMLElement;

  let state = $derived(getSandboxState());
  let ready = $derived(state.status === "ready");
  let running = $state(false);
  let output: string | null = $state(null);
  let isError = $state(false);
  let elapsed: number | null = $state(null);
  let span = $derived(Math.max(1, ...calls.map((c) => c.end ?? c.start)));

  async function run() {
    const trimmed = code.trim();
    if (!trimmed || running || !ready) return;
    running = true;
    calls = [];
    runStart = performance.now();
    const result = await runCode(trimmed);
    running = false;
    if (result.ok) {
      const { stdout, stderr } = result.result;
      output = [stdout, stderr && "[stderr] " + stderr].filter(Boolean).join("\n") || "(no output)";
      isError = false;
    } else {
      output = "Error: " + result.error;
      isError = true;
    }
    elapsed = result.elapsed;
  }

  function select(key: ExampleKey) {
    active = key;
    code = examples[key].code;
    output = null;
    calls = [];
    run();
  }

  async function handleCallback(name: string, argsJson: string): Promise<string> {
    const host = hostFunctions[name];
    if (!host) throw new Error(`Unknown callback: ${name}`);
    const args = JSON.parse(argsJson);
    const shown = Object.entries(args)
      .map(([k, v]) => `${k}=${JSON.stringify(v)}`)
      .join(", ");
    calls.push({ label: `${name}(${shown})`, start: performance.now() - runStart, end: null });
    const call = calls[calls.length - 1];
    await new Promise((resolve) => setTimeout(resolve, host.latencyMs));
    call.end = performance.now() - runStart;
    return JSON.stringify(host.fn(args));
  }

  onMount(async () => {
    const { setCallbacks, setCallbackHandler } = await import("@bsull/eryx");
    setCallbacks(
      Object.entries(hostFunctions).map(([name, { description }]) => ({ name, description })),
    );
    setCallbackHandler(handleCallback);
    // Tell the embedding page (eryx.run) how tall we are, so it can size
    // the iframe instead of making us scroll.
    if (window.parent !== window) {
      new ResizeObserver(() =>
        window.parent.postMessage({ eryxEmbedHeight: root.offsetHeight }, "*"),
      ).observe(root);
    }
    await initSandbox();
    await run();
  });
</script>

<div class="embed" bind:this={root}>
  <div class="pill-row" role="tablist">
    {#each Object.entries(examples) as [key, ex]}
      <button
        class="pill"
        class:active={active === key}
        role="tab"
        aria-selected={active === key}
        disabled={running}
        onclick={() => select(key as ExampleKey)}>{ex.label}</button
      >
    {/each}
    <a class="full-demo" href="/" target="_blank" rel="noopener">Open full demo ↗</a>
  </div>
  <CodeEditor bind:value={code} onrun={run} />
  <div class="btn-row">
    <button class="btn-primary" disabled={!ready || running} onclick={run}>
      {running ? "Running..." : "Run (Ctrl+Enter)"}
    </button>
  </div>
  {#if calls.length}
    <div class="calls" aria-label="Calls from the sandbox to this page">
      <div class="calls-title">sandbox → host</div>
      {#each calls as call}
        <div class="call">
          <code>{call.label}</code>
          <span class="track">
            <span
              class="bar"
              class:pending={call.end == null}
              style:left="{(call.start / span) * 100}%"
              style:width="{(((call.end ?? span) - call.start) / span) * 100}%"
            ></span>
          </span>
          <span class="ms">{call.end == null ? "…" : `${Math.round(call.end - call.start)}ms`}</span>
        </div>
      {/each}
    </div>
  {/if}
  {#if state.status === "ready" && output != null}
    <div class:nowrap={active === "python"}>
      <OutputBox {output} {isError} {elapsed} />
    </div>
  {:else if state.status === "error"}
    <div class="output-box error-output">{state.message}</div>
  {:else}
    <div class="output-box loading">
      {state.status === "ready" ? "Running..." : "Loading Python sandbox (~45MB)…"}
    </div>
  {/if}
</div>

<style>
  .embed {
    padding: 16px;
  }
  .embed :global(textarea) {
    field-sizing: content;
    min-height: 0;
    font-size: 13px;
    white-space: pre;
    overflow-x: auto;
    resize: none;
  }
  .embed :global(.output-box) {
    max-height: none;
  }
  /* ASCII art must not wrap on narrow screens; let it scroll instead. */
  .nowrap :global(.output-box) {
    white-space: pre;
    overflow-x: auto;
  }
  .pill-row {
    align-items: center;
    margin-bottom: 12px;
  }
  .pill.active {
    color: var(--accent);
    border-color: var(--accent);
  }
  .full-demo {
    margin-left: auto;
    font-size: 13px;
    color: var(--text-muted);
    text-decoration: none;
  }
  .full-demo:hover {
    color: var(--text);
  }
  .calls {
    margin: 0 0 12px;
    padding: 10px 12px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--surface);
    font-size: 12px;
  }
  .calls-title {
    color: var(--text-muted);
    margin-bottom: 6px;
  }
  .call {
    display: grid;
    grid-template-columns: minmax(0, 15em) 1fr 3.5em;
    gap: 10px;
    align-items: center;
    line-height: 1.8;
  }
  .call code {
    font-family: var(--font-mono);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .track {
    position: relative;
    height: 6px;
    border-radius: 3px;
    background: var(--surface-2);
  }
  .bar {
    position: absolute;
    top: 0;
    bottom: 0;
    border-radius: 3px;
    background: var(--accent);
  }
  .bar.pending {
    opacity: 0.5;
  }
  .ms {
    text-align: right;
    color: var(--text-muted);
    font-variant-numeric: tabular-nums;
  }
  :global(html.embed),
  :global(html.embed body) {
    background: transparent;
  }
  :global(html.embed body::before) {
    display: none;
  }
</style>
