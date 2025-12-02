# rs-zip

> A high-performance Node.js package for compressing and decompressing directories using Rust.

## Features

- **High Performance**: Powered by Rust with optimized buffering (BufWriter/BufReader).
- **Non-blocking**: Asynchronous API running on Rust thread pool, keeping the Node.js event loop free.
- **Security**: Prevents "Zip Slip" vulnerabilities during decompression.
- **Cross-Platform**: Consistent behavior on Windows, macOS, and Linux.
- **Advanced Features**:
  - Preserves file permissions (Unix execution bits).
  - Supports Zip64 for large files (> 4GB).
  - Glob pattern filtering (exclude files).

## Installation

```bash
yarn add rs-zip
# or
npm install rs-zip
```

## Usage

### Compress a Directory

```javascript
const { zip } = require('rs-zip')

async function compress() {
  try {
    // Basic usage
    const count = await zip('./src', './archive.zip')
    console.log(`Successfully compressed ${count} files.`)

    // With options
    const count2 = await zip('./src', './archive_filtered.zip', {
      level: 9, // 0-9, default is 6
      exclude: ['*.tmp', '.git/**', 'node_modules/**'], // Glob patterns
    })
  } catch (err) {
    console.error('Compression failed:', err)
  }
}

compress()
```

### Decompress a Archive

```javascript
const { unzip } = require('rs-zip')

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

- `level` (number): Compression level from 0 (store) to 9 (best). Default: `6`.
- `exclude` (string[]): Array of glob patterns to exclude from the archive.

### `unzip(sourcePath: string, outputDir: string): Promise<void>`

Decompresses a zip file into a directory.

- Automatically creates output directory if it doesn't exist.
- Safely handles paths to prevent writing outside the target directory.
- Restores file permissions on Unix systems.

## Development

- **Build**: `npm run build`
- **Test**: `npm test`
- **Benchmark**: `npm run bench`

## License

MIT
