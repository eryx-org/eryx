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

<header class="site-header">
  <a class="logo" href="https://eryx.run">
    <svg class="logo-mark" viewBox="0 0 32 32" aria-hidden="true">
      <path d="M6 24c0-5 5-6 10-6s10-1 10-6-4-7-9-7c-3 0-5 1-6 3" />
      <circle cx="12.5" cy="7.6" r="1.1" />
    </svg>
    eryx
  </a>
  <nav class="site-links" aria-label="Site">
    <a href="https://docs.eryx.run">Docs</a>
    <a href="https://github.com/eryx-org/eryx">GitHub</a>
  </nav>
</header>

<div class="intro">
  <svg
    class="ripples"
    viewBox="0 0 1440 260"
    preserveAspectRatio="xMidYMax slice"
    aria-hidden="true"
  >
      <path d="M0 20.0 L40 22.4 L80 24.2 L120 25.1 L160 25.0 L200 24.3 L240 23.5 L280 23.0 L320 23.0 L360 23.5 L400 24.1 L440 24.2 L480 23.6 L520 22.0 L560 19.7 L600 17.3 L640 15.3 L680 14.2 L720 14.2 L760 14.9 L800 16.0 L840 17.0 L880 17.4 L920 17.3 L960 17.0 L1000 16.8 L1040 17.2 L1080 18.5 L1120 20.6 L1160 22.9 L1200 25.0 L1240 26.2 L1280 26.4 L1320 25.7 L1360 24.3 L1400 23.0 L1440 22.1" stroke-opacity="0.04" />
      <path d="M0 39.8 L40 41.1 L80 41.2 L120 40.4 L160 39.2 L200 38.2 L240 37.6 L280 37.7 L320 38.1 L360 38.4 L400 37.9 L440 36.6 L480 34.4 L520 31.8 L560 29.5 L600 28.0 L640 27.7 L680 28.5 L720 29.9 L760 31.4 L800 32.4 L840 32.8 L880 32.7 L920 32.5 L960 32.7 L1000 33.7 L1040 35.6 L1080 37.9 L1120 40.2 L1160 41.8 L1200 42.3 L1240 41.6 L1280 40.1 L1320 38.2 L1360 36.7 L1400 35.8 L1440 35.6" stroke-opacity="0.05" />
      <path d="M0 58.9 L40 58.2 L80 56.7 L120 55.0 L160 53.9 L200 53.4 L240 53.5 L280 53.8 L320 53.6 L360 52.5 L400 50.6 L440 48.0 L480 45.5 L520 43.6 L560 43.0 L600 43.7 L640 45.3 L680 47.3 L720 49.0 L760 50.0 L800 50.3 L840 50.1 L880 50.1 L920 50.8 L960 52.3 L1000 54.5 L1040 56.9 L1080 58.8 L1120 59.6 L1160 59.0 L1200 57.4 L1240 55.1 L1280 53.0 L1320 51.5 L1360 50.8 L1400 50.8 L1440 50.9" stroke-opacity="0.07" />
      <path d="M0 75.7 L40 73.6 L80 71.7 L120 70.6 L160 70.3 L200 70.4 L240 70.4 L280 69.8 L320 68.2 L360 65.8 L400 63.2 L440 61.1 L480 60.1 L520 60.6 L560 62.3 L600 64.7 L640 67.1 L680 68.8 L720 69.5 L760 69.6 L800 69.5 L840 69.7 L880 70.8 L920 72.7 L960 75.0 L1000 77.1 L1040 78.2 L1080 77.9 L1120 76.2 L1160 73.6 L1200 70.9 L1240 68.7 L1280 67.5 L1320 67.2 L1360 67.4 L1400 67.5 L1440 66.9" stroke-opacity="0.08" />
      <path d="M0 91.3 L40 89.4 L80 88.5 L120 88.4 L160 88.6 L200 88.3 L240 87.2 L280 85.2 L320 82.7 L360 80.5 L400 79.2 L440 79.4 L480 81.1 L520 83.8 L560 86.7 L600 89.1 L640 90.5 L680 90.9 L720 90.7 L760 90.6 L800 91.1 L840 92.5 L880 94.6 L920 96.7 L960 98.0 L1000 98.0 L1040 96.4 L1080 93.7 L1120 90.5 L1160 87.7 L1200 85.8 L1240 85.1 L1280 85.2 L1320 85.6 L1360 85.7 L1400 84.9 L1440 83.4" stroke-opacity="0.09" />
      <path d="M0 108.3 L40 107.8 L80 108.0 L120 108.1 L160 107.6 L200 106.1 L240 104.0 L280 101.7 L320 100.2 L360 100.1 L400 101.6 L440 104.4 L480 107.8 L520 110.9 L560 113.1 L600 113.9 L640 113.8 L680 113.4 L720 113.3 L760 114.1 L800 115.6 L840 117.5 L880 119.0 L920 119.3 L960 118.0 L1000 115.3 L1040 111.8 L1080 108.4 L1120 105.8 L1160 104.5 L1200 104.5 L1240 105.1 L1280 105.7 L1320 105.7 L1360 104.8 L1400 103.4 L1440 102.1" stroke-opacity="0.11" />
      <path d="M0 128.7 L40 129.2 L80 129.3 L120 128.6 L160 126.9 L200 124.9 L240 123.3 L280 122.8 L320 124.1 L360 126.9 L400 130.6 L440 134.3 L480 137.1 L520 138.5 L560 138.7 L600 138.0 L640 137.3 L680 137.3 L720 138.2 L760 139.8 L800 141.2 L840 141.7 L880 140.8 L920 138.3 L960 134.6 L1000 130.7 L1040 127.5 L1080 125.7 L1120 125.3 L1160 126.1 L1200 127.2 L1240 127.9 L1280 127.8 L1320 126.9 L1360 125.8 L1400 125.2 L1440 125.8" stroke-opacity="0.12" />
      <path d="M0 152.4 L40 152.4 L80 151.5 L120 149.9 L160 148.3 L200 147.7 L240 148.5 L280 151.1 L320 154.9 L360 159.0 L400 162.5 L440 164.6 L480 165.1 L520 164.4 L560 163.2 L600 162.4 L640 162.5 L680 163.4 L720 164.6 L760 165.3 L800 164.8 L840 162.6 L880 159.0 L920 154.8 L960 151.1 L1000 148.6 L1040 147.8 L1080 148.6 L1120 150.1 L1160 151.6 L1200 152.4 L1240 152.2 L1280 151.4 L1320 150.7 L1360 151.0 L1400 152.8 L1440 156.0" stroke-opacity="0.13" />
      <path d="M0 177.7 L40 176.7 L80 175.4 L120 174.6 L160 175.0 L200 177.2 L240 180.9 L280 185.3 L320 189.4 L360 192.1 L400 193.1 L440 192.4 L480 190.8 L520 189.2 L560 188.3 L600 188.5 L640 189.3 L680 190.1 L720 189.8 L760 188.1 L800 184.8 L840 180.6 L880 176.5 L920 173.4 L960 172.1 L1000 172.7 L1040 174.5 L1080 176.7 L1120 178.4 L1160 179.1 L1200 178.9 L1240 178.3 L1280 178.3 L1320 179.6 L1360 182.3 L1400 186.2 L1440 190.3" stroke-opacity="0.15" />
      <path d="M0 204.4 L40 203.5 L80 203.7 L120 205.4 L160 208.7 L200 213.1 L240 217.6 L280 220.9 L320 222.4 L360 222.0 L400 220.1 L440 217.8 L480 216.0 L520 215.2 L560 215.4 L600 215.9 L640 216.0 L680 214.8 L720 212.0 L760 208.0 L800 203.7 L840 200.2 L880 198.3 L920 198.6 L960 200.5 L1000 203.3 L1040 205.9 L1080 207.6 L1120 208.2 L1160 207.9 L1200 207.7 L1240 208.4 L1280 210.6 L1320 214.0 L1360 218.1 L1400 221.6 L1440 223.7" stroke-opacity="0.16" />
  </svg>
  <div class="intro-content">
    <h1>Python in your browser, safely sandboxed</h1>
    <p class="description">
      CPython 3.14 compiled to WebAssembly, running entirely on this page. No
      server, no installs. Powered by
      <a href="https://github.com/eryx-org/eryx">eryx</a>, which you can
      <a href="https://docs.eryx.run">embed in your own app</a>.
    </p>
  </div>
</div>

<main>
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
  :global(:root) {
    --bg: #0e0b08;
    --surface: #15110d;
    --surface-2: #1d1812;
    --surface-3: #262017;
    --border: #2e261c;
    --border-hover: #4a3d2c;
    --text: #f6efe4;
    --text-secondary: #b8ab97;
    --text-muted: #8a7d69;
    --accent: #e3ad62;
    --accent-hover: #cf963f;
    --accent-muted: rgba(227, 173, 98, 0.12);
    --accent-ring: rgba(227, 173, 98, 0.3);
    --success: #a9c98a;
    --success-bg: rgba(169, 201, 138, 0.08);
    --success-border: rgba(169, 201, 138, 0.28);
    --danger: #e8897a;
    --danger-bg: rgba(232, 137, 122, 0.08);
    --danger-border: rgba(232, 137, 122, 0.3);
    --info: #9cc3d1;
    --info-bg: rgba(156, 195, 209, 0.07);
    --info-border: rgba(156, 195, 209, 0.25);
    --violet: #c2a6e0;
    --font-mono: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas,
      monospace;
    color-scheme: dark;
  }

  :global(*) {
    box-sizing: border-box;
  }
  :global(body) {
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto,
      "Helvetica Neue", Arial, sans-serif;
    margin: 0;
    line-height: 1.6;
    color: var(--text);
    background: var(--bg);
    -webkit-font-smoothing: antialiased;
  }
  /* Faint sand grain, as on eryx.run */
  :global(body::before) {
    content: "";
    position: fixed;
    inset: 0;
    z-index: -1;
    pointer-events: none;
    background-image:
      radial-gradient(rgba(227, 173, 98, 0.04) 1px, transparent 1px),
      radial-gradient(rgba(246, 239, 228, 0.025) 1px, transparent 1px);
    background-size: 3px 3px, 7px 7px;
    background-position: 0 0, 1px 2px;
  }
  :global(a) {
    color: var(--accent);
  }
  :global(a:hover) {
    color: var(--accent-hover);
  }
  :global(h2),
  :global(h3) {
    color: var(--text);
  }
  :global(code) {
    font-family: var(--font-mono);
  }

  .site-header {
    max-width: 960px;
    margin: 0 auto;
    padding: 18px 20px;
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  .logo {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 20px;
    font-weight: 600;
    letter-spacing: -0.02em;
    color: var(--text);
    text-decoration: none;
  }
  .logo-mark {
    width: 22px;
    height: 22px;
  }
  .logo-mark path {
    fill: none;
    stroke: var(--accent);
    stroke-width: 3.2;
    stroke-linecap: round;
  }
  .logo-mark circle {
    fill: var(--bg);
  }
  .site-links {
    display: flex;
    gap: 24px;
  }
  .site-links a {
    color: var(--text-secondary);
    text-decoration: none;
    font-size: 14px;
  }
  .site-links a:hover {
    color: var(--text);
  }

  .intro {
    position: relative;
    overflow: hidden;
    border-top: 1px solid var(--border);
    border-bottom: 1px solid var(--border);
    background: radial-gradient(
      ellipse 70% 80% at 70% 20%,
      rgba(227, 173, 98, 0.06),
      transparent 70%
    );
  }
  .ripples {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    fill: none;
    stroke: var(--accent);
    pointer-events: none;
  }
  .ripples path {
    vector-effect: non-scaling-stroke;
  }
  .intro-content {
    position: relative;
    max-width: 960px;
    margin: 0 auto;
    padding: 56px 20px 64px;
  }
  h1 {
    margin: 0 0 12px;
    max-width: 640px;
    font-size: clamp(28px, 5vw, 42px);
    line-height: 1.15;
    letter-spacing: -0.03em;
  }
  .description {
    margin: 0;
    max-width: 620px;
    color: var(--text-secondary);
    font-size: 17px;
  }

  main {
    max-width: 960px;
    margin: 0 auto;
    padding: 32px 20px 64px;
  }

  .tab-bar {
    display: flex;
    border-bottom: 1px solid var(--border);
    margin-bottom: 24px;
    gap: 4px;
    overflow-x: auto;
  }
  .tab-btn {
    padding: 10px 18px;
    background: none;
    border: none;
    border-radius: 0;
    border-bottom: 2px solid transparent;
    margin-bottom: -1px;
    cursor: pointer;
    font-size: 15px;
    font-weight: 500;
    color: var(--text-muted);
    transition: all 0.15s;
  }
  .tab-btn.active {
    color: var(--text);
    border-bottom-color: var(--accent);
  }
  .tab-btn:hover:not(.active):not(:disabled) {
    color: var(--text-secondary);
    border-bottom-color: var(--border-hover);
  }
  .tab-btn:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }

  /* Shared global styles for child components */
  :global(textarea) {
    width: 100%;
    padding: 14px 16px;
    background: var(--surface);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 10px;
    font-size: 14px;
    line-height: 1.7;
    font-family: var(--font-mono);
    min-height: 180px;
    resize: vertical;
    tab-size: 4;
    margin-bottom: 12px;
    caret-color: var(--accent);
  }
  :global(textarea::placeholder) {
    color: var(--text-muted);
  }
  :global(textarea:focus) {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 3px var(--accent-ring);
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
    border: 1px solid transparent;
    border-radius: 8px;
    font-size: 14px;
    font-weight: 500;
    font-family: inherit;
    transition: background 0.15s, border-color 0.15s, color 0.15s;
  }
  :global(.btn-primary) {
    background: var(--accent);
    color: var(--bg);
  }
  :global(.btn-primary:hover:not(:disabled)) {
    background: var(--accent-hover);
  }
  :global(.btn-primary:disabled) {
    background: var(--surface-3);
    color: var(--text-muted);
    cursor: wait;
  }
  :global(.btn-secondary) {
    background: var(--surface-2);
    color: var(--text);
    border-color: var(--border);
  }
  :global(.btn-secondary:hover:not(:disabled)) {
    background: var(--surface-3);
    border-color: var(--border-hover);
  }
  :global(.btn-success) {
    background: var(--success-bg);
    color: var(--success);
    border-color: var(--success-border);
  }
  :global(.btn-success:hover:not(:disabled)) {
    background: rgba(169, 201, 138, 0.16);
  }
  :global(.btn-warning) {
    background: var(--accent-muted);
    color: var(--accent);
    border-color: var(--accent-ring);
  }
  :global(.btn-warning:hover:not(:disabled)) {
    background: rgba(227, 173, 98, 0.2);
  }
  :global(.btn-danger) {
    background: var(--danger-bg);
    color: var(--danger);
    border-color: var(--danger-border);
  }
  :global(.btn-danger:hover:not(:disabled)) {
    background: rgba(232, 137, 122, 0.16);
  }
  :global(button:disabled) {
    opacity: 0.6;
    cursor: not-allowed;
  }
  :global(.btn-sm) {
    padding: 6px 12px;
    font-size: 13px;
  }

  :global(.examples) {
    margin-bottom: 16px;
  }
  :global(.examples-label) {
    font-size: 12px;
    color: var(--text-muted);
    margin: 0 0 8px 0;
    font-weight: 500;
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }
  :global(.pill-row) {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  :global(.pill) {
    padding: 5px 14px;
    font-size: 13px;
    background: var(--surface-2);
    border: 1px solid var(--border);
    border-radius: 999px;
    color: var(--text-secondary);
    cursor: pointer;
    transition: all 0.1s;
  }
  :global(.pill:hover) {
    color: var(--text);
    border-color: var(--accent);
  }

  :global(.output-box) {
    background: #090705;
    color: var(--text-secondary);
    padding: 14px 16px;
    border: 1px solid var(--border);
    border-radius: 10px;
    font-family: var(--font-mono);
    font-size: 13px;
    white-space: pre-wrap;
    word-wrap: break-word;
    min-height: 60px;
    max-height: 400px;
    overflow-y: auto;
  }
  :global(.output-box.error-output) {
    color: var(--danger);
  }
  :global(.exec-time) {
    font-size: 12px;
    color: var(--text-muted);
    margin-top: 6px;
  }

  :global(.info-banner) {
    background: var(--accent-muted);
    border: 1px solid var(--accent-ring);
    border-radius: 10px;
    padding: 14px 18px;
    margin-bottom: 16px;
    font-size: 14px;
    color: var(--text-secondary);
  }
  :global(.info-banner strong) {
    color: var(--accent);
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

  @media (max-width: 640px) {
    .intro-content {
      padding: 36px 16px 44px;
    }
    main,
    .site-header {
      padding-left: 16px;
      padding-right: 16px;
    }
  }
</style>
