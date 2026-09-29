import { createHost, type Host } from "./host";
import { setCadQuality } from "./stepLoader";
import { extensionOf, ModelViewer, SUPPORTED_EXTENSIONS, type ModelSource, type ModelStats } from "./viewer";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

const app = $("app");
const canvas = $<HTMLCanvasElement>("canvas");
const empty = $("empty");
const loading = $("loading");
const loadingText = $("loading-text");
const errorBox = $("error");
const errorText = $("error-text");
const fileNameEl = $("file-name");
const infoPanel = $("info-panel");
const infoList = $("info-list");
const badge = $("badge");
const dropOverlay = $("drop-overlay");
const animWrap = $("anim-wrap");
const animSelect = $<HTMLSelectElement>("anim-select");
const btnGrid = $("btn-grid");
const btnWire = $("btn-wire");
const btnInfo = $("btn-info");
const fileInput = $<HTMLInputElement>("file-input");

const APP_NAME = "Babylon Viewer";

/** Status lines for the native host (logged by the Explorer preview handler when enabled). */
function report(message: string): void {
    console.info(message);
    (window as unknown as { chrome?: { webview?: { postMessage(m: string): void } } }).chrome?.webview?.postMessage(message);
}
window.addEventListener("error", (e) => report(`error: ${e.message}`));
window.addEventListener("unhandledrejection", (e) => report(`unhandled rejection: ${String(e.reason)}`));

function formatLength(v: number): string {
    const a = Math.abs(v);
    if (a === 0) return "0";
    if (a >= 1000) return v.toFixed(0);
    if (a >= 100) return v.toFixed(1);
    if (a >= 1) return v.toFixed(2);
    return v.toPrecision(3);
}

function formatCount(n: number): string {
    return n.toLocaleString();
}

function dims(stats: ModelStats): string {
    return `${formatLength(stats.size.x)} × ${formatLength(stats.size.z)} × ${formatLength(stats.size.y)}`;
}

function renderStats(name: string, stats: ModelStats, gridCell: number): void {
    const rows: [string, string][] = [["File", name]];
    rows.push(["Size (W×D×H)", dims(stats)]);
    if (stats.splats > 0) {
        rows.push(["Splats", formatCount(stats.splats)]);
    }
    if (stats.vertices > 0) {
        rows.push(["Vertices", formatCount(stats.vertices)]);
        rows.push(["Triangles", formatCount(stats.triangles)]);
    }
    rows.push(["Meshes", formatCount(stats.meshes)]);
    if (stats.animations.length > 0) {
        rows.push(["Animations", formatCount(stats.animations.length)]);
    }
    rows.push(["Grid cell", formatLength(gridCell)]);

    infoList.replaceChildren(
        ...rows.flatMap(([k, v]) => {
            const dt = document.createElement("dt");
            dt.textContent = k;
            const dd = document.createElement("dd");
            dd.textContent = v;
            return [dt, dd];
        }),
    );

    const parts = [dims(stats)];
    if (stats.splats > 0) parts.push(`${formatCount(stats.splats)} splats`);
    else if (stats.triangles > 0) parts.push(`${formatCount(stats.triangles)} triangles`);
    badge.textContent = parts.join("  ·  ");

    animSelect.replaceChildren(
        ...stats.animations.map((a, i) => {
            const o = document.createElement("option");
            o.value = String(i);
            o.textContent = a || `Animation ${i + 1}`;
            return o;
        }),
    );
    animWrap.hidden = stats.animations.length === 0;
}

function showError(message: string): void {
    errorText.textContent = message;
    errorBox.hidden = false;
}

function toggle(button: HTMLElement, on: boolean): void {
    button.setAttribute("aria-pressed", String(on));
}

async function main(): Promise<void> {
    const host: Host = await createHost();
    app.dataset.mode = host.mode;
    // Coarser CAD tessellation in the preview pane: it converts faster.
    setCadQuality(host.mode === "preview" ? "fast" : "fine");

    const viewer = new ModelViewer(canvas);
    const applyBackground = () => {
        const bg = host.background ?? getComputedStyle(document.documentElement).getPropertyValue("--bg").trim();
        viewer.setBackground(bg);
    };
    if (host.background) {
        // Match Explorer's preview pane color and pick the UI theme from its luminance.
        const n = parseInt(host.background.slice(1), 16);
        const lum = (0.2126 * (n >> 16) + 0.7152 * ((n >> 8) & 255) + 0.0722 * (n & 255)) / 255;
        document.documentElement.dataset.theme = lum > 0.5 ? "light" : "dark";
        document.body.style.background = host.background;
    }
    applyBackground();
    matchMedia("(prefers-color-scheme: dark)").addEventListener("change", applyBackground);

    $("formats").textContent = "Supported: " + SUPPORTED_EXTENSIONS.map((e) => e.slice(1).toUpperCase()).join(", ");
    fileInput.accept = SUPPORTED_EXTENSIONS.join(",") + ",.bin,.png,.jpg,.jpeg,.webp,.ktx2,.mtl";

    let loadSeq = 0;
    async function open(source: ModelSource | null): Promise<void> {
        if (!source) return;
        const seq = ++loadSeq;
        errorBox.hidden = true;
        empty.hidden = true;
        badge.hidden = true;
        loading.hidden = false;
        loadingText.textContent = "Loading…";
        fileNameEl.textContent = source.displayName;
        host.setTitle(`${source.displayName} — ${APP_NAME}`);
        try {
            const stats = await viewer.load(source, (f) => {
                if (seq === loadSeq) {
                    loadingText.textContent = f === null ? "Loading…" : `Loading… ${Math.round(f * 100)}%`;
                }
            });
            if (seq !== loadSeq) return;
            report(`loaded ${source.displayName} in ${Math.round(performance.now())} ms since page start`);
            renderStats(source.displayName, stats, viewer.gridCellSize);
            badge.hidden = host.mode !== "preview";
        } catch (e) {
            if (seq !== loadSeq || (e instanceof Error && e.message === "superseded")) return;
            report(`load failed: ${e instanceof Error ? e.stack ?? e.message : String(e)}`);
            viewer.clear();
            const msg = e instanceof Error ? e.message : String(e);
            showError(msg || "The file could not be parsed.");
            if (host.mode !== "preview") empty.hidden = true;
        } finally {
            if (seq === loadSeq) loading.hidden = true;
        }
    }

    async function pick(): Promise<void> {
        if (host.pickModel) {
            await open(await host.pickModel());
        } else {
            fileInput.click();
        }
    }

    fileInput.addEventListener("change", () => {
        const files = Array.from(fileInput.files ?? []);
        fileInput.value = "";
        const main = files.find((f) => SUPPORTED_EXTENSIONS.includes(extensionOf(f.name)));
        if (main) {
            void open({ kind: "files", main, all: files, displayName: main.name });
        } else if (files.length) {
            showError("None of the selected files is a supported 3D format.");
        }
    });

    // Drag & drop: native file paths in the app, File objects in a plain browser.
    if (host.onDropPaths && host.openPath) {
        const openPath = host.openPath;
        host.onDropPaths(
            (paths) => {
                const path = paths.find((p) => SUPPORTED_EXTENSIONS.includes(extensionOf(p)));
                if (path) void openPath(path).then(open, (e) => showError(String(e)));
                else if (paths.length) showError("That file type isn't supported.");
            },
            (hovering) => (dropOverlay.hidden = !hovering),
        );
    } else if (host.mode === "browser") {
        document.addEventListener("dragover", (e) => {
            e.preventDefault();
            dropOverlay.hidden = false;
        });
        document.addEventListener("dragleave", (e) => {
            if (!e.relatedTarget) dropOverlay.hidden = true;
        });
        document.addEventListener("drop", (e) => {
            e.preventDefault();
            dropOverlay.hidden = true;
            const files = Array.from(e.dataTransfer?.files ?? []);
            const main = files.find((f) => SUPPORTED_EXTENSIONS.includes(extensionOf(f.name)));
            if (main) void open({ kind: "files", main, all: files, displayName: main.name });
        });
    }

    const setGrid = (on: boolean) => {
        viewer.setGridVisible(on);
        toggle(btnGrid, on);
    };
    const setWire = (on: boolean) => {
        viewer.setWireframe(on);
        toggle(btnWire, on);
    };
    const setInfo = (on: boolean) => {
        infoPanel.hidden = !on;
        toggle(btnInfo, on);
    };

    $("btn-open").addEventListener("click", () => void pick());
    $("btn-open-empty").addEventListener("click", () => void pick());
    $("btn-frame").addEventListener("click", () => viewer.frame(true));
    btnGrid.addEventListener("click", () => setGrid(!viewer.gridVisible));
    btnWire.addEventListener("click", () => setWire(!viewer.wireframe));
    btnInfo.addEventListener("click", () => setInfo(infoPanel.hidden !== false));
    animSelect.addEventListener("change", () => viewer.playAnimation(Number(animSelect.value)));

    window.addEventListener("keydown", (e) => {
        if (e.target instanceof HTMLSelectElement) return;
        const key = e.key.toLowerCase();
        if ((e.ctrlKey || e.metaKey) && key === "o" && host.mode !== "preview") {
            e.preventDefault();
            void pick();
            return;
        }
        if (e.ctrlKey || e.metaKey || e.altKey) return;
        if (key === "f") viewer.frame(true);
        else if (key === "g") setGrid(!viewer.gridVisible);
        else if (key === "w") setWire(!viewer.wireframe);
        else if (key === "i" && host.mode !== "preview") setInfo(infoPanel.hidden !== false);
    });

    const initial = await host.initialModel().catch((e) => {
        showError(String(e));
        return null;
    });
    if (initial) {
        await open(initial);
    } else if (host.mode !== "preview") {
        empty.hidden = false;
    }
}

void main();
