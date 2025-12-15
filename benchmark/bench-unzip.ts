import { Bench } from 'tinybench'
import { zip, unzip } from '../index.js'
import AdmZip from 'adm-zip'
import { join } from 'path'
import { existsSync, mkdirSync, writeFileSync, rmSync } from 'fs'

const BENCH_DIR = join(process.cwd(), 'temp_bench_unzip_dir')
const SRC_DIR = join(BENCH_DIR, 'src')
const ZIP_FILE = join(BENCH_DIR, 'test.zip')
const OUT_RS = join(BENCH_DIR, 'out_rs')
const OUT_ADM = join(BENCH_DIR, 'out_adm')
const FILE_COUNT = 500

// Setup
if (existsSync(BENCH_DIR)) {
  rmSync(BENCH_DIR, { recursive: true, force: true })
}
mkdirSync(SRC_DIR, { recursive: true })
// Generate random files
console.log(`Generating ${FILE_COUNT} files for unzip benchmark...`)
for (let i = 0; i < FILE_COUNT; i++) {
  writeFileSync(
    join(SRC_DIR, `file_${i}.txt`),
    `Content for file ${i}. This is larger content to test decompression speed.`.repeat(20),
  )
}

// Create source zip
console.log('Creating source zip file...')
await zip(SRC_DIR, ZIP_FILE)

const b = new Bench({ time: 3000 })

b.add('rs-zip (Rust)', async () => {
  await unzip(ZIP_FILE, OUT_RS)
})

b.add('adm-zip (JS)', async () => {
  const zip = new AdmZip(ZIP_FILE)
  zip.extractAllTo(OUT_ADM, true)
})

console.log('Running unzip benchmark...')
await b.run()

console.table(b.table())

// Cleanup
if (existsSync(BENCH_DIR)) {
  rmSync(BENCH_DIR, { recursive: true, force: true })
}
