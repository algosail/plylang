import assert from 'node:assert/strict'
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import { checkRelease } from './check-release.js'
import { stageNative } from './stage-native.js'

const root = join(import.meta.dirname, '..')

test('the release manifest matches the compiler version', () => {
  assert.deepEqual(checkRelease(root), [])
  assert.deepEqual(checkRelease(root, 'v1.0.5'), [])
  assert.match(checkRelease(root, 'v0.0.0').join('\n'), /release tag v0.0.0/)
})

test('stages one platform package', () => {
  const dir = mkdtempSync(join(tmpdir(), 'plylang-stage-'))
  const built = join(dir, 'built')
  const out = join(dir, 'out')
  mkdirSync(built)
  for (const name of ['plylang', 'plylang-ls']) {
    const file = join(built, name)
    writeFileSync(file, '#!/bin/sh\n')
    chmodSync(file, 0o755)
  }
  stageNative('linux-x64', built, out, '1.0.0')
  const pkg = JSON.parse(readFileSync(join(out, 'package.json'), 'utf8'))
  assert.equal(pkg.name, '@plylang/plylang-linux-x64')
  assert.equal(pkg.version, '1.0.0')
  assert.deepEqual(pkg.os, ['linux'])
  assert.deepEqual(pkg.cpu, ['x64'])
  assert.equal(readFileSync(join(out, 'bin', 'plylang'), 'utf8'), '#!/bin/sh\n')
  assert.equal(readFileSync(join(out, 'LICENSE'), 'utf8').slice(0, 3), 'MIT')
  assert.throws(() => stageNative('sunos-x64', built, out, '1.0.0'), /unknown platform/)
})
