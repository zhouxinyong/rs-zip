import test from 'ava'
import rsZip from '../index.js'
const { zip, unzip } = rsZip
import { join } from 'path'
import { existsSync, mkdirSync, writeFileSync, readFileSync, rmSync, chmodSync, statSync, symlinkSync } from 'fs'

const TEST_DIR = join(process.cwd(), 'temp_test_dir')
const SRC_DIR = join(TEST_DIR, 'src')
const INVALID_ZIP = join(TEST_DIR, 'invalid.zip')

test.before(() => {
  if (existsSync(TEST_DIR)) {
    rmSync(TEST_DIR, { recursive: true, force: true })
  }
  mkdirSync(SRC_DIR, { recursive: true })
  writeFileSync(join(SRC_DIR, 'file1.txt'), 'Hello World')
  writeFileSync(join(SRC_DIR, 'file2.txt'), 'Rust Zip')
  mkdirSync(join(SRC_DIR, 'subdir'))
  writeFileSync(join(SRC_DIR, 'subdir', 'file3.txt'), 'Nested File')
  writeFileSync(join(SRC_DIR, 'ignore.tmp'), 'Should be ignored')
  writeFileSync(join(SRC_DIR, 'script.sh'), '#!/bin/bash\necho hi')

  // Set executable permission for script.sh (755)
  chmodSync(join(SRC_DIR, 'script.sh'), 0o755)

  // Create a symlink for symlink tests (Unix only)
  if (process.platform !== 'win32') {
    try {
      symlinkSync(join(SRC_DIR, 'file1.txt'), join(SRC_DIR, 'link.txt'))
    } catch {
      // Ignore if symlink creation fails
    }
  }
})

test.after.always(() => {
  if (existsSync(TEST_DIR)) {
    rmSync(TEST_DIR, { recursive: true, force: true })
  }
})

test('zip and unzip basic', async (t) => {
  const outZip = join(TEST_DIR, 'basic.zip')
  const outDir = join(TEST_DIR, 'out_basic')

  // 1. Zip
  const count = await zip(SRC_DIR, outZip)
  // 5 files total: file1, file2, subdir/file3, ignore.tmp, script.sh
  // (symlink is NOT followed by default, so link.txt is excluded)
  t.is(count, 5, 'Should compress 5 files')
  t.true(existsSync(outZip), 'Zip file should exist')

  // 2. Unzip
  await unzip(outZip, outDir)
  t.true(existsSync(outDir), 'Output directory should exist')

  // 3. Verify content
  t.is(readFileSync(join(outDir, 'file1.txt'), 'utf8'), 'Hello World')
})

test('zip with exclude', async (t) => {
  const outZip = join(TEST_DIR, 'exclude.zip')
  const outDir = join(TEST_DIR, 'out_exclude')

  // Exclude *.tmp
  const count = await zip(SRC_DIR, outZip, { level: 6, exclude: ['*.tmp'] })
  t.is(count, 4, 'Should compress 4 files (excluding .tmp)')

  await unzip(outZip, outDir)
  t.false(existsSync(join(outDir, 'ignore.tmp')), 'Ignored file should not exist')
  t.true(existsSync(join(outDir, 'file1.txt')), 'Regular file should exist')
})

test('zip preserves permissions (Unix)', async (t) => {
  if (process.platform === 'win32') {
    t.pass('Skipping permission test on Windows')
    return
  }

  const outZip = join(TEST_DIR, 'perm.zip')
  const outDir = join(TEST_DIR, 'out_perm')

  await zip(SRC_DIR, outZip)
  await unzip(outZip, outDir)

  const stat = statSync(join(outDir, 'script.sh'))
  // Check if executable bit is set (mode & 0o111)
  const isExecutable = (stat.mode & 0o111) !== 0
  t.true(isExecutable, 'Should preserve executable permission')
})

test('zip rejects invalid level', async (t) => {
  await t.throwsAsync(
    async () => {
      // @ts-ignore testing runtime validation
      await zip(SRC_DIR, INVALID_ZIP, { level: 42 })
    },
    { message: /between 0 and 9/ },
  )
})

test('zip rejects invalid level for zstd', async (t) => {
  await t.throwsAsync(
    async () => {
      await zip(SRC_DIR, INVALID_ZIP, { level: 30, algorithm: 'zstd' })
    },
    { message: /between 1 and 22/ },
  )
})

test('zip with zstd algorithm', async (t) => {
  const outZip = join(TEST_DIR, 'zstd.zip')
  const outDir = join(TEST_DIR, 'out_zstd')

  const count = await zip(SRC_DIR, outZip, { algorithm: 'zstd', level: 3 })
  t.true(count > 0, 'Should compress files with zstd')

  await unzip(outZip, outDir)
  t.is(readFileSync(join(outDir, 'file1.txt'), 'utf8'), 'Hello World')
})

test('zip with bzip2 algorithm', async (t) => {
  const outZip = join(TEST_DIR, 'bzip2.zip')
  const outDir = join(TEST_DIR, 'out_bzip2')

  const count = await zip(SRC_DIR, outZip, { algorithm: 'bzip2', level: 5 })
  t.true(count > 0, 'Should compress files with bzip2')

  await unzip(outZip, outDir)
  t.is(readFileSync(join(outDir, 'file1.txt'), 'utf8'), 'Hello World')
})

test('zip creates parent directories for output', async (t) => {
  const outZip = join(TEST_DIR, 'nested', 'deep', 'output.zip')
  const outDir = join(TEST_DIR, 'out_nested')

  const count = await zip(SRC_DIR, outZip)
  t.true(count > 0, 'Should compress files')
  t.true(existsSync(outZip), 'Zip file should exist in nested directory')

  await unzip(outZip, outDir)
  t.is(readFileSync(join(outDir, 'file1.txt'), 'utf8'), 'Hello World')
})

test('zip with followSymlinks', async (t) => {
  if (process.platform === 'win32') {
    t.pass('Skipping symlink test on Windows')
    return
  }

  const outZip = join(TEST_DIR, 'symlinks.zip')
  const outDir = join(TEST_DIR, 'out_symlinks')

  // With followSymlinks: true, the symlink target should be included
  const count = await zip(SRC_DIR, outZip, { followSymlinks: true })
  // 6 files: file1, file2, subdir/file3, ignore.tmp, script.sh, link.txt
  t.is(count, 6, 'Should compress 6 files (including symlink target)')

  await unzip(outZip, outDir)
  t.true(existsSync(join(outDir, 'link.txt')), 'Symlink target file should exist')
  t.is(readFileSync(join(outDir, 'link.txt'), 'utf8'), 'Hello World')
})

test('zip propagates directory read errors', async (t) => {
  await t.throwsAsync(
    async () => {
      await zip('/nonexistent/path', INVALID_ZIP)
    },
    { message: /Source not found/ },
  )
})
