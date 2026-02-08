<script lang="ts">
  interface Props {
    output: string;
    isError?: boolean;
    elapsed?: number | null;
    showCopy?: boolean;
  }

  let {
    output,
    isError = false,
    elapsed = null,
    showCopy = false,
  }: Props = $props();

  let copied = $state(false);

  async function copy() {
    await navigator.clipboard.writeText(output);
    copied = true;
    setTimeout(() => (copied = false), 1500);
  }
</script>

<div class="output-section">
  {#if showCopy}
    <div class="output-header">
      <h2>Output</h2>
      <button class="btn-secondary btn-sm" onclick={copy}>
        {copied ? "Copied!" : "Copy"}
      </button>
    </div>
  {/if}
  <div class="output-box" class:error-output={isError}>{output}</div>
  {#if elapsed != null}
    <div class="exec-time">
      {isError ? "Failed" : "Executed"} in {elapsed.toFixed(1)}ms
    </div>
  {/if}
</div>

<style>
  .output-section {
    margin-top: 4px;
  }
  .output-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 8px;
  }
  .output-header h2 {
    margin: 0;
    font-size: 18px;
  }
</style>
