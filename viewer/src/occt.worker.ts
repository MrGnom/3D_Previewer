/// <reference lib="webworker" />
// Converts CAD files (STEP, IGES, BREP) to triangle meshes with OpenCascade, off the UI thread.
import occtimportjs, { type Occt, type OcctColor, type OcctFormat, type OcctMesh, type OcctNode, type OcctParams } from "occt-import-js";

export type CadColor = [number, number, number];

/** One tessellated solid/shell, already in Babylon's Y-up space. Colors are linear RGB. */
export interface CadPart {
    name: string;
    color: CadColor | null;
    positions: Float32Array;
    normals: Float32Array | null;
    indices: Uint32Array;
    /** Per-vertex RGBA, only when individual B-rep faces carry their own colors. */
    colors: Float32Array | null;
}

export type CadRequest =
    | { type: "init"; wasm: ArrayBuffer }
    | { type: "convert"; id: number; format: OcctFormat; data: ArrayBuffer; params: OcctParams };

export type CadResponse =
    | { type: "ready" }
    | { type: "init-failed"; error: string }
    | { type: "converted"; id: number; root: OcctNode; parts: CadPart[] }
    /** `fatal`: the WebAssembly runtime aborted and this worker can't be reused. */
    | { type: "failed"; id: number; error: string; fatal: boolean };

/** Linear-space match for the viewer's default material. */
const DEFAULT_COLOR: CadColor = [0.5, 0.51, 0.53];

const post = (msg: CadResponse, transfer: Transferable[] = []) => self.postMessage(msg, transfer);

function toLinear(c: OcctColor): CadColor {
    const lin = (v: number) => (v <= 0.04045 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4));
    return [lin(c[0]), lin(c[1]), lin(c[2])];
}

/** Z-up (CAD) to Y-up (Babylon), same swap as Babylon's STL loader. */
function swapYZ(src: number[]): Float32Array {
    const out = new Float32Array(src.length);
    for (let i = 0; i < src.length; i += 3) {
        out[i] = src[i];
        out[i + 1] = src[i + 2];
        out[i + 2] = src[i + 1];
    }
    return out;
}

function toPart(mesh: OcctMesh): CadPart {
    const color = mesh.color ? toLinear(mesh.color) : null;
    const indices = Uint32Array.from(mesh.index.array);
    const vertexCount = mesh.attributes.position.array.length / 3;

    let colors: Float32Array | null = null;
    if (mesh.brep_faces.some((f) => f.color)) {
        colors = new Float32Array(vertexCount * 4);
        const base = color ?? DEFAULT_COLOR;
        for (let v = 0; v < vertexCount; v++) {
            colors.set(base, v * 4);
            colors[v * 4 + 3] = 1;
        }
        for (const face of mesh.brep_faces) {
            if (!face.color) continue;
            const c = toLinear(face.color);
            for (let i = face.first * 3; i <= face.last * 3 + 2; i++) {
                colors.set(c, indices[i] * 4);
            }
        }
    }

    return {
        name: mesh.name,
        color,
        positions: swapYZ(mesh.attributes.position.array),
        normals: mesh.attributes.normal ? swapYZ(mesh.attributes.normal.array) : null,
        indices,
        colors,
    };
}

let occt: Occt | null = null;

self.onmessage = async (ev: MessageEvent<CadRequest>) => {
    const req = ev.data;
    if (req.type === "init") {
        try {
            occt = await occtimportjs({ wasmBinary: req.wasm });
            post({ type: "ready" });
        } catch (e) {
            post({ type: "init-failed", error: e instanceof Error ? e.message : String(e) });
        }
        return;
    }

    try {
        if (!occt) throw new Error("OpenCascade isn't initialized.");
        const result = occt.ReadFile(req.format, new Uint8Array(req.data), req.params);
        if (!result.success) {
            throw new Error(`The ${req.format.toUpperCase()} file could not be parsed.`);
        }
        const parts = result.meshes.map(toPart);
        const transfer = parts.flatMap((p) =>
            [p.positions, p.normals, p.indices, p.colors].filter((a) => a !== null).map((a) => a.buffer),
        );
        post({ type: "converted", id: req.id, root: result.root, parts }, transfer);
    } catch (e) {
        const fatal = !occt || e instanceof WebAssembly.RuntimeError;
        post({ type: "failed", id: req.id, error: e instanceof Error ? e.message : String(e), fatal });
    }
};
