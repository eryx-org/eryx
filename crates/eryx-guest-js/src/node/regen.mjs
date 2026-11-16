// Called by regen.sh: node regen.mjs <node_modules> <out dir>.
import { execFileSync } from 'node:child_process';
import { copyFileSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { join } from 'node:path';

const [modulesDir, outDir] = process.argv.slice(2);
const require = createRequire(join(modulesDir, 'x.js'));

// Built-in name -> the npm package that implements it, the packages bundled
// into it (for their licences), and what it leaves to another built-in.
const builtins = {
    buffer: { entry: 'buffer', bundled: ['buffer', 'base64-js', 'ieee754'], external: [] },
    events: { entry: 'events', bundled: ['events'], external: [] },
    path: { entry: 'path-browserify', bundled: ['path-browserify'], external: [] },
    string_decoder: {
        entry: 'string_decoder',
        bundled: ['string_decoder', 'safe-buffer'],
        // safe-buffer's `require("buffer")` goes to node:buffer, so both
        // share one Buffer.
        external: ['buffer'],
    },
};

const IDENTIFIER = /^[A-Za-z_$][\w$]*$/;
const ESBUILD = '0.28.2';

const licenceFile = (pkg) => {
    const dir = join(modulesDir, pkg);
    const name = readdirSync(dir).find((f) => /^licen[cs]e/i.test(f));
    if (!name) throw new Error(`no licence file for ${pkg}`);
    return join(dir, name);
};

const versions = [];
const notice = (pkg) => {
    const { version } = JSON.parse(readFileSync(join(modulesDir, pkg, 'package.json'), 'utf8'));
    versions.push(`${pkg}@${version}`);
    const file = licenceFile(pkg);
    copyFileSync(file, join(outDir, 'licenses', `${pkg}-LICENSE`));
    const text = readFileSync(file, 'utf8').replaceAll('*/', '* /').trim();
    return `${pkg}@${version}\n\n${text}`;
};

for (const [name, { entry, bundled, external }] of Object.entries(builtins)) {
    let code = execFileSync(
        'npx',
        [
            '--yes',
            `esbuild@${ESBUILD}`,
            join(modulesDir, entry),
            '--bundle',
            '--format=iife',
            '--global-name=__m',
            '--platform=browser',
            '--minify',
            '--legal-comments=none',
            ...external.map((e) => `--external:${e}`),
        ],
        { encoding: 'utf8' },
    );
    // An external built-in is a `node:` import, not a call to the global
    // `require`, which an environment's own package of that name would win.
    const imports = external.map((name) => {
        const calls = code.match(new RegExp(`[A-Za-z_$][\\w$]*\\("${name}"\\)`, 'g')) ?? [];
        if (calls.length !== 1) throw new Error(`expected one require("${name}"), found ${calls.length}`);
        const local = `__node_${name}`;
        code = code.replace(calls[0], local);
        return `import ${local} from "node:${name}";`;
    });
    // Named exports, so a static `import { Buffer } from "node:buffer"` (as
    // bundled environment modules have) links.
    const names = Object.keys(require(join(modulesDir, entry))).filter(
        (key) => IDENTIFIER.test(key) && key !== 'default',
    );
    const notices = bundled.map(notice);
    const out = [
        `/*! node:${name}, vendored by regen.sh. It contains:\n\n${notices.join('\n\n---\n\n')}\n*/`,
        ...imports,
        code.trim(),
        'export default __m;',
        names.length > 0 ? `export const { ${names.join(', ')} } = __m;` : '',
        '',
    ].join('\n');
    writeFileSync(join(outDir, `${name}.js`), out);
}
// The `URL` and `URLSearchParams` globals (src/web.js installs them): core-js's
// pure build, which defines no globals and patches no built-ins, as one
// expression evaluating to `{ URL, URLSearchParams }`.
{
    const entry = join(outDir, '.url-entry.mjs');
    writeFileSync(
        entry,
        `export { default as URL } from ${JSON.stringify(join(modulesDir, 'core-js-pure/stable/url/index.js'))};\n` +
            `export { default as URLSearchParams } from ${JSON.stringify(join(modulesDir, 'core-js-pure/stable/url-search-params/index.js'))};\n`,
    );
    const code = execFileSync(
        'npx',
        [
            '--yes',
            `esbuild@${ESBUILD}`,
            entry,
            '--bundle',
            '--format=iife',
            '--global-name=__m',
            '--platform=neutral',
            '--main-fields=module,main',
            '--minify',
            '--legal-comments=none',
        ],
        { encoding: 'utf8' },
    );
    rmSync(entry);
    const out = [
        `/*! URL and URLSearchParams, vendored by regen.sh. It contains:\n\n${notice('core-js-pure')}\n*/`,
        // A function body, so esbuild's `var __m` stays local.
        `(() => {`,
        code.trim(),
        `return __m;`,
        `})()`,
        '',
    ].join('\n');
    writeFileSync(join(outDir, 'url_globals.js'), out);
}
console.log(versions.join('\n'));
