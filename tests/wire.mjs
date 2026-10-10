// Test support for the node tests: the vectors of tests/wire.json, and the
// bytes that the plugin's commands answer.
import { readFileSync } from 'node:fs'

const encoder = new TextEncoder()

export const wire = JSON.parse(readFileSync(new URL('./wire.json', import.meta.url), 'utf8'))

// The bytes of a vector's list.
export function bytes(parts) {
  return Uint8Array.from(
    parts.flatMap((part) => (typeof part === 'string' ? [...encoder.encode(part)] : [part]))
  )
}

// A response frame as `frame` in src/commands.rs builds it.
export function frame(status, headers = [], id = null, initial = '') {
  const head = encoder.encode(JSON.stringify({ status, headers, id }))
  const body = encoder.encode(initial)
  const out = new Uint8Array(4 + head.length + body.length)
  new DataView(out.buffer).setUint32(0, head.length)
  out.set(head, 4)
  out.set(body, 4 + head.length)
  return out.buffer
}

// A body chunk as `Chunk::into_bytes` builds it, with text or bytes as data.
export function chunk(flag, data = '') {
  const payload = typeof data === 'string' ? encoder.encode(data) : data
  const out = new Uint8Array(1 + payload.length)
  out[0] = flag
  out.set(payload, 1)
  return out.buffer
}

// Records as `Record::encode` writes them, from ['text', 'hi'],
// ['binary', [1, 2]], ['close', 1000, 'bye'] or ['error', 'why'].
export function batch(...items) {
  const parts = items.map(([kind, ...rest]) => {
    if (kind === 'text') {
      return [0, encoder.encode(rest[0])]
    }
    if (kind === 'binary') {
      return [1, Uint8Array.from(rest[0])]
    }
    if (kind === 'close') {
      const reason = encoder.encode(rest[1] ?? '')
      const payload = new Uint8Array(2 + reason.length)
      new DataView(payload.buffer).setUint16(0, rest[0])
      payload.set(reason, 2)
      return [2, payload]
    }
    return [3, encoder.encode(rest[0])]
  })
  const out = new Uint8Array(parts.reduce((length, [, payload]) => length + 5 + payload.length, 0))
  const view = new DataView(out.buffer)
  let offset = 0
  for (const [kind, payload] of parts) {
    out[offset] = kind
    view.setUint32(offset + 1, payload.length)
    out.set(payload, offset + 5)
    offset += 5 + payload.length
  }
  return out.buffer
}
