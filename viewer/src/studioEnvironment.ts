/**
 * Builds a small procedural "photo studio" environment as an in-memory Radiance HDR file,
 * so image-based lighting works fully offline (no .env download from a CDN).
 */

interface SoftBox {
    lon: number; // degrees
    lat: number; // degrees
    radius: number; // degrees
    color: [number, number, number];
}

const SOFT_BOXES: SoftBox[] = [
    { lon: 40, lat: 38, radius: 20, color: [5.0, 4.8, 4.5] }, // key, slightly warm
    { lon: -125, lat: 22, radius: 28, color: [1.2, 1.3, 1.5] }, // fill, slightly cool
    { lon: 160, lat: 55, radius: 16, color: [2.5, 2.5, 2.5] }, // rim
    { lon: 0, lat: 88, radius: 22, color: [1.5, 1.5, 1.5] }, // top
];

function lerp(a: number, b: number, t: number): number {
    return a + (b - a) * t;
}

function smoothstep(e0: number, e1: number, x: number): number {
    const t = Math.min(1, Math.max(0, (x - e0) / (e1 - e0)));
    return t * t * (3 - 2 * t);
}

function writeRgbe(out: Uint8Array, o: number, r: number, g: number, b: number): void {
    const v = Math.max(r, g, b);
    if (v < 1e-32) {
        out[o] = out[o + 1] = out[o + 2] = out[o + 3] = 0;
        return;
    }
    const exp = Math.floor(Math.log2(v)) + 1;
    const scale = 256 / Math.pow(2, exp);
    out[o] = Math.min(255, Math.floor(r * scale));
    out[o + 1] = Math.min(255, Math.floor(g * scale));
    out[o + 2] = Math.min(255, Math.floor(b * scale));
    out[o + 3] = exp + 128;
}

export function createStudioEnvironmentUrl(width = 512, height = 256): string {
    const header = `#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y ${height} +X ${width}\n`;
    const bytes = new Uint8Array(header.length + width * height * 4);
    for (let i = 0; i < header.length; i++) {
        bytes[i] = header.charCodeAt(i);
    }

    const toRad = Math.PI / 180;
    const boxes = SOFT_BOXES.map((b) => {
        const lat = b.lat * toRad;
        const lon = b.lon * toRad;
        return {
            dir: [Math.cos(lat) * Math.sin(lon), Math.sin(lat), Math.cos(lat) * Math.cos(lon)],
            cosInner: Math.cos(b.radius * 0.7 * toRad),
            cosOuter: Math.cos(b.radius * toRad),
            color: b.color,
        };
    });

    let o = header.length;
    for (let y = 0; y < height; y++) {
        const lat = (0.5 - (y + 0.5) / height) * Math.PI;
        const sinLat = Math.sin(lat);
        const cosLat = Math.cos(lat);
        for (let x = 0; x < width; x++) {
            const lon = ((x + 0.5) / width) * 2 * Math.PI - Math.PI;
            const d0 = cosLat * Math.sin(lon);
            const d1 = sinLat;
            const d2 = cosLat * Math.cos(lon);

            // Neutral studio gradient: bright ceiling, mid horizon, dark floor.
            let r: number, g: number, b: number;
            if (sinLat >= 0) {
                const t = Math.pow(sinLat, 0.6);
                r = lerp(0.3, 0.5, t);
                g = lerp(0.31, 0.51, t);
                b = lerp(0.32, 0.53, t);
            } else {
                const t = Math.pow(-sinLat, 0.5);
                r = lerp(0.24, 0.05, t);
                g = lerp(0.24, 0.05, t);
                b = lerp(0.25, 0.055, t);
            }

            for (const box of boxes) {
                const c = d0 * box.dir[0] + d1 * box.dir[1] + d2 * box.dir[2];
                const w = smoothstep(box.cosOuter, box.cosInner, c);
                if (w > 0) {
                    r += box.color[0] * w;
                    g += box.color[1] * w;
                    b += box.color[2] * w;
                }
            }

            writeRgbe(bytes, o, r, g, b);
            o += 4;
        }
    }

    return URL.createObjectURL(new Blob([bytes], { type: "application/octet-stream" }));
}
