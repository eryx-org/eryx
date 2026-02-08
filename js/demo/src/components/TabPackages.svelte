<script lang="ts">
  interface Package {
    name: string;
    version: string;
    size: number;
  }

  let packages: Package[] = $state([]);
  let dragover = $state(false);

  function handleFiles(files: FileList) {
    for (const f of files) {
      if (!f.name.endsWith(".whl")) continue;
      const parts = f.name.replace(".whl", "").split("-");
      packages.push({
        name: parts[0],
        version: parts[1] || "?",
        size: f.size,
      });
    }
    packages = [...packages]; // trigger reactivity
  }

  function handleDrop(e: DragEvent) {
    e.preventDefault();
    dragover = false;
    if (e.dataTransfer?.files) handleFiles(e.dataTransfer.files);
  }

  let fileInput: HTMLInputElement;
</script>

<div class="info-banner">
  <strong>Coming soon.</strong> Package installation is available in the Rust and
  Python SDKs via <code>SandboxFactory</code>. Browser-side package loading is
  planned.
</div>

<div
  class="upload-zone"
  class:dragover
  role="button"
  tabindex="0"
  onclick={() => fileInput.click()}
  onkeydown={(e) => e.key === "Enter" && fileInput.click()}
  ondragover={(e) => {
    e.preventDefault();
    dragover = true;
  }}
  ondragleave={() => (dragover = false)}
  ondrop={handleDrop}
>
  <p><strong>Drop .whl files here</strong> or click to browse</p>
  <p class="small">Pure Python wheels (py3-none-any) are supported</p>
  <input
    bind:this={fileInput}
    type="file"
    accept=".whl"
    multiple
    onchange={(e) => {
      const input = e.target as HTMLInputElement;
      if (input.files) handleFiles(input.files);
    }}
  />
</div>

{#if packages.length > 0}
  <h2 class="pkg-title">Uploaded Packages</h2>
  {#each packages as pkg}
    <div class="pkg-row">
      <strong>{pkg.name}</strong>
      {pkg.version}
      <span class="pkg-size">{(pkg.size / 1024).toFixed(1)} KB</span>
      <span class="pkg-status">Installation API coming soon</span>
    </div>
  {/each}
{/if}

<p class="pkg-hint">
  Example packages that work with eryx: <strong>requests</strong>,
  <strong>jinja2</strong>, <strong>pyyaml</strong>, <strong>httpx</strong>,
  <strong>beautifulsoup4</strong> (pure Python wheels only).
</p>

<style>
  .upload-zone {
    border: 2px dashed #ccc;
    border-radius: 8px;
    padding: 40px 20px;
    text-align: center;
    color: #737373;
    margin-bottom: 16px;
    transition: all 0.2s;
    cursor: pointer;
  }
  .upload-zone:hover,
  .upload-zone.dragover {
    border-color: #007bff;
    color: #007bff;
    background: #f0f7ff;
  }
  .upload-zone input[type="file"] {
    display: none;
  }
  .upload-zone p {
    margin: 0 0 8px;
  }
  .upload-zone .small {
    font-size: 13px;
    margin: 0;
  }
  .pkg-title {
    font-size: 15px;
    margin-bottom: 8px;
  }
  .pkg-row {
    padding: 8px 12px;
    background: #f1f3f5;
    border-radius: 6px;
    margin-bottom: 6px;
    font-size: 14px;
  }
  .pkg-size {
    color: #737373;
    margin-left: 8px;
  }
  .pkg-status {
    color: #737373;
    float: right;
  }
  .pkg-hint {
    font-size: 13px;
    color: #737373;
    margin-top: 16px;
  }
</style>
