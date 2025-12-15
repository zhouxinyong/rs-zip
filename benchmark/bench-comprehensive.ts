import { Bench } from 'tinybench'
import { zip } from '../index.js'
import zipDir from 'zip-dir'
import { join } from 'path'
import { existsSync, mkdirSync, writeFileSync, rmSync, statSync, readdirSync } from 'fs'
import chalk from 'chalk'

const BENCH_DIR = join(process.cwd(), 'temp_bench_comprehensive')

interface BenchmarkScenario {
  name: string
  fileCount: number
  fileSize: number
  description: string
}

const scenarios: BenchmarkScenario[] = [
  {
    name: 'tiny-files',
    fileCount: 1000,
    fileSize: 100, // 100 bytes
    description: '1000 tiny files (100B each)',
  },
  {
    name: 'small-files',
    fileCount: 500,
    fileSize: 10 * 1024, // 10KB
    description: '500 small files (10KB each)',
  },
  {
    name: 'medium-files',
    fileCount: 100,
    fileSize: 100 * 1024, // 100KB
    description: '100 medium files (100KB each)',
  },
  {
    name: 'large-files',
    fileCount: 20,
    fileSize: 1024 * 1024, // 1MB
    description: '20 large files (1MB each)',
  },
]

// Generate random content for better compression testing
function generateContent(size: number): string {
  const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789\n '
  let content = ''
  for (let i = 0; i < size; i++) {
    content += chars.charAt(Math.floor(Math.random() * chars.length))
  }
  return content
}

function setupScenario(scenario: BenchmarkScenario): string {
  const srcDir = join(BENCH_DIR, scenario.name, 'src')

  if (existsSync(srcDir)) {
    rmSync(srcDir, { recursive: true, force: true })
  }
  mkdirSync(srcDir, { recursive: true })

  console.log(chalk.gray(`  Setting up ${scenario.description}...`))

  for (let i = 0; i < scenario.fileCount; i++) {
    writeFileSync(join(srcDir, `file_${i}.txt`), generateContent(scenario.fileSize))
  }

  return srcDir
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes}B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)}MB`
}

function getDirectorySize(dirPath: string): number {
  let totalSize = 0
  const files = readdirSync(dirPath, { recursive: true })

  for (const file of files) {
    const filePath = join(dirPath, String(file))
    try {
      const stats = statSync(filePath)
      if (stats.isFile()) {
        totalSize += stats.size
      }
    } catch (e) {
      // Skip if file doesn't exist
    }
  }

  return totalSize
}

async function runBenchmark(scenario: BenchmarkScenario) {
  console.log(chalk.bold.cyan(`\n📊 ${scenario.description}`))
  console.log(chalk.gray('─'.repeat(60)))

  const srcDir = setupScenario(scenario)
  const outRsZip = join(BENCH_DIR, scenario.name, 'rs.zip')
  const outNodeZip = join(BENCH_DIR, scenario.name, 'node.zip')

  const totalSize = getDirectorySize(srcDir)
  console.log(chalk.gray(`  Total data size: ${formatBytes(totalSize)}`))

  const bench = new Bench({ time: 3000 })

  bench.add('rs-zip (Rust)', async () => {
    await zip(srcDir, outRsZip, { level: 1 })
  })

  bench.add('zip-dir (JS)', async () => {
    await new Promise<void>((resolve, reject) => {
      zipDir(srcDir, { saveTo: outNodeZip }, (err: any) => {
        if (err) reject(err)
        else resolve()
      })
    })
  })

  await bench.run()

  // Get results
  const results = bench.tasks.map((task) => ({
    name: task.name,
    opsPerSec: task.result?.hz || 0,
    avgTime: task.result?.mean ? task.result.mean * 1000 : 0, // Convert to ms
  }))

  const rsZipResult = results.find((r) => r.name === 'rs-zip (Rust)')!
  const zipDirResult = results.find((r) => r.name === 'zip-dir (JS)')!

  const speedup = rsZipResult.opsPerSec / zipDirResult.opsPerSec

  // Get compressed sizes
  const rsZipSize = existsSync(outRsZip) ? statSync(outRsZip).size : 0
  const nodeZipSize = existsSync(outNodeZip) ? statSync(outNodeZip).size : 0

  console.log(
    chalk.green(
      `  ✓ rs-zip:   ${rsZipResult.avgTime.toFixed(1)}ms (${rsZipResult.opsPerSec.toFixed(1)} ops/s) → ${formatBytes(rsZipSize)}`,
    ),
  )
  console.log(
    chalk.yellow(
      `  ✓ zip-dir:  ${zipDirResult.avgTime.toFixed(1)}ms (${zipDirResult.opsPerSec.toFixed(1)} ops/s) → ${formatBytes(nodeZipSize)}`,
    ),
  )
  console.log(chalk.bold.magenta(`  ⚡ Speedup: ${speedup.toFixed(2)}x faster`))

  return {
    scenario: scenario.description,
    totalSize,
    rsZipTime: rsZipResult.avgTime,
    zipDirTime: zipDirResult.avgTime,
    speedup,
    rsZipSize,
    nodeZipSize,
  }
}

async function main() {
  console.log(chalk.bold.blue('\n🚀 Comprehensive Benchmark Suite\n'))

  const allResults = []

  for (const scenario of scenarios) {
    const result = await runBenchmark(scenario)
    allResults.push(result)
  }

  // Summary table
  console.log(chalk.bold.cyan('\n\n📈 Summary\n'))
  console.log(chalk.gray('─'.repeat(80)))

  console.table(
    allResults.map((r) => ({
      Scenario: r.scenario,
      'Data Size': formatBytes(r.totalSize),
      'rs-zip (ms)': r.rsZipTime.toFixed(1),
      'zip-dir (ms)': r.zipDirTime.toFixed(1),
      Speedup: `${r.speedup.toFixed(2)}x`,
      Compressed: formatBytes(r.rsZipSize),
    })),
  )

  // Cleanup
  if (existsSync(BENCH_DIR)) {
    rmSync(BENCH_DIR, { recursive: true, force: true })
  }

  console.log(chalk.green('\n✅ Benchmark complete!\n'))
}

main().catch(console.error)
