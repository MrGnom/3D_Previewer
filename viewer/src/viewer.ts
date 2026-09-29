import {
    AbstractMesh,
    ArcRotateCamera,
    AssetContainer,
    Color3,
    Color4,
    DirectionalLight,
    Engine,
    FilesInputStore,
    HDRCubeTexture,
    HemisphericLight,
    ImageProcessingConfiguration,
    LoadAssetContainerAsync,
    Mesh,
    MeshBuilder,
    PBRMaterial,
    Scene,
    Vector3,
} from "@babylonjs/core";
import { registerBuiltInLoaders } from "@babylonjs/loaders/dynamic";
import { GridMaterial } from "@babylonjs/materials/grid/gridMaterial";
import { createStudioEnvironmentUrl } from "./studioEnvironment";

registerBuiltInLoaders();

/** A model reachable over HTTP: `rootUrl + fileName` (fileName already URL-encoded). */
export interface UrlModelSource {
    kind: "url";
    rootUrl: string;
    fileName: string;
    displayName: string;
}

/** Files picked or dropped in a plain browser (dev mode). The first one is the main file. */
export interface FilesModelSource {
    kind: "files";
    main: File;
    all: File[];
    displayName: string;
}

export type ModelSource = UrlModelSource | FilesModelSource;

export interface ModelStats {
    meshes: number;
    vertices: number;
    triangles: number;
    splats: number;
    size: Vector3;
    animations: string[];
}

export const SUPPORTED_EXTENSIONS = [
    ".stl", ".obj", ".glb", ".gltf", ".fbx", ".ply", ".splat", ".spz", ".sog",
    ".babylon", ".usdz", ".usd", ".usda", ".usdc", ".bvh",
];

export function extensionOf(name: string): string {
    const clean = name.split(/[?#]/)[0];
    const dot = clean.lastIndexOf(".");
    return dot >= 0 ? clean.substring(dot).toLowerCase() : "";
}

function isGaussianSplat(mesh: AbstractMesh): boolean {
    return mesh.getClassName().startsWith("GaussianSplatting");
}

export class ModelViewer {
    readonly engine: Engine;
    readonly scene: Scene;
    readonly camera: ArcRotateCamera;

    private container: AssetContainer | null = null;
    private readonly defaultMaterial: PBRMaterial;
    private grid: Mesh | null = null;
    private readonly gridMaterial: GridMaterial;
    private readonly keyLight: DirectionalLight;
    private dirtyUntil = 0;
    private loadToken = 0;
    private homeAlpha = -Math.PI / 2 + 0.6;
    private homeBeta = Math.PI / 2 - 0.4;
    private modelMin = Vector3.Zero();
    private modelMax = Vector3.Zero();

    gridVisible = true;

    constructor(canvas: HTMLCanvasElement) {
        this.engine = new Engine(canvas, true, { stencil: true, antialias: true, adaptToDeviceRatio: true }, true);
        const scene = (this.scene = new Scene(this.engine));
        scene.clearColor = new Color4(0.12, 0.12, 0.13, 1);
        scene.imageProcessingConfiguration.toneMappingEnabled = true;
        scene.imageProcessingConfiguration.toneMappingType = ImageProcessingConfiguration.TONEMAPPING_KHR_PBR_NEUTRAL;

        const camera = (this.camera = new ArcRotateCamera("camera", this.homeAlpha, this.homeBeta, 5, Vector3.Zero(), scene));
        camera.attachControl(canvas, true);
        camera.wheelDeltaPercentage = 0.01;
        camera.pinchDeltaPercentage = 0.01;
        camera.useNaturalPinchZoom = true;
        camera.lowerBetaLimit = null;
        camera.upperBetaLimit = null;
        camera.allowUpsideDown = false;
        camera.inertia = 0.85;
        camera.panningInertia = 0.85;
        camera.onViewMatrixChangedObservable.add(() => this.invalidate(600));

        const env = new HDRCubeTexture(createStudioEnvironmentUrl(), scene, 256, false, true, false, true, () => this.invalidate());
        scene.environmentTexture = env;
        scene.environmentIntensity = 0.85;

        // Punctual lights so non-PBR (Standard) materials from OBJ/.babylon files are lit too.
        const hemi = new HemisphericLight("hemi", new Vector3(0, 1, 0), scene);
        hemi.intensity = 0.2;
        hemi.groundColor = new Color3(0.25, 0.25, 0.27);
        this.keyLight = new DirectionalLight("key", new Vector3(-0.5, -0.8, 0.6), scene);
        this.keyLight.intensity = 0.6;

        const mat = (this.defaultMaterial = new PBRMaterial("default", scene));
        mat.albedoColor = new Color3(0.5, 0.51, 0.53);
        mat.metallic = 0.0;
        mat.roughness = 0.45;
        mat.backFaceCulling = false;
        mat.twoSidedLighting = true;

        this.gridMaterial = new GridMaterial("gridMaterial", scene);
        this.gridMaterial.majorUnitFrequency = 10;
        this.gridMaterial.minorUnitVisibility = 0.3;
        this.gridMaterial.opacity = 0.98;
        this.gridMaterial.backFaceCulling = false;

        // Keep panning 1:1 with the cursor regardless of model scale / zoom.
        scene.onBeforeRenderObservable.add(() => {
            const h = this.engine.getRenderHeight(true) || 1;
            camera.panningSensibility = h / (2 * camera.radius * Math.tan(camera.fov / 2));
            // Key light follows the camera a bit, like a photographer's flash.
            const fwd = camera.getForwardRay().direction;
            this.keyLight.direction = new Vector3(fwd.x - 0.3, fwd.y - 0.6, fwd.z).normalize();
        });

        for (const evt of ["pointerdown", "pointermove", "wheel", "keydown"]) {
            canvas.addEventListener(evt, () => this.invalidate(1000), { passive: true });
        }
        window.addEventListener("resize", () => {
            this.engine.resize();
            this.invalidate();
        });

        // Render on demand: only while something is changing, to keep the Explorer preview cheap.
        this.engine.runRenderLoop(() => {
            const animating = !!this.container?.animationGroups.some((g) => g.isPlaying);
            if (animating || performance.now() < this.dirtyUntil || !scene.isReady()) {
                scene.render();
            }
        });
        this.invalidate();
    }

    invalidate(ms = 500): void {
        this.dirtyUntil = Math.max(this.dirtyUntil, performance.now() + ms);
    }

    setBackground(hex: string): void {
        const c = Color3.FromHexString(hex);
        this.scene.clearColor = new Color4(c.r, c.g, c.b, 1);
        this.gridMaterial.mainColor = c;
        const lum = 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
        this.gridMaterial.lineColor = lum > 0.5 ? new Color3(0.55, 0.55, 0.57) : new Color3(0.42, 0.43, 0.46);
        this.invalidate();
    }

    get hasModel(): boolean {
        return this.container !== null;
    }

    clear(): void {
        this.loadToken++;
        if (this.container) {
            this.container.removeAllFromScene();
            this.container.dispose();
            this.container = null;
        }
        this.grid?.setEnabled(false);
        this.invalidate();
    }

    async load(source: ModelSource, onProgress?: (fraction: number | null) => void): Promise<ModelStats> {
        this.clear();
        const token = this.loadToken;
        const name = source.kind === "url" ? decodeURIComponent(source.fileName) : source.main.name;
        const ext = extensionOf(name);

        const progress = (e: { lengthComputable: boolean; loaded: number; total: number }) =>
            onProgress?.(e.lengthComputable && e.total > 0 ? e.loaded / e.total : null);

        let container: AssetContainer;
        if (source.kind === "url") {
            container = await LoadAssetContainerAsync(source.fileName, this.scene, {
                rootUrl: source.rootUrl,
                pluginExtension: ext,
                onProgress: progress,
            });
        } else {
            FilesInputStore.FilesToLoad = {};
            for (const f of source.all) {
                FilesInputStore.FilesToLoad[f.name.toLowerCase()] = f;
            }
            container = await LoadAssetContainerAsync(source.main.name, this.scene, {
                rootUrl: "file:",
                pluginExtension: ext,
                onProgress: progress,
            });
        }

        if (token !== this.loadToken) {
            // A newer load started while this one was in flight.
            container.dispose();
            throw new Error("superseded");
        }

        container.addAllToScene();
        this.container = container;

        for (const mesh of container.meshes) {
            if (!mesh.material && mesh.getTotalVertices() > 0 && !isGaussianSplat(mesh)) {
                mesh.material = this.defaultMaterial;
            }
        }

        if (container.animationGroups.length > 0) {
            container.animationGroups.forEach((g) => g.stop());
            container.animationGroups[0].start(true);
        }

        this.frame(true);
        this.scene.executeWhenReady(() => this.invalidate(1500));
        return this.computeStats();
    }

    playAnimation(index: number): void {
        const groups = this.container?.animationGroups ?? [];
        groups.forEach((g, i) => (i === index ? g.start(true) : g.stop()));
        this.invalidate();
    }

    private contentMeshes(): AbstractMesh[] {
        return (this.container?.meshes ?? []).filter(
            (m) => m.isEnabled() && (m.getTotalVertices() > 0 || isGaussianSplat(m)),
        );
    }

    private computeBounds(): boolean {
        const meshes = this.contentMeshes();
        if (meshes.length === 0) {
            this.modelMin = new Vector3(-1, -1, -1);
            this.modelMax = new Vector3(1, 1, 1);
            return false;
        }
        const min = new Vector3(Infinity, Infinity, Infinity);
        const max = new Vector3(-Infinity, -Infinity, -Infinity);
        for (const m of meshes) {
            m.computeWorldMatrix(true);
            if (m.skeleton || m.morphTargetManager) {
                m.refreshBoundingInfo({ applySkeleton: true, applyMorph: true });
            }
            const bb = m.getBoundingInfo().boundingBox;
            min.minimizeInPlace(bb.minimumWorld);
            max.maximizeInPlace(bb.maximumWorld);
        }
        if (!isFinite(min.x) || !isFinite(max.x)) {
            return false;
        }
        this.modelMin = min;
        this.modelMax = max;
        return true;
    }

    /** Frames the model. With `resetAngles`, also returns to the default 3/4 view. */
    frame(resetAngles = false): void {
        this.computeBounds();
        const cam = this.camera;
        const center = this.modelMin.add(this.modelMax).scale(0.5);
        const radius = Math.max(this.modelMax.subtract(this.modelMin).length() / 2, 1e-6);
        const aspect = this.engine.getAspectRatio(cam) || 1;
        const halfFov = Math.min(cam.fov / 2, Math.atan(Math.tan(cam.fov / 2) * aspect));
        const distance = (radius / Math.sin(halfFov)) * 1.02;

        cam.setTarget(center);
        if (resetAngles) {
            cam.alpha = this.homeAlpha;
            cam.beta = this.homeBeta;
        }
        cam.radius = distance;
        cam.lowerRadiusLimit = radius * 0.02;
        cam.upperRadiusLimit = distance * 20;
        cam.minZ = Math.max(distance * 0.001, radius * 0.001);
        cam.maxZ = distance * 100;
        this.updateGrid();
        this.invalidate(1000);
    }

    private updateGrid(): void {
        const size = this.modelMax.subtract(this.modelMin);
        const extent = Math.max(size.x, size.z, size.y * 0.5, 1e-6);
        const cell = Math.pow(10, Math.floor(Math.log10(extent / 4)));
        const planeSize = Math.ceil((extent * 6) / (cell * 10)) * cell * 10;
        const center = this.modelMin.add(this.modelMax).scale(0.5);

        // GridMaterial draws in the mesh's local space, so size the geometry itself (no scaling).
        this.grid?.dispose();
        const grid = (this.grid = MeshBuilder.CreateGround("__grid__", { width: planeSize, height: planeSize }, this.scene));
        grid.material = this.gridMaterial;
        grid.isPickable = false;
        grid.doNotSerialize = true;
        // Snap the plane origin to the grid so lines stay put relative to world axes.
        const snap = cell * 10;
        grid.position.set(Math.round(center.x / snap) * snap, this.modelMin.y - extent * 1e-4, Math.round(center.z / snap) * snap);
        this.gridMaterial.gridRatio = cell;
        grid.setEnabled(this.gridVisible && this.hasModel);
    }

    get gridCellSize(): number {
        return this.gridMaterial.gridRatio;
    }

    setGridVisible(visible: boolean): void {
        this.gridVisible = visible;
        this.grid?.setEnabled(visible && this.hasModel);
        this.invalidate();
    }

    setWireframe(on: boolean): void {
        this.scene.forceWireframe = on;
        this.invalidate();
    }

    get wireframe(): boolean {
        return this.scene.forceWireframe;
    }

    private computeStats(): ModelStats {
        let meshes = 0;
        let vertices = 0;
        let triangles = 0;
        let splats = 0;
        for (const m of this.contentMeshes()) {
            meshes++;
            if (isGaussianSplat(m)) {
                splats += (m as unknown as { splatCount?: number }).splatCount ?? 0;
                continue;
            }
            const v = m.getTotalVertices();
            const i = m.getTotalIndices();
            vertices += v;
            triangles += Math.floor((i > 0 ? i : v) / 3);
        }
        return {
            meshes,
            vertices,
            triangles,
            splats,
            size: this.modelMax.subtract(this.modelMin),
            animations: (this.container?.animationGroups ?? []).map((g) => g.name),
        };
    }
}
