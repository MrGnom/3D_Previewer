import { extensionOf, SUPPORTED_EXTENSIONS, type ModelSource, type UrlModelSource } from "./viewer";

/** A model queued for preview image generation. */
export interface BatchModel {
    source: ModelSource;
    /** File name, e.g. "part.step". */
    name: string;
    /** Absolute path on disk (app only); used for the "next to each model" output option. */
    path?: string;
}

export type WriteResult = "written" | "exists";

/**
 * The viewer bundle runs in three hosts:
 *  - "app":     the Tauri desktop app (native open dialog, drag & drop of file paths, window title)
 *  - "preview": the Explorer preview pane (file given in the query string, no chrome)
 *  - "browser": plain browser during development (HTML file input / drag & drop)
 */
export type HostMode = "app" | "preview" | "browser";

export interface Host {
    mode: HostMode;
    initialModel(): Promise<ModelSource | null>;
    pickModel?(): Promise<ModelSource | null>;
    onDropPaths?(handler: (paths: string[]) => void, onHover: (hovering: boolean) => void): void;
    openPath?(path: string): Promise<ModelSource | null>;
    setTitle(title: string): void;
    /** Background color requested by the host (e.g. Explorer theme), as #rrggbb. */
    background?: string;
    /** Calls `apply` with the system theme now and again whenever it changes. */
    watchTheme(apply: (dark: boolean) => void): void;

    /** Lets the user pick several models for preview image generation. */
    pickModels?(): Promise<BatchModel[]>;
    /** Lets the user pick an output folder (app only). Returns its absolute path. */
    pickOutputFolder?(): Promise<string | null>;
    /**
     * Saves a generated image. In the app `target` is an absolute path; in a browser it is
     * downloaded under the file name part of `target`.
     */
    saveImage?(target: string, image: Blob, overwrite: boolean): Promise<WriteResult>;
    /** True when images are written to disk (and output folders are available). */
    writesToDisk?: boolean;
}

/** Follows `prefers-color-scheme`, which WebView2 derives from the Windows app theme. */
function watchMediaTheme(apply: (dark: boolean) => void, initial?: boolean): void {
    const query = matchMedia("(prefers-color-scheme: dark)");
    apply(initial ?? query.matches);
    query.addEventListener("change", (e) => apply(e.matches));
}

interface NativeModelRef {
    rootUrl: string;
    fileName: string;
    displayName: string;
    path: string;
}

function fromNative(ref: NativeModelRef | null): UrlModelSource | null {
    return ref ? { kind: "url", rootUrl: ref.rootUrl, fileName: ref.fileName, displayName: ref.displayName } : null;
}

/** Opens the browser's file picker for several files. Resolves with [] when cancelled. */
function pickBrowserFiles(accept: string): Promise<File[]> {
    return new Promise((resolve) => {
        const input = document.createElement("input");
        input.type = "file";
        input.multiple = true;
        input.accept = accept;
        input.addEventListener("change", () => resolve(Array.from(input.files ?? [])));
        input.addEventListener("cancel", () => resolve([]));
        input.click();
    });
}

/** Each supported file becomes a model; the other picked files (textures, .bin, .mtl) are shared side files. */
async function pickBrowserModels(): Promise<BatchModel[]> {
    const files = await pickBrowserFiles(SUPPORTED_EXTENSIONS.join(",") + ",.bin,.png,.jpg,.jpeg,.webp,.ktx2,.mtl");
    return files
        .filter((f) => SUPPORTED_EXTENSIONS.includes(extensionOf(f.name)))
        .map((main) => ({ name: main.name, source: { kind: "files", main, all: files, displayName: main.name } }));
}

async function downloadImage(target: string, image: Blob): Promise<WriteResult> {
    const url = URL.createObjectURL(image);
    const a = document.createElement("a");
    a.href = url;
    a.download = target.split(/[\\/]/).pop() || "preview.png";
    a.click();
    setTimeout(() => URL.revokeObjectURL(url), 10_000);
    return "written";
}

async function createAppHost(): Promise<Host> {
    const { invoke } = await import("@tauri-apps/api/core");
    const { getCurrentWebview } = await import("@tauri-apps/api/webview");
    const { getCurrentWindow } = await import("@tauri-apps/api/window");

    return {
        mode: "app",
        initialModel: async () => fromNative(await invoke<NativeModelRef | null>("initial_model")),
        pickModel: async () => fromNative(await invoke<NativeModelRef | null>("pick_model")),
        openPath: async (path) => fromNative(await invoke<NativeModelRef | null>("open_path", { path })),
        writesToDisk: true,
        pickModels: async () =>
            (await invoke<NativeModelRef[]>("pick_models")).map((ref) => ({
                source: fromNative(ref)!,
                name: ref.displayName,
                path: ref.path,
            })),
        pickOutputFolder: () => invoke<string | null>("pick_output_folder"),
        saveImage: async (target, image, overwrite) =>
            invoke<WriteResult>("write_image", new Uint8Array(await image.arrayBuffer()), {
                headers: { "x-path": encodeURIComponent(target), "x-overwrite": overwrite ? "1" : "0" },
            }),
        onDropPaths(handler, onHover) {
            void getCurrentWebview().onDragDropEvent((event) => {
                const p = event.payload;
                if (p.type === "enter" || p.type === "over") {
                    onHover(true);
                } else if (p.type === "leave") {
                    onHover(false);
                } else if (p.type === "drop") {
                    onHover(false);
                    handler(p.paths);
                }
            });
        },
        setTitle(title) {
            void getCurrentWindow().setTitle(title);
        },
        watchTheme(apply) {
            // The window's theme follows Windows' app theme, including later changes.
            const win = getCurrentWindow();
            win.theme().then(
                (theme) => {
                    if (theme === null) {
                        watchMediaTheme(apply);
                        return;
                    }
                    apply(theme === "dark");
                    void win.onThemeChanged(({ payload }) => apply(payload === "dark"));
                },
                () => watchMediaTheme(apply),
            );
        },
    };
}

function createPreviewHost(params: URLSearchParams): Host {
    const file = params.get("file");
    const bg = params.get("bg");
    // Windows' app theme as read by the preview handler; later changes come through the media query.
    const theme = params.get("theme");
    return {
        mode: "preview",
        background: bg && /^[0-9a-f]{6}$/i.test(bg) ? `#${bg}` : undefined,
        initialModel: async () =>
            file
                ? {
                      kind: "url",
                      rootUrl: params.get("root") ?? "../model/",
                      fileName: encodeURIComponent(file),
                      displayName: file,
                  }
                : null,
        setTitle() {},
        watchTheme(apply) {
            watchMediaTheme(apply, theme === "dark" ? true : theme === "light" ? false : undefined);
        },
    };
}

function createBrowserHost(params: URLSearchParams): Host {
    const url = params.get("url");
    return {
        mode: "browser",
        initialModel: async () => {
            if (!url) {
                return null;
            }
            const slash = url.lastIndexOf("/");
            return {
                kind: "url",
                rootUrl: url.substring(0, slash + 1),
                fileName: url.substring(slash + 1),
                displayName: decodeURIComponent(url.substring(slash + 1)),
            };
        },
        setTitle(title) {
            document.title = title;
        },
        watchTheme(apply) {
            watchMediaTheme(apply);
        },
        pickModels: pickBrowserModels,
        saveImage: downloadImage,
    };
}

export async function createHost(): Promise<Host> {
    const params = new URLSearchParams(location.search);
    if (params.get("mode") === "preview") {
        return createPreviewHost(params);
    }
    if ("__TAURI_INTERNALS__" in window) {
        return createAppHost();
    }
    return createBrowserHost(params);
}
