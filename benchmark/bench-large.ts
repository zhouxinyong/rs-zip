import { zip } from '../index.js'
import { join } from 'path'
import { existsSync, mkdirSync, writeFileSync, rmSync } from 'fs'
import { performance } from 'perf_hooks'

const BENCH_DIR = join(process.cwd(), 'temp_bench_dir')
const SRC_DIR = join(BENCH_DIR, 'src')
const OUT_ZIP = join(BENCH_DIR, 'bench.zip')
const FILE_COUNT = 1000

function setup() {
  if (existsSync(BENCH_DIR)) {
    rmSync(BENCH_DIR, { recursive: true, force: true })
  }
  mkdirSync(SRC_DIR, { recursive: true })

  console.log(`Generating ${FILE_COUNT} files...`)
  for (let i = 0; i < FILE_COUNT; i++) {
    writeFileSync(join(SRC_DIR, `file_${i}.txt`), `Content for file ${i}.This is some random text to compress.`)
  }
}

function teardown() {
  if (existsSync(BENCH_DIR)) {
    rmSync(BENCH_DIR, { recursive: true, force: true })
  }
}

async function run() {
  try {
    setup()

    console.log('Starting compression...')
    const start = performance.now()

    const count = await zip(SRC_DIR, OUT_ZIP)

    const end = performance.now()
    const duration = end - start

    console.log(`Compressed ${count} files in ${duration.toFixed(2)} ms`)
  } catch (e) {
    console.error('Benchmark failed:', e)
  } finally {
    teardown()
  }
}

run()
