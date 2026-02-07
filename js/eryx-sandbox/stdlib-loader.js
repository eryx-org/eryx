/**
 * Python stdlib loader for browser/Node.js environments.
 *
 * Fetches python-stdlib.tar.gz, decompresses it, parses the tar archive,
 * and returns a file tree compatible with the @bytecodealliance/preview2-shim
 * filesystem.
 */

/**
 * Parse a tar archive from a Uint8Array into a preview2-shim file tree.
 *
 * @param {Uint8Array} tarData - Raw tar file bytes
 * @returns {object} File tree with { dir: { ... } } structure
 */
function parseTar(tarData) {
  const root = { dir: {} };
  let offset = 0;

  while (offset + 512 <= tarData.length) {
    // Read 512-byte header
    const header = tarData.subarray(offset, offset + 512);

    // Check for end-of-archive (two consecutive zero blocks)
    if (header.every((b) => b === 0)) break;

    // Extract filename: prefix (bytes 345-499) + name (bytes 0-99)
    const prefix = readString(header, 345, 155);
    const name = readString(header, 0, 100);
    const fullName = prefix ? `${prefix}/${name}` : name;

    // File size in octal (bytes 124-135)
    const sizeStr = readString(header, 124, 12);
    const size = parseInt(sizeStr, 8) || 0;

    // Type flag (byte 156): '0' or '\0' = file, '5' = directory
    const typeFlag = header[156];

    offset += 512; // Move past header

    if (fullName && fullName !== "pax_global_header") {
      // Normalize path: strip leading ./ and trailing /
      const cleanPath = fullName.replace(/^\.\//, "").replace(/\/$/, "");

      if (cleanPath) {
        if (typeFlag === 53 /* '5' = directory */) {
          ensureDir(root, cleanPath);
        } else if (
          typeFlag === 48 /* '0' = regular file */ ||
          typeFlag === 0 /* '\0' = regular file (old format) */
        ) {
          const content = tarData.slice(offset, offset + size);
          setFile(root, cleanPath, content);
        }
      }
    }

    // Advance past file data (padded to 512-byte boundary)
    offset += Math.ceil(size / 512) * 512;
  }

  return root;
}

/** Read a null-terminated string from a buffer at the given offset. */
function readString(buf, start, maxLen) {
  let end = start;
  while (end < start + maxLen && buf[end] !== 0) end++;
  return new TextDecoder().decode(buf.subarray(start, end));
}

/** Ensure a directory path exists in the file tree. */
function ensureDir(root, path) {
  const parts = path.split("/");
  let current = root;
  for (const part of parts) {
    if (!part) continue;
    if (!current.dir) current.dir = {};
    if (!current.dir[part]) current.dir[part] = { dir: {} };
    current = current.dir[part];
  }
  return current;
}

/** Set a file at the given path in the file tree. */
function setFile(root, path, content) {
  const parts = path.split("/");
  const fileName = parts.pop();
  const dir = parts.length > 0 ? ensureDir(root, parts.join("/")) : root;
  if (!dir.dir) dir.dir = {};
  dir.dir[fileName] = { source: content };
}

/**
 * Read and decompress the stdlib tar.gz in Node.js using fs and zlib.
 * @param {string} filePath - Absolute path to the tar.gz file
 * @returns {Promise<Uint8Array>} Decompressed tar data
 */
async function loadTarGzNode(filePath) {
  const { readFileSync } = await import(/* webpackIgnore: true */ "node:fs");
  const { gunzipSync } = await import(/* webpackIgnore: true */ "node:zlib");
  const compressed = readFileSync(filePath);
  return gunzipSync(compressed);
}

/**
 * Read and decompress the stdlib tar.gz in the browser using fetch + DecompressionStream.
 * @param {string} url - URL to the tar.gz file
 * @returns {Promise<Uint8Array>} Decompressed tar data
 */
async function loadTarGzBrowser(url) {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(
      `Failed to fetch Python stdlib: ${response.status} ${response.statusText}`,
    );
  }

  if (typeof DecompressionStream !== "undefined") {
    const ds = new DecompressionStream("gzip");
    const decompressed = response.body.pipeThrough(ds);
    const reader = decompressed.getReader();
    const chunks = [];
    let totalLen = 0;
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      chunks.push(value);
      totalLen += value.length;
    }
    const tarData = new Uint8Array(totalLen);
    let pos = 0;
    for (const chunk of chunks) {
      tarData.set(chunk, pos);
      pos += chunk.length;
    }
    return tarData;
  }

  // Fallback: try node:zlib
  const buf = new Uint8Array(await response.arrayBuffer());
  const { gunzipSync } = await import(/* webpackIgnore: true */ "node:zlib");
  return gunzipSync(buf);
}

/**
 * Fetch and decompress python-stdlib.tar.gz, returning a file tree.
 *
 * @param {string|URL} [url] - URL to python-stdlib.tar.gz. Defaults to
 *   './python-stdlib.tar.gz' relative to this module.
 * @returns {Promise<object>} File tree with { dir: { ... } } structure
 */
export async function loadStdlib(url) {
  const stdlibUrl =
    url || new URL("./python-stdlib.tar.gz", import.meta.url).href;

  let tarData;

  // In Node.js, use fs + zlib for file:// URLs (fetch doesn't support file://)
  const isNode =
    typeof globalThis.process !== "undefined" &&
    typeof globalThis.process.versions?.node !== "undefined";

  if (isNode && stdlibUrl.startsWith("file://")) {
    const { fileURLToPath } = await import(
      /* webpackIgnore: true */ "node:url"
    );
    const filePath = fileURLToPath(stdlibUrl);
    tarData = await loadTarGzNode(filePath);
  } else {
    tarData = await loadTarGzBrowser(stdlibUrl);
  }

  return parseTar(tarData);
}
