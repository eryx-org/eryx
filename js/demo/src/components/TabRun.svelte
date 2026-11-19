<script lang="ts">
  import { getSandboxState, runCode, warmSandbox } from "../lib/sandbox.svelte";
  import { JS_EXAMPLES, RUN_EXAMPLES, SHELL_EXAMPLES } from "../lib/examples";
  import CodeEditor from "./CodeEditor.svelte";
  import ExamplePills from "./ExamplePills.svelte";
  import OutputBox from "./OutputBox.svelte";

  let state = $derived(getSandboxState());
  let ready = $derived(state.status === "ready");

  type Language = "python" | "javascript" | "shell";
  const LANGUAGES: Record<Language, { label: string; examples: typeof RUN_EXAMPLES }> = {
    python: { label: "Python", examples: RUN_EXAMPLES },
    javascript: { label: "JavaScript", examples: JS_EXAMPLES },
    shell: { label: "Shell", examples: SHELL_EXAMPLES },
  };

  let language: Language = $state("python");
  let code = $state(
    `print("Hello from eryx!")
result = sum(range(10))
print(f"Sum of 0..9 = {result}")`,
  );
  let running = $state(false);
  let output: string | null = $state(null);
  let isError = $state(false);
  let elapsed: number | null = $state(null);

  // Restore code (and its language, `#<language>:<code>`, Python when
  // there's no prefix) from the URL hash
  if (window.location.hash) {
    try {
      const hash = decodeURIComponent(window.location.hash.slice(1));
      const prefix = hash.slice(0, hash.indexOf(":"));
      if (prefix === "javascript" || prefix === "shell") {
        language = prefix;
        code = hash.slice(prefix.length + 1);
        // Only load a language's runtime once it's picked, not for every visitor.
        warmSandbox(prefix);
      } else {
        code = hash;
      }
    } catch {}
  }

  // Save code to URL hash on change
  $effect(() => {
    const hash = language === "python" ? code : `${language}:${code}`;
    history.replaceState(null, "", "#" + encodeURIComponent(hash));
  });

  function selectLanguage(next: Language) {
    if (next === language) return;
    language = next;
    warmSandbox(next);
    code = Object.values(LANGUAGES[next].examples)[0].code;
    output = null;
    elapsed = null;
  }

  async function run() {
    const trimmed = code.trim();
    if (!trimmed || running) return;

    running = true;
    output = "";
    const result = await runCode(trimmed, language);
    running = false;

    if (result.ok) {
      let text = "";
      if (result.result.stdout) text += result.result.stdout;
      if (result.result.stderr) {
        if (text) text += "\n";
        text += "[stderr] " + result.result.stderr;
      }
      output = text || "(no output)";
      isError = false;
    } else {
      output = "Error: " + result.error;
      isError = true;
    }
    elapsed = result.elapsed;
  }
</script>

<p class="share-hint">Code is saved in the URL for sharing.</p>

<div class="pill-row languages" role="radiogroup" aria-label="Language">
  {#each Object.entries(LANGUAGES) as [key, lang]}
    <button
      class="pill"
      class:active={language === key}
      role="radio"
      aria-checked={language === key}
      onclick={() => selectLanguage(key as Language)}>{lang.label}</button
    >
  {/each}
</div>

<ExamplePills
  examples={LANGUAGES[language].examples}
  onselect={(c) => (code = c)}
/>

<CodeEditor bind:value={code} onrun={run} />

<div class="btn-row">
  <button class="btn-primary" disabled={!ready || running} onclick={run}>
    {running ? "Running..." : "Run (Ctrl+Enter)"}
  </button>
  <button
    class="btn-secondary"
    onclick={() => {
      output = null;
      elapsed = null;
    }}
  >
    Clear Output
  </button>
</div>

{#if output != null}
  <OutputBox {output} {isError} {elapsed} showCopy />
{/if}

<style>
  .languages {
    margin-bottom: 12px;
  }
  .languages .pill.active {
    background: var(--accent);
    color: var(--bg);
  }
  .share-hint {
    color: var(--text-muted);
    margin-top: 0;
    font-size: 13px;
  }
</style>
