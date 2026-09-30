// Prepares OpenCascade (occt-import-js) for the thumbnail provider, which runs it in wasmtime:
//  1. restores the Emscripten import/export names that the minifier shortened (read from the
//     package's JS glue), so the DLL can bind them without that glue;
//  2. translates legacy wasm exception handling (try/catch) to the standardized exnref form
//     with Binaryen, because wasmtime only implements the latter.
//
// node scripts/build-occt.mjs [out.wasm]   (default: target/occt/occt.wasm)

import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const dist = join(root, "viewer/node_modules/occt-import-js/dist");
const out = resolve(process.argv[2] ?? join(root, "target/occt/occt.wasm"));

const glue = readFileSync(join(dist, "occt-import-js.js"), "utf8");
const wasm = readFileSync(join(dist, "occt-import-js.wasm"));

// `var wasmImports={M:___syscall_chmod,...}`: JS function names carry one extra leading "_".
const importBlock = glue.match(/var wasmImports=\{([^}]*)\}/);
if (!importBlock) throw new Error("wasmImports not found in occt-import-js.js");
const importNames = new Map(
    [...importBlock[1].matchAll(/([\w$]+):([\w$]+)/g)].map(([, short, js]) => [short, js.slice(1)]),
);

// `_malloc=wasmExports["ca"]`, `wasmMemory=wasmExports["_"]`, ...
const exportRenames = { wasmMemory: "memory", wasmTable: "__indirect_function_table" };
const exportNames = new Map();
for (const [, js, short] of glue.matchAll(/([\w$]+)=wasmExports\["([\w$]+)"\]/g)) {
    const name = exportRenames[js] ?? (js.startsWith("_") ? js.slice(1) : null);
    if (name) exportNames.set(short, name);
}
for (const required of ["memory", "__indirect_function_table", "malloc", "free", "__wasm_call_ctors"]) {
    if (![...exportNames.values()].includes(required)) throw new Error(`export ${required} not found in glue`);
}

// --- minimal wasm binary rewriting: only the names in the import and export sections change ---

function readLeb(buf, pos) {
    let result = 0, shift = 0, byte;
    do {
        byte = buf[pos++];
        result += (byte & 0x7f) * 2 ** shift;
        shift += 7;
    } while (byte & 0x80);
    return [result, pos];
}

function leb(n) {
    const bytes = [];
    do {
        let byte = n % 128;
        n = Math.floor(n / 128);
        if (n > 0) byte |= 0x80;
        bytes.push(byte);
    } while (n > 0);
    return Buffer.from(bytes);
}

function readName(buf, pos) {
    const [len, p] = readLeb(buf, pos);
    return [buf.toString("utf8", p, p + len), p + len];
}

const encodeName = (s) => Buffer.concat([leb(Buffer.byteLength(s)), Buffer.from(s)]);

/** Returns the end offset of an import/export descriptor starting at `pos` (after the kind byte). */
function skipImportDesc(buf, kind, pos) {
    switch (kind) {
        case 0: return readLeb(buf, pos)[1]; // func: type index
        case 1: { // table: reftype + limits
            pos++;
            const flags = buf[pos++];
            pos = readLeb(buf, pos)[1];
            return flags & 1 ? readLeb(buf, pos)[1] : pos;
        }
        case 2: { // memory: limits
            const flags = buf[pos++];
            pos = readLeb(buf, pos)[1];
            return flags & 1 ? readLeb(buf, pos)[1] : pos;
        }
        case 3: return pos + 2; // global: valtype + mutability
        case 4: return readLeb(buf, pos + 1)[1]; // tag: attribute + type index
        default: throw new Error(`unknown import kind ${kind}`);
    }
}

function rewriteImports(body) {
    let [count, pos] = readLeb(body, 0);
    const parts = [leb(count)];
    for (let i = 0; i < count; i++) {
        let module, field;
        [module, pos] = readName(body, pos);
        [field, pos] = readName(body, pos);
        const kind = body[pos];
        const end = skipImportDesc(body, kind, pos + 1);
        const name = kind === 0 ? importNames.get(field) : field;
        if (!name) throw new Error(`no name for import ${module}.${field}`);
        parts.push(encodeName("env"), encodeName(name), body.subarray(pos, end));
        pos = end;
    }
    return Buffer.concat(parts);
}

function rewriteExports(body) {
    let [count, pos] = readLeb(body, 0);
    const parts = [leb(count)];
    for (let i = 0; i < count; i++) {
        let field;
        [field, pos] = readName(body, pos);
        const kind = body[pos];
        const end = readLeb(body, pos + 1)[1];
        parts.push(encodeName(exportNames.get(field) ?? field), body.subarray(pos, end));
        pos = end;
    }
    return Buffer.concat(parts);
}

const sections = [wasm.subarray(0, 8)];
for (let pos = 8; pos < wasm.length; ) {
    const id = wasm[pos];
    const [size, bodyStart] = readLeb(wasm, pos + 1);
    let body = wasm.subarray(bodyStart, bodyStart + size);
    if (id === 2) body = rewriteImports(body);
    if (id === 7) body = rewriteExports(body);
    sections.push(Buffer.from([id]), leb(body.length), body);
    pos = bodyStart + size;
}

mkdirSync(dirname(out), { recursive: true });
const renamed = out + ".renamed.tmp";
writeFileSync(renamed, Buffer.concat(sections));

const require = createRequire(import.meta.url);
const wasmOpt = join(dirname(require.resolve("binaryen/package.json")), "bin/wasm-opt");
try {
    execFileSync(
        process.execPath,
        [
            wasmOpt,
            "--detect-features",
            "--enable-reference-types",
            "--enable-multivalue",
            "--enable-exception-handling",
            "--translate-to-exnref",
            renamed,
            "-o",
            out,
        ],
        { stdio: "inherit" },
    );
} finally {
    rmSync(renamed, { force: true });
}
console.log(`wrote ${out}`);
