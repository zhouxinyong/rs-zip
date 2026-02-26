# rs-zip

> A high-performance Node.js package for compressing and decompressing directories using Rust.

## Features

- **High Performance**: Powered by Rust with streaming I/O and optimized buffering (constant memory usage regardless of file size).
- **Non-blocking**: Asynchronous API running on Rust thread pool, keeping the Node.js event loop free.
- **Security**: Prevents "Zip Slip" vulnerabilities during decompression.
- **Cross-Platform**: Consistent behavior on Windows, macOS, and Linux.
- **Advanced Features**:
  - Preserves file permissions (Unix execution bits).
  - Supports Zip64 for large files (> 4GB).
  - Glob pattern filtering (exclude files).
  - Multiple compression algorithms (Deflate, Bzip2, Zstd).
  - Configurable symlink handling.

## Installation

```bash
yarn add @rsdx/rs-zip
# or
npm install @rsdx/rs-zip
```

## Usage

### Compress a Directory

```javascript
const { zip } = require('@rsdx/rs-zip')

async function compress() {
  try {
    // Basic usage
    const count = await zip('./src', './archive.zip')
    console.log(`Successfully compressed ${count} files.`)

    // With options
    const count2 = await zip('./src', './archive_filtered.zip', {
      level: 9, // 0-9 for deflate, 1-9 for bzip2, 1-22 for zstd
      exclude: ['*.tmp', '.git/**', 'node_modules/**'], // Glob patterns
      algorithm: 'deflate', // "deflate" (default), "bzip2", or "zstd"
      followSymlinks: true, // Follow symbolic links (default: false)
    })
  } catch (err) {
    console.error('Compression failed:', err)
  }
}

compress()
```

### Decompress a Archive

```javascript
const { unzip } = require('@rsdx/rs-zip')

async function decompress() {
  try {
    await unzip('./archive.zip', './output_dir')
    console.log('Decompression completed')
  } catch (err) {
    console.error('Decompression failed:', err)
  }
}

decompress()
```

## API

### `zip(sourceDir: string, outputPath: string, options?: ZipOptions): Promise<number>`

Compresses a directory into a zip file. Returns the number of files compressed.

**Options:**

- `level` (number): Compression level. Default: `1`. Range depends on algorithm:
  - `deflate`: 0 (store) to 9 (best)
  - `bzip2`: 1 to 9
  - `zstd`: 1 to 22
- `exclude` (string[]): Array of glob patterns to exclude from the archive.
- `algorithm` (string): Compression algorithm. Options: `"deflate"` (default), `"bzip2"`, `"zstd"`.
- `followSymlinks` (boolean): Whether to follow symbolic links. Default: `false`.

### `unzip(sourcePath: string, outputDir: string): Promise<void>`

Decompresses a zip file into a directory.

- Automatically creates output directory if it doesn't exist.
- Safely handles paths to prevent writing outside the target directory.
- Restores file permissions on Unix systems.

## Development

- **Build**: `npm run build`
- **Test**: `npm test`
- **Benchmark**: `npm run bench`

## Credits

Rust zip implementation powered by [zip](https://crates.io/crates/zip).

## License

MIT
