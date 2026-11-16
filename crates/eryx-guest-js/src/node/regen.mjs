// Called by regen.sh: node regen.mjs <node_modules> <out dir>.
import { execFileSync } from 'node:child_process';
import { copyFileSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
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

const licenceFile = (pkg) => {
    const dir = join(modulesDir, pkg);
    const name = readdirSync(dir).find((f) => /^licen[cs]e/i.test(f));
    if (!name) throw new Error(`no licence file for ${pkg}`);
    return join(dir, name);
};

const versions = [];
for (const [name, { entry, bundled, external }] of Object.entries(builtins)) {
    const code = execFileSync(
        'esbuild',
        [
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
    // Named exports, so a static `import { Buffer } from "node:buffer"` (as
    // bundled environment modules have) links.
    const names = Object.keys(require(join(modulesDir, entry))).filter(
        (key) => IDENTIFIER.test(key) && key !== 'default',
    );
    const notices = bundled.map((pkg) => {
        const { version } = JSON.parse(readFileSync(join(modulesDir, pkg, 'package.json'), 'utf8'));
        versions.push(`${pkg}@${version}`);
        const file = licenceFile(pkg);
        copyFileSync(file, join(outDir, 'licenses', `${pkg}-LICENSE`));
        const text = readFileSync(file, 'utf8').replaceAll('*/', '* /').trim();
        return `${pkg}@${version}\n\n${text}`;
    });
    const out = [
        `/*! node:${name}, vendored by regen.sh. It contains:\n\n${notices.join('\n\n---\n\n')}\n*/`,
        code.trim(),
        'export default __m;',
        names.length > 0 ? `export const { ${names.join(', ')} } = __m;` : '',
        '',
    ].join('\n');
    writeFileSync(join(outDir, `${name}.js`), out);
}
console.log(versions.join('\n'));
