/**
 * Let sandboxed code read a local directory, read-only, without uploading it:
 * files are served from this process as the code reads them. `.env`, keys
 * and the like are hidden unless you say otherwise.
 *
 *     ERYX_API_KEY=eryx_... node examples/volumes.ts [directory]
 */

import { Sandbox } from "@eryx-org/eryx-run";

const sandbox = new Sandbox({
  volumes: [{ hostPath: process.argv[2] ?? ".", guestPath: "/mnt/repo" }],
});

const result = await sandbox.execute(`
from collections import Counter
from pathlib import Path

files = [p for p in Path("/mnt/repo").rglob("*") if p.is_file() and "node_modules" not in p.parts]
by_suffix = Counter(p.suffix or p.name for p in files)
lines = sum(len(p.read_bytes().splitlines()) for p in files if p.suffix in {".ts", ".md", ".json"})

print(f"{len(files)} files, {lines} lines of .ts/.md/.json")
for suffix, count in by_suffix.most_common(5):
    print(f"  {suffix}: {count}")
`);

console.log(result.stdout.trimEnd());
