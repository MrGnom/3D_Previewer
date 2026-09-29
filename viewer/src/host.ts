import type { ModelSource, UrlModelSource } from "./viewer";

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
}

interface NativeModelRef {
    rootUrl: string;
    fileName: string;
    displayName: string;
}

function fromNative(ref: NativeModelRef | null): UrlModelSource | null {
    return ref ? { kind: "url", ...ref } : null;
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
    };
}

function createPreviewHost(params: URLSearchParams): Host {
    const file = params.get("file");
    const bg = params.get("bg");
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
