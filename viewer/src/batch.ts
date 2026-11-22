import type { BatchModel, Host } from "./host";
import { extensionOf, ModelViewer, SUPPORTED_EXTENSIONS, type RenderImageOptions, type ViewPreset } from "./viewer";

/** Output settings for preview image generation, remembered between sessions. */
interface BatchSettings {
    width: number;
    height: number;
    format: "png" | "jpeg";
    /** 50..100 */
    quality: number;
    background: "theme" | "transparent" | "custom";
    color: string;
    view: ViewPreset;
    grid: boolean;
    name: string;
    dest: "beside" | "folder";
    folder: string;
    overwrite: boolean;
}

const DEFAULTS: BatchSettings = {
    width: 1024,
    height: 1024,
    format: "png",
    quality: 90,
    background: "theme",
    color: "#ffffff",
    view: "iso",
    grid: false,
    name: "{name}",
    dest: "beside",
    folder: "",
    overwrite: false,
};

const STORAGE_KEY = "batchPreviewSettings";

function loadSettings(): BatchSettings {
    try {
        const saved = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "{}") as Partial<BatchSettings>;
        return { ...DEFAULTS, ...saved };
    } catch {
        return { ...DEFAULTS };
    }
}

function saveSettings(settings: BatchSettings): void {
    try {
        localStorage.setItem(STORAGE_KEY, JSON.stringify(settings));
    } catch {
        // Storage unavailable: settings just aren't remembered.
    }
}

type ItemState = "pending" | "working" | "done" | "skipped" | "error";

interface Item {
    model: BatchModel;
    state: ItemState;
    message?: string;
}

const STATE_LABEL: Record<ItemState, string> = {
    pending: "",
    working: "Rendering…",
    done: "Saved",
    skipped: "Exists — skipped",
    error: "Failed",
};

/** Characters Windows does not allow in file names. */
const INVALID_NAME_CHARS = /[<>:"/\\|?*\u0000-\u001f]/g;

function baseName(name: string): string {
    const dot = name.lastIndexOf(".");
    return dot > 0 ? name.substring(0, dot) : name;
}

function dirName(path: string): string {
    const i = Math.max(path.lastIndexOf("\\"), path.lastIndexOf("/"));
    return i >= 0 ? path.substring(0, i) : "";
}

function joinPath(dir: string, file: string): string {
    return dir.endsWith("\\") || dir.endsWith("/") ? dir + file : `${dir}\\${file}`;
}

export interface BatchDialog {
    open(): void;
    readonly isOpen: boolean;
    /** Adds models by path (drag & drop onto the app while the dialog is open). */
    addPaths(paths: string[]): Promise<void>;
}

/**
 * The "Generate preview images" dialog. Models are rendered one at a time by a hidden,
 * non-interactive viewer, so the model open in the main view is left alone.
 */
export function createBatchDialog(host: Host, themeBackground: () => string): BatchDialog {
    const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
    const dialog = $<HTMLDialogElement>("batch-dialog");
    const list = $<HTMLUListElement>("batch-list");
    const emptyState = $("batch-empty");
    const count = $("batch-count");
    const addBtn = $<HTMLButtonElement>("batch-add");
    const clearBtn = $<HTMLButtonElement>("batch-clear");
    const runBtn = $<HTMLButtonElement>("batch-run");
    const closeBtn = $<HTMLButtonElement>("batch-close");
    const progress = $<HTMLProgressElement>("batch-progress");
    const status = $("batch-status");

    const sizeSel = $<HTMLSelectElement>("opt-size");
    const widthIn = $<HTMLInputElement>("opt-width");
    const heightIn = $<HTMLInputElement>("opt-height");
    const qualityField = $("quality-field");
    const qualityIn = $<HTMLInputElement>("opt-quality");
    const qualityOut = $<HTMLOutputElement>("opt-quality-value");
    const bgSel = $<HTMLSelectElement>("opt-bg");
    const bgColor = $<HTMLInputElement>("opt-bg-color");
    const bgNote = $("bg-note");
    const viewSel = $<HTMLSelectElement>("opt-view");
    const gridChk = $<HTMLInputElement>("opt-grid");
    const nameIn = $<HTMLInputElement>("opt-name");
    const extOut = $("opt-ext");
    const destFields = $("dest-fields");
    const folderIn = $<HTMLInputElement>("opt-folder");
    const folderBtn = $<HTMLButtonElement>("opt-folder-browse");
    const downloadNote = $("download-note");
    const overwriteChk = $<HTMLInputElement>("opt-overwrite");
    const formatRadios = Array.from(document.querySelectorAll<HTMLInputElement>('input[name="opt-format"]'));
    const destRadios = Array.from(document.querySelectorAll<HTMLInputElement>('input[name="opt-dest"]'));

    const settings = loadSettings();
    // A folder picked in an earlier session is no longer authorized for writing; pick it again.
    settings.folder = "";
    if (settings.dest === "folder") settings.dest = "beside";

    let items: Item[] = [];
    let running = false;
    let cancelRequested = false;
    let renderer: ModelViewer | null = null;

    // ---- settings <-> form ----

    function writeForm(): void {
        const sizeKey = `${settings.width}x${settings.height}`;
        sizeSel.value = Array.from(sizeSel.options).some((o) => o.value === sizeKey) ? sizeKey : "custom";
        widthIn.value = String(settings.width);
        heightIn.value = String(settings.height);
        formatRadios.forEach((r) => (r.checked = r.value === settings.format));
        qualityIn.value = String(settings.quality);
        bgSel.value = settings.background;
        bgColor.value = settings.color;
        viewSel.value = settings.view;
        gridChk.checked = settings.grid;
        nameIn.value = settings.name;
        destRadios.forEach((r) => (r.checked = r.value === settings.dest));
        folderIn.value = settings.folder;
        overwriteChk.checked = settings.overwrite;
        syncDependents();
    }

    function readForm(): void {
        settings.width = Math.round(Number(widthIn.value)) || DEFAULTS.width;
        settings.height = Math.round(Number(heightIn.value)) || DEFAULTS.height;
        settings.format = formatRadios.find((r) => r.checked)?.value === "jpeg" ? "jpeg" : "png";
        settings.quality = Number(qualityIn.value);
        settings.background = bgSel.value as BatchSettings["background"];
        settings.color = bgColor.value;
        settings.view = viewSel.value as ViewPreset;
        settings.grid = gridChk.checked;
        settings.name = nameIn.value;
        settings.dest = destRadios.find((r) => r.checked)?.value === "folder" ? "folder" : "beside";
        settings.overwrite = overwriteChk.checked;
        saveSettings(settings);
        syncDependents();
    }

    /** Shows/hides the controls that depend on other settings. */
    function syncDependents(): void {
        const jpeg = settings.format === "jpeg";
        qualityField.hidden = !jpeg;
        qualityOut.textContent = String(settings.quality);
        extOut.textContent = jpeg ? ".jpg" : ".png";
        bgColor.hidden = settings.background !== "custom";
        bgNote.hidden = !(jpeg && settings.background === "transparent");
        const toDisk = !!host.writesToDisk;
        destFields.hidden = !toDisk;
        overwriteChk.parentElement!.hidden = !toDisk;
        downloadNote.hidden = toDisk;
        const folderMode = settings.dest === "folder";
        folderIn.disabled = !folderMode;
        folderBtn.disabled = !folderMode;
        updateRunState();
    }

    sizeSel.addEventListener("change", () => {
        if (sizeSel.value !== "custom") {
            const [w, h] = sizeSel.value.split("x");
            widthIn.value = w;
            heightIn.value = h;
        } else {
            widthIn.focus();
        }
        readForm();
    });
    for (const input of [widthIn, heightIn]) {
        input.addEventListener("input", () => {
            sizeSel.value = "custom";
            readForm();
        });
    }
    for (const el of [qualityIn, bgSel, bgColor, viewSel, gridChk, nameIn, overwriteChk, ...formatRadios, ...destRadios]) {
        el.addEventListener("input", readForm);
        el.addEventListener("change", readForm);
    }

    folderBtn.addEventListener("click", async () => {
        const folder = await host.pickOutputFolder?.().catch((e) => {
            setStatus(String(e), true);
            return null;
        });
        if (folder) {
            settings.folder = folder;
            folderIn.value = folder;
            saveSettings(settings);
            updateRunState();
        }
    });

    // ---- model list ----

    function addModels(models: BatchModel[]): void {
        const known = new Set(items.map((i) => i.model.path ?? i.model.name));
        for (const model of models) {
            const key = model.path ?? model.name;
            if (!known.has(key)) {
                known.add(key);
                items.push({ model, state: "pending" });
            }
        }
        renderList();
    }

    function renderList(): void {
        list.replaceChildren(
            ...items.map((item, index) => {
                const li = document.createElement("li");
                li.className = `batch-item state-${item.state}`;

                const text = document.createElement("div");
                text.className = "item-text";
                const name = document.createElement("span");
                name.className = "item-name";
                name.textContent = item.model.name;
                text.append(name);
                if (item.model.path) {
                    const path = document.createElement("span");
                    path.className = "item-path";
                    path.textContent = dirName(item.model.path);
                    path.title = item.model.path;
                    text.append(path);
                }

                const state = document.createElement("span");
                state.className = "item-state";
                state.textContent = STATE_LABEL[item.state];
                if (item.message) state.title = item.message;

                const remove = document.createElement("button");
                remove.type = "button";
                remove.className = "btn btn-icon btn-small";
                remove.title = "Remove";
                remove.setAttribute("aria-label", `Remove ${item.model.name}`);
                remove.disabled = running;
                remove.innerHTML =
                    '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="m6 6 8 8M14 6l-8 8" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></svg>';
                remove.addEventListener("click", () => {
                    items.splice(index, 1);
                    renderList();
                });

                li.append(text, state, remove);
                return li;
            }),
        );
        emptyState.hidden = items.length > 0;
        count.textContent = items.length ? `(${items.length})` : "";
        updateRunState();
    }

    function updateRunState(): void {
        const needsFolder = !!host.writesToDisk && settings.dest === "folder" && !settings.folder;
        runBtn.disabled = !running && (items.length === 0 || needsFolder);
        runBtn.textContent = running ? "Cancel" : "Generate";
        addBtn.disabled = running;
        clearBtn.disabled = running || items.length === 0;
        if (!running && needsFolder && items.length > 0) {
            setStatus("Choose an output folder.");
        }
    }

    function setStatus(text: string, isError = false): void {
        status.textContent = text;
        status.classList.toggle("is-error", isError);
    }

    addBtn.addEventListener("click", async () => {
        if (!host.pickModels) return;
        try {
            addModels(await host.pickModels());
        } catch (e) {
            setStatus(String(e), true);
        }
    });
    clearBtn.addEventListener("click", () => {
        items = [];
        renderList();
        setStatus("");
    });

    // Plain browser: dropping files onto the dialog adds them.
    if (host.mode === "browser") {
        dialog.addEventListener("dragover", (e) => {
            e.preventDefault();
            e.stopPropagation();
        });
        dialog.addEventListener("drop", (e) => {
            e.preventDefault();
            e.stopPropagation();
            if (running) return;
            const files = Array.from(e.dataTransfer?.files ?? []);
            addModels(
                files
                    .filter((f) => SUPPORTED_EXTENSIONS.includes(extensionOf(f.name)))
                    .map((main) => ({ name: main.name, source: { kind: "files", main, all: files, displayName: main.name } })),
            );
        });
    }

    // ---- generation ----

    function outputName(model: BatchModel, used: Set<string>, dir: string): string {
        const ext = settings.format === "jpeg" ? ".jpg" : ".png";
        const pattern = settings.name.trim() || DEFAULTS.name;
        const stem =
            pattern
                .replaceAll("{name}", baseName(model.name))
                .replaceAll("{ext}", extensionOf(model.name).slice(1))
                .replaceAll("{w}", String(settings.width))
                .replaceAll("{h}", String(settings.height))
                .replace(INVALID_NAME_CHARS, "_")
                .replace(/[. ]+$/, "") || "preview";
        // Two models with the same base name (part.stl, part.step) must not overwrite each other.
        let file = stem + ext;
        for (let n = 2; used.has(`${dir}|${file}`.toLowerCase()); n++) {
            file = `${stem} (${n})${ext}`;
        }
        used.add(`${dir}|${file}`.toLowerCase());
        return file;
    }

    function renderOptions(): RenderImageOptions {
        const background =
            settings.background === "transparent" ? null : settings.background === "custom" ? settings.color : themeBackground();
        return {
            width: settings.width,
            height: settings.height,
            mimeType: settings.format === "jpeg" ? "image/jpeg" : "image/png",
            quality: settings.quality / 100,
            background,
            view: settings.view,
            grid: settings.grid,
        };
    }

    async function run(): Promise<void> {
        readForm();
        if (!renderer) {
            const canvas = document.createElement("canvas");
            canvas.width = 16;
            canvas.height = 16;
            renderer = new ModelViewer(canvas, { interactive: false });
        }
        const max = renderer.maxImageSize;
        if (settings.width < 16 || settings.height < 16 || settings.width > max || settings.height > max) {
            setStatus(`Resolution must be between 16 and ${max} pixels per side.`, true);
            return;
        }

        running = true;
        cancelRequested = false;
        items.forEach((i) => ((i.state = "pending"), (i.message = undefined)));
        renderList();
        progress.hidden = false;
        progress.max = items.length;
        progress.value = 0;

        const options = renderOptions();
        const used = new Set<string>();
        let done = 0;
        let skipped = 0;
        let failed = 0;

        for (const [index, item] of items.entries()) {
            if (cancelRequested) break;
            item.state = "working";
            renderList();
            setStatus(`Rendering ${index + 1} of ${items.length}: ${item.model.name}`);
            try {
                const dir =
                    !host.writesToDisk ? "" : settings.dest === "folder" ? settings.folder : dirName(item.model.path ?? "");
                if (host.writesToDisk && !dir) {
                    throw new Error("No output folder for this model.");
                }
                const file = outputName(item.model, used, dir);
                const target = dir ? joinPath(dir, file) : file;
                await renderer.load(item.model.source);
                const image = await renderer.renderImage(options);
                const result = await host.saveImage!(target, image, settings.overwrite);
                item.state = result === "exists" ? "skipped" : "done";
                item.message = target;
                if (result === "exists") skipped++;
                else done++;
            } catch (e) {
                item.state = "error";
                item.message = e instanceof Error ? e.message : String(e);
                failed++;
            }
            progress.value = index + 1;
            renderList();
        }

        renderer.clear();
        running = false;
        renderList();
        const parts = [`${done} saved`];
        if (skipped) parts.push(`${skipped} skipped (already exist)`);
        if (failed) parts.push(`${failed} failed`);
        setStatus((cancelRequested ? "Cancelled. " : "Done. ") + parts.join(", ") + ".", failed > 0);
    }

    runBtn.addEventListener("click", () => {
        if (running) {
            cancelRequested = true;
            setStatus("Cancelling after the current model…");
        } else {
            void run();
        }
    });

    // ---- open / close ----

    function close(): void {
        if (running) {
            cancelRequested = true;
            setStatus("Cancelling after the current model…");
            return;
        }
        dialog.close();
    }

    closeBtn.addEventListener("click", close);
    $("batch-x").addEventListener("click", close);
    dialog.addEventListener("cancel", (e) => {
        // Esc: never close mid-run (the run would keep going invisibly).
        e.preventDefault();
        close();
    });
    dialog.addEventListener("close", () => {
        renderer?.dispose();
        renderer = null;
    });

    writeForm();
    renderList();

    return {
        get isOpen() {
            return dialog.open;
        },
        open() {
            if (!dialog.open) {
                progress.hidden = true;
                setStatus("");
                writeForm();
                dialog.showModal();
            }
        },
        async addPaths(paths) {
            if (running || !host.openPath) return;
            const models: BatchModel[] = [];
            for (const path of paths.filter((p) => SUPPORTED_EXTENSIONS.includes(extensionOf(p)))) {
                try {
                    const source = await host.openPath(path);
                    if (source) models.push({ source, name: source.displayName, path });
                } catch (e) {
                    setStatus(String(e), true);
                }
            }
            addModels(models);
        },
    };
}
