// Minimal typings for occt-import-js (OpenCascade compiled to WebAssembly).
declare module "occt-import-js" {
    export type OcctFormat = "step" | "iges" | "brep";
    export type OcctColor = [number, number, number];

    export interface OcctParams {
        linearUnit?: "millimeter" | "centimeter" | "meter" | "inch" | "foot";
        linearDeflectionType?: "bounding_box_ratio" | "absolute_value";
        linearDeflection?: number;
        angularDeflection?: number;
    }

    export interface OcctNode {
        name: string;
        meshes: number[];
        children: OcctNode[];
    }

    export interface OcctMesh {
        name: string;
        color?: OcctColor;
        /** Triangle ranges (inclusive) of each B-rep face. */
        brep_faces: { first: number; last: number; color: OcctColor | null }[];
        attributes: {
            position: { array: number[] };
            normal?: { array: number[] };
        };
        index: { array: number[] };
    }

    export interface OcctResult {
        success: boolean;
        root: OcctNode;
        meshes: OcctMesh[];
    }

    export interface Occt {
        ReadFile(format: OcctFormat, content: Uint8Array, params: OcctParams | null): OcctResult;
    }

    export interface OcctModuleOverrides {
        wasmBinary?: ArrayBuffer;
        locateFile?(path: string, prefix: string): string;
    }

    export default function occtimportjs(overrides?: OcctModuleOverrides): Promise<Occt>;
}
