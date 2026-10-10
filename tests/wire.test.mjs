// Pins the builders of tests/wire.mjs to the vectors of tests/wire.json, which
// the Rust tests pin to the plugin: `node --test 'tests/*.test.mjs'`.
import assert from 'node:assert/strict'
import { test } from 'node:test'

import { batch, bytes, chunk, frame, wire } from './wire.mjs'

const same = (buffer, parts) => assert.deepEqual(new Uint8Array(buffer), bytes(parts))

test('frames match the vector', () => {
  const { status, headers, id, body } = wire.frame
  same(frame(status, headers, id, body), wire.frame.bytes)
})

test('chunks match the vectors', () => {
  const flags = { data: 0, last: 1, idle: 2 }
  for (const vector of wire.chunks) {
    same(chunk(flags[vector.flag], vector.data), vector.bytes)
  }
})

test('records match the vectors', () => {
  for (const { bytes: parts, ...record } of wire.records) {
    const [[kind, value]] = Object.entries(record)
    same(batch(kind === 'close' ? [kind, ...value] : [kind, value]), parts)
  }
})
