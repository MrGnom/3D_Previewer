// Babylon.js loader plugin for CAD files (STEP, IGES, BREP), backed by OpenCascade (occt-import-js).
// The ~8 MB .wasm is a separate asset, fetched only when the first CAD file is opened, and the
// conversion runs in a Web Worker so the UI (and the Explorer preview pane) stays responsive.
import {
    AssetContainer,
    Color3,
    Mesh,
    PBRMaterial,
    RegisterSceneLoaderPlugin,
    TransformNode,
    VertexData,
    type ISceneLoaderAsyncResult,
    type ISceneLoaderPluginAsync,
    type Scene,
} from "@babylonjs/core";
import type { OcctFormat, OcctNode, OcctParams } from "occt-import-js";
import wasmUrl from "occt-import-js/dist/occt-import-js.wasm?url";
import type { CadPart, CadRequest, CadResponse } from "./occt.worker";
// Inlined as a blob: the preview pane serves files through WebView2 request interception,
// which a worker's own requests can't rely on. The .wasm is fetched here and handed over instead.
import CadWorker from "./occt.worker?worker&inline";

const FORMATS: Record<string, OcctFormat> = {
    ".step": "step",
    ".stp": "step",
    ".iges": "iges",
    ".igs": "iges",
    ".brep": "brep",
};

export const CAD_EXTENSIONS = Object.keys(FORMATS);

/** Tessellation quality: "fast" for the preview pane, "fine" for the app. */
export type CadQuality = "fast" | "fine";

const QUALITY: Record<CadQuality, OcctParams> = {
    fast: { linearUnit: "millimeter", linearDeflectionType: "bounding_box_ratio", linearDeflection: 0.004, angularDeflection: 0.8 },
    fine: { linearUnit: "millimeter", linearDeflectionType: "bounding_box_ratio", linearDeflection: 0.001, angularDeflection: 0.5 },
};

let quality: CadQuality = "fine";

export function setCadQuality(q: CadQuality): void {
    quality = q;
}

interface Converted {
    root: OcctNode;
    parts: CadPart[];
}

/** Owns the OpenCascade worker: started on first use, kept for later files, restarted after a crash. */
class CadConverter {
    private worker: Promise<Worker> | null = null;
    private live: Worker | null = null;
    private nextId = 0;
    private readonly pending = new Map<number, { resolve(c: Converted): void; reject(e: Error): void }>();

    async convert(format: OcctFormat, data: ArrayBuffer, params: OcctParams): Promise<Converted> {
        const worker = await this.start();
        const id = ++this.nextId;
        return new Promise((resolve, reject) => {
            this.pending.set(id, { resolve, reject });
            const req: CadRequest = { type: "convert", id, format, data, params };
            worker.postMessage(req, [data]);
        });
    }

    private start(): Promise<Worker> {
        if (!this.worker) {
            const worker = (this.worker = this.spawn());
            worker.catch(() => {
                if (this.worker === worker) this.worker = null;
            });
        }
        return this.worker;
    }

    private async spawn(): Promise<Worker> {
        const response = await fetch(wasmUrl);
        if (!response.ok) {
            throw new Error(`Couldn't load the CAD kernel (HTTP ${response.status}).`);
        }
        const wasm = await response.arrayBuffer();
        const worker = new CadWorker();
        try {
            await new Promise<void>((resolve, reject) => {
                worker.onmessage = (ev: MessageEvent<CadResponse>) => {
                    if (ev.data.type === "ready") resolve();
                    else if (ev.data.type === "init-failed") reject(new Error(`Couldn't start the CAD kernel: ${ev.data.error}`));
                };
                worker.onerror = (e) => reject(new Error(e.message || "Couldn't start the CAD kernel."));
                const req: CadRequest = { type: "init", wasm };
                worker.postMessage(req, [wasm]);
            });
        } catch (e) {
            worker.terminate();
            throw e;
        }
        worker.onmessage = (ev: MessageEvent<CadResponse>) => this.onMessage(ev.data);
        worker.onerror = (e) => this.reset(new Error(e.message || "The CAD kernel crashed."));
        this.live = worker;
        return worker;
    }

    private onMessage(msg: CadResponse): void {
        if (msg.type !== "converted" && msg.type !== "failed") return;
        const job = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.type === "converted") {
            job?.resolve({ root: msg.root, parts: msg.parts });
        } else {
            job?.reject(new Error(msg.error));
            if (msg.fatal) this.reset(new Error("The CAD kernel stopped after an earlier error."));
        }
    }

    private reset(error: Error): void {
        this.live?.terminate();
        this.live = null;
        this.worker = null;
        for (const job of this.pending.values()) job.reject(error);
        this.pending.clear();
    }
}

const converter = new CadConverter();

function formatOf(fileName: string | undefined): OcctFormat {
    const clean = (fileName ?? "").split(/[?#]/)[0].toLowerCase();
    const ext = clean.substring(clean.lastIndexOf("."));
    return FORMATS[ext] ?? "step";
}

function createMaterial(name: string, scene: Scene, color: Color3): PBRMaterial {
    const mat = new PBRMaterial(name, scene);
    mat.albedoColor = color;
    mat.metallic = 0.0;
    mat.roughness = 0.45;
    // CAD exports often contain open shells or inconsistently oriented faces.
    mat.backFaceCulling = false;
    mat.twoSidedLighting = true;
    return mat;
}

/**
 * Builds the assembly tree: a TransformNode per OCCT node (keeping part names) and a Mesh per
 * tessellated part. OCCT has already applied the assembly placements to the vertices.
 */
function buildModel(scene: Scene, converted: Converted, fileName: string): { root: TransformNode; meshes: Mesh[]; nodes: TransformNode[] } {
    const materials = new Map<string, PBRMaterial>();
    let vertexColorMaterial: PBRMaterial | null = null;
    const meshes: Mesh[] = [];
    const nodes: TransformNode[] = [];

    const materialFor = (part: CadPart): PBRMaterial | null => {
        if (part.colors) {
            return (vertexColorMaterial ??= createMaterial("cad-vertex-colors", scene, Color3.White()));
        }
        if (!part.color) {
            return null; // The viewer assigns its default material.
        }
        const key = part.color.map((c) => c.toFixed(4)).join(",");
        let mat = materials.get(key);
        if (!mat) {
            mat = createMaterial(`cad-color-${materials.size}`, scene, new Color3(...part.color));
            materials.set(key, mat);
        }
        return mat;
    };

    const visit = (node: OcctNode, parent: TransformNode | null, fallbackName: string): TransformNode => {
        const tn = new TransformNode(node.name || fallbackName, scene);
        tn.parent = parent;
        nodes.push(tn);
        for (const index of node.meshes) {
            const part = converted.parts[index];
            if (!part || part.indices.length === 0) continue;
            const name = node.meshes.length === 1 ? node.name || part.name : part.name || node.name;
            const mesh = new Mesh(name || `Part ${index + 1}`, scene);
            const data = new VertexData();
            data.positions = part.positions;
            data.indices = part.indices;
            if (part.normals) {
                data.normals = part.normals;
            } else {
                const normals = new Float32Array(part.positions.length);
                VertexData.ComputeNormals(part.positions, part.indices, normals);
                data.normals = normals;
            }
            if (part.colors) {
                data.colors = part.colors;
            }
            data.applyToMesh(mesh);
            mesh.material = materialFor(part);
            mesh.parent = tn;
            meshes.push(mesh);
        }
        node.children.forEach((child, i) => visit(child, tn, `${tn.name} ${i + 1}`));
        return tn;
    };

    const root = visit(converted.root, null, fileName);
    return { root, meshes, nodes };
}

class CadFileLoader implements ISceneLoaderPluginAsync {
    readonly name = "occt-cad";
    readonly extensions = Object.fromEntries(CAD_EXTENSIONS.map((e) => [e, { isBinary: true }]));

    async importMeshAsync(
        _meshesNames: unknown,
        scene: Scene,
        data: unknown,
        _rootUrl: string,
        _onProgress?: unknown,
        fileName?: string,
    ): Promise<ISceneLoaderAsyncResult> {
        const { meshes, nodes } = await this.build(scene, data, fileName, false);
        return {
            meshes,
            transformNodes: nodes,
            particleSystems: [],
            skeletons: [],
            animationGroups: [],
            geometries: [],
            lights: [],
            spriteManagers: [],
        };
    }

    async loadAsync(scene: Scene, data: unknown, rootUrl: string, _onProgress?: unknown, fileName?: string): Promise<void> {
        await this.importMeshAsync(null, scene, data, rootUrl, undefined, fileName);
    }

    async loadAssetContainerAsync(scene: Scene, data: unknown, _rootUrl: string, _onProgress?: unknown, fileName?: string): Promise<AssetContainer> {
        const { root } = await this.build(scene, data, fileName, true);
        const container = new AssetContainer(scene);
        container.addAllAssetsToContainer(root);
        return container;
    }

    private async build(scene: Scene, data: unknown, fileName: string | undefined, forContainer: boolean) {
        if (!(data instanceof ArrayBuffer)) {
            throw new Error("Expected binary CAD data.");
        }
        const converted = await converter.convert(formatOf(fileName), data, QUALITY[quality]);
        if (converted.parts.every((p) => p.indices.length === 0)) {
            throw new Error("The file contains no solid or surface geometry.");
        }
        // Same trick as Babylon's own loaders: keep new entities out of the scene until the
        // container adds them.
        const s = scene as unknown as { _blockEntityCollection: boolean };
        s._blockEntityCollection = forContainer;
        try {
            return buildModel(scene, converted, fileName ?? "Model");
        } finally {
            s._blockEntityCollection = false;
        }
    }
}

RegisterSceneLoaderPlugin(new CadFileLoader());
