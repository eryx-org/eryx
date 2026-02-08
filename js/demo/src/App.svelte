<script lang="ts">
  import { onMount } from "svelte";
  import {
    getSandboxState,
    initSandbox,
  } from "./lib/sandbox.svelte";
  import StatusBar from "./components/StatusBar.svelte";
  import TabRun from "./components/TabRun.svelte";
  import TabSessions from "./components/TabSessions.svelte";
  import TabNetworking from "./components/TabNetworking.svelte";
  import TabFilesystem from "./components/TabFilesystem.svelte";
  import TabPackages from "./components/TabPackages.svelte";

  const tabs = [
    { id: "run", label: "Run" },
    { id: "sessions", label: "Sessions" },
    { id: "networking", label: "Networking" },
    { id: "filesystem", label: "Filesystem" },
    { id: "packages", label: "Packages" },
  ] as const;

  type TabId = (typeof tabs)[number]["id"];

  let activeTab: TabId = $state("run");

  let state = $derived(getSandboxState());
  let ready = $derived(state.status === "ready");

  onMount(() => {
    initSandbox();
  });
</script>

<main>
  <h1>eryx</h1>
  <p class="description">
    A sandboxed Python 3.14 interpreter running entirely in your browser via
    WebAssembly. Powered by <a href="https://github.com/eryx-org/eryx">eryx</a>.
  </p>

  <StatusBar />

  <nav class="tab-bar" aria-label="Demo sections">
    {#each tabs as tab}
      <button
        class="tab-btn"
        class:active={activeTab === tab.id}
        disabled={tab.id !== "run" && !ready}
        onclick={() => (activeTab = tab.id)}
        aria-current={activeTab === tab.id ? "page" : undefined}
      >
        {tab.label}
      </button>
    {/each}
  </nav>

  {#if activeTab === "run"}
    <TabRun />
  {:else if activeTab === "sessions"}
    <TabSessions />
  {:else if activeTab === "networking"}
    <TabNetworking />
  {:else if activeTab === "filesystem"}
    <TabFilesystem />
  {:else if activeTab === "packages"}
    <TabPackages />
  {/if}
</main>

<style>
  :global(*) {
    box-sizing: border-box;
  }
  :global(body) {
    font-family: system-ui, -apple-system, Segoe UI, Roboto, Helvetica, Arial,
      sans-serif;
    max-width: 900px;
    margin: 0 auto;
    padding: 20px;
    line-height: 1.6;
    color: #1a1a1a;
  }

  h1 {
    margin-bottom: 6px;
    font-size: 28px;
  }
  .description {
    margin-top: 0;
    color: #666;
    margin-bottom: 20px;
    font-size: 15px;
  }
  .description a {
    color: #0056b3;
  }

  .tab-bar {
    display: flex;
    border-bottom: 2px solid #dee2e6;
    margin-bottom: 20px;
    gap: 4px;
  }
  .tab-btn {
    padding: 10px 20px;
    background: none;
    border: none;
    border-bottom: 2px solid transparent;
    margin-bottom: -2px;
    cursor: pointer;
    font-size: 15px;
    font-weight: 500;
    color: #6c757d;
    transition: all 0.15s;
  }
  .tab-btn.active {
    color: #0056b3;
    border-bottom-color: #0056b3;
  }
  .tab-btn:hover:not(.active) {
    color: #495057;
    border-bottom-color: #dee2e6;
  }
  .tab-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  /* Shared global styles for child components */
  :global(textarea) {
    width: 100%;
    padding: 12px;
    border: 1px solid #ccc;
    border-radius: 6px;
    font-size: 14px;
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    min-height: 180px;
    resize: vertical;
    tab-size: 4;
    margin-bottom: 12px;
  }
  :global(textarea:focus) {
    outline: none;
    border-color: #007bff;
    box-shadow: 0 0 0 2px rgba(0, 123, 255, 0.25);
  }

  :global(.btn-row) {
    display: flex;
    gap: 10px;
    margin-bottom: 16px;
    flex-wrap: wrap;
  }
  :global(button) {
    padding: 10px 20px;
    cursor: pointer;
    border: none;
    border-radius: 6px;
    font-size: 14px;
    font-weight: 500;
    transition: background 0.15s;
  }
  :global(.btn-primary) {
    background: #0061d1;
    color: white;
  }
  :global(.btn-primary:hover:not(:disabled)) {
    background: #004a9e;
  }
  :global(.btn-primary:disabled) {
    background: #6c757d;
    cursor: wait;
    opacity: 0.7;
  }
  :global(.btn-secondary) {
    background: #6c757d;
    color: white;
  }
  :global(.btn-secondary:hover:not(:disabled)) {
    background: #545b62;
  }
  :global(.btn-success) {
    background: #28a745;
    color: white;
  }
  :global(.btn-success:hover:not(:disabled)) {
    background: #218838;
  }
  :global(.btn-warning) {
    background: #ffc107;
    color: #212529;
  }
  :global(.btn-warning:hover:not(:disabled)) {
    background: #e0a800;
  }
  :global(.btn-danger) {
    background: #dc3545;
    color: white;
  }
  :global(.btn-danger:hover:not(:disabled)) {
    background: #c82333;
  }
  :global(.btn-sm) {
    padding: 6px 12px;
    font-size: 13px;
  }

  :global(.examples) {
    margin-bottom: 16px;
  }
  :global(.examples-label) {
    font-size: 13px;
    color: #595959;
    margin: 0 0 8px 0;
    font-weight: 500;
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }
  :global(.pill-row) {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  :global(.pill) {
    padding: 5px 14px;
    font-size: 13px;
    background: #f1f3f5;
    border: 1px solid #dee2e6;
    border-radius: 20px;
    color: #495057;
    cursor: pointer;
    transition: all 0.1s;
  }
  :global(.pill:hover) {
    background: #e9ecef;
    border-color: #adb5bd;
  }

  :global(.output-box) {
    background: #1e1e1e;
    color: #d4d4d4;
    padding: 14px;
    border-radius: 6px;
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    font-size: 13px;
    white-space: pre-wrap;
    word-wrap: break-word;
    min-height: 60px;
    max-height: 400px;
    overflow-y: auto;
  }
  :global(.output-box.error-output) {
    color: #f97583;
  }
  :global(.exec-time) {
    font-size: 12px;
    color: #737373;
    margin-top: 6px;
  }

  :global(.info-banner) {
    background: #fff3cd;
    border: 1px solid #ffeaa7;
    border-radius: 6px;
    padding: 14px 18px;
    margin-bottom: 16px;
    font-size: 14px;
    color: #856404;
  }
  :global(.info-banner strong) {
    color: #664d03;
  }

  @keyframes -global-pulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.4;
    }
  }
  :global(.loading) {
    animation: pulse 1.5s ease-in-out infinite;
  }
</style>
