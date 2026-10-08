# rust-wasm-zipper

Streams a ZIP archive in the browser, built in Rust and compiled to WebAssembly.

`zip_files` takes a list of `{ name, source }` entries and returns a `ReadableStream<Uint8Array>` of the archive. Each source is read chunk by chunk and written straight to the output, so the files are never loaded into memory all at once. The stream applies backpressure: no more input is read until the consumer pulls more output.

- **Sources:** `ReadableStream<Uint8Array>`, `Blob` / `File`, `Response`, `FileSystemFileHandle`, `ArrayBuffer`, or any typed array / `DataView`.
- **Format:** entries are stored without compression (STORED). File names are UTF-8.
- **Limits:** no ZIP64, so the archive must stay under 4 GiB and 65,535 entries.

## Installation

The package isn't on npm. Each release on GitHub has an npm tarball attached, built by [the release workflow](.github/workflows/release.yml). Install it straight from the release URL:

```sh
npm install https://github.com/kjmagic13/rust-wasm-zipper/releases/download/vX.Y.Z/rust-wasm-zipper-vX.Y.Z.tgz
```

Change `vX.Y.Z` (it appears twice) to the release you want. The [releases page](https://github.com/kjmagic13/rust-wasm-zipper/releases) lists them all.

This adds the following to `package.json`:

```json
{
  "dependencies": {
    "rust-wasm-zipper": "https://github.com/kjmagic13/rust-wasm-zipper/releases/download/vX.Y.Z/rust-wasm-zipper-vX.Y.Z.tgz"
  }
}
```

pnpm and yarn accept the same URL (`pnpm add <url>`, `yarn add <url>`).

TypeScript types (`rust_wasm_zipper.d.ts`) are included in the package.

## Usage

The package is built with `wasm-pack --target web`, so you have to initialize the WebAssembly module (the default export) before you call `zip_files`. The init function loads `rust_wasm_zipper_bg.wasm` relative to the module, which works with Vite, webpack 5, and native ES modules. Calling it again after the first time does nothing.

### Downloading a ZIP

You can import the package dynamically inside the download function. The wasm then loads only when someone actually downloads something.

```ts
export async function downloadZip(files: File[], fileName = "archive.zip") {
  const { default: initWasm, zip_files } = await import("rust-wasm-zipper");
  await initWasm();

  const stream = zip_files(
    files.map((file) => ({ name: file.name, source: file })),
  );

  // Collect the stream into a Blob and save it with a temporary link.
  const blob = await new Response(stream).blob();
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = fileName;
  a.click();
  URL.revokeObjectURL(url);
}
```

Connect it to a file input:

```ts
const input = document.querySelector<HTMLInputElement>("#files")!;
input.addEventListener("change", () => {
  if (input.files?.length) {
    downloadZip([...input.files]);
  }
});
```

### Streaming to disk

The example above holds the whole archive in memory as a `Blob`. For large archives, use the File System Access API (Chromium-based browsers) and pipe the archive straight into the file being saved:

```ts
export async function saveZipToDisk(
  files: File[],
  suggestedName = "archive.zip",
) {
  const { default: initWasm, zip_files } = await import("rust-wasm-zipper");
  await initWasm();

  const handle = await window.showSaveFilePicker({
    suggestedName,
    types: [
      { description: "ZIP archive", accept: { "application/zip": [".zip"] } },
    ],
  });

  const stream = zip_files(
    files.map((file) => ({ name: file.name, source: file })),
  );
  await stream.pipeTo(await handle.createWritable());
}
```

TypeScript's built-in DOM types don't include `showSaveFilePicker`. To get them, install `@types/wicg-file-system-access`.

### Mixing sources

Entries in one archive can use different source types. Use `/` in a name to put the entry in a folder.

```ts
import initWasm, { zip_files, type ZipSource } from "rust-wasm-zipper";

export async function downloadReport(photos: File[]) {
  await initWasm();

  const entries: { name: string; source: ZipSource }[] = [
    // Plain bytes
    {
      name: "README.txt",
      source: new TextEncoder().encode("Generated report\n"),
    },
    // A Blob built in the page
    {
      name: "data/summary.json",
      source: new Blob([JSON.stringify({ count: photos.length })], {
        type: "application/json",
      }),
    },
    // A fetch response body, streamed as it downloads
    { name: "data/remote.csv", source: await fetch("/api/export.csv") },
    // Files the user picked, placed in a folder
    ...photos.map((photo) => ({ name: `photos/${photo.name}`, source: photo })),
  ];

  const blob = await new Response(zip_files(entries)).blob();
  const url = URL.createObjectURL(blob);
  const a = Object.assign(document.createElement("a"), {
    href: url,
    download: "report.zip",
  });
  a.click();
  URL.revokeObjectURL(url);
}
```

Sources are opened one at a time, in order, while the archive is written. A `fetch` you pass in starts right away, but its body isn't read until that entry's turn.

### Error handling

`zip_files` never throws. Problems with the input make the returned stream error instead: a name that isn't a string, an unsupported or already-locked source, a source that fails partway through, or an archive that goes over the ZIP limits. The error shows up wherever the stream is consumed:

```ts
try {
  const blob = await new Response(zip_files(entries)).blob();
  // ...
} catch (err) {
  console.error("Failed to build ZIP:", err);
}
```

If you cancel the stream (for example, by aborting a `pipeTo`), the archive stops being written.

## API

```ts
export default function init(
  input?: InitInput | Promise<InitInput>,
): Promise<InitOutput>;

export function zip_files(
  files: { name: string; source: ZipSource }[],
): ReadableStream;

export type ZipSource =
  | ReadableStream<Uint8Array>
  | Blob
  | Response
  | FileSystemFileHandle
  | ArrayBuffer
  | ArrayBufferView;
```

| Source                       | How it's read                                 |
| ---------------------------- | --------------------------------------------- |
| `ReadableStream<Uint8Array>` | Used as is. Chunks must be `Uint8Array`s.     |
| `Blob` / `File`              | `blob.stream()`                               |
| `Response`                   | `response.body` (an error if there's no body) |
| `FileSystemFileHandle`       | `(await handle.getFile()).stream()`           |
| `ArrayBuffer` / typed array  | Wrapped in a `Blob` and streamed              |

## Building from source

Requirements: [Rust](https://rustup.rs) with the `wasm32-unknown-unknown` target, and [wasm-pack](https://rustwasm.github.io/wasm-pack/).

```sh
rustup target add wasm32-unknown-unknown
wasm-pack build --target web --release   # or: just wasm-build
```

The output goes to `pkg/`. To install a local build in another project, run `npm install ../rust-wasm-zipper/pkg`, or run `npm pack` inside `pkg/` and install the resulting tarball.

## Releasing

Push a tag in the form `vX.Y.Z`:

```sh
git tag vX.Y.Z
git push origin vX.Y.Z
```

The [release workflow](.github/workflows/release.yml) builds the package, sets the `package.json` version from the tag, and attaches `rust-wasm-zipper-vX.Y.Z.tgz` to a new GitHub release.
