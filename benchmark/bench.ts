import { Bench } from 'tinybench'
import { zip } from '../index.js'
import zipDir from 'zip-dir'
import { join } from 'path'
import { existsSync, mkdirSync, writeFileSync, rmSync } from 'fs'

const BENCH_DIR = join(process.cwd(), 'temp_bench_compare_dir')
const SRC_DIR = join(BENCH_DIR, 'src')
const OUT_RS_ZIP = join(BENCH_DIR, 'rs.zip')
const OUT_NODE_ZIP = join(BENCH_DIR, 'node.zip')
const FILE_COUNT = 600

// Setup files
if (existsSync(BENCH_DIR)) {
  rmSync(BENCH_DIR, { recursive: true, force: true })
}
mkdirSync(SRC_DIR, { recursive: true })
console.log(`Generating ${FILE_COUNT} files for benchmark...`)
for (let i = 0; i < FILE_COUNT; i++) {
  writeFileSync(join(SRC_DIR, `file_${i}.txt`), `Content for file ${i}. This is some random text to compress.`)
}

const b = new Bench({ time: 3000 }) // Run for at least 3 seconds

b.add('rs-zip (Rust)', async () => {
  await zip(SRC_DIR, OUT_RS_ZIP)
})

b.add('zip-dir (JS)', async () => {
  await new Promise<void>((resolve, reject) => {
    zipDir(SRC_DIR, { saveTo: OUT_NODE_ZIP }, (err) => {
      if (err) reject(err)
      else resolve()
    })
  })
})

console.log('Running benchmark...')
await b.run()

console.table(b.table())

// Cleanup
if (existsSync(BENCH_DIR)) {
  rmSync(BENCH_DIR, { recursive: true, force: true })
}
