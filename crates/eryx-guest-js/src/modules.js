// Evaluated once after fetch.js, when the QuickJS context is created.
// Receives the native intrinsics (src/modules.rs) and installs, as
// non-enumerable globals:
//
// - `require(name)`: a synchronous import of an environment module (or a
//   Node built-in stub), for CommonJS-style code and bundles' `__require`.
//   A module with only a default export (a bundled CommonJS package)
//   returns that export, as `require` would in Node.
// - `__eryx_import(namespace, names, specifier)`: what the import rewrite
//   (src/modules.rs) destructures. A namespace with only a default export
//   also exposes the default's properties as named exports, so `import {
//   chunk } from "lodash"` works for a CommonJS package, as in Node. Each of
//   `names` must be an export, or it throws the SyntaxError an ES module
//   import would.
(native) => {
    const define = (name, value) =>
        Object.defineProperty(globalThis, name, { value, writable: true, configurable: true });

    const onlyDefault = (namespace) => {
        const names = Object.keys(namespace);
        const value = namespace.default;
        return (
            names.length === 1 &&
            names[0] === 'default' &&
            value !== null &&
            (typeof value === 'object' || typeof value === 'function')
        );
    };

    const interop = (namespace) => {
        if (!onlyDefault(namespace)) return namespace;
        const value = namespace.default;
        return new Proxy(value, {
            get: (target, key) => (key === 'default' ? value : Reflect.get(target, key)),
            has: (target, key) => key === 'default' || Reflect.has(target, key),
        });
    };

    const checkedImport = (namespace, names, specifier) => {
        const imported = interop(namespace);
        for (const name of names) {
            if (!(name in imported)) {
                throw new SyntaxError(
                    `The requested module '${specifier}' does not provide an export named '${name}'`,
                );
            }
        }
        return imported;
    };

    function require(name) {
        const namespace = native.requireSync(String(name));
        return onlyDefault(namespace) ? namespace.default : namespace;
    }

    define('require', require);
    define('__eryx_import', checkedImport);
};
