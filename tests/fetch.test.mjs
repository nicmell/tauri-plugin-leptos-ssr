// Runs `src/fetch.js` against a stubbed page and Tauri IPC: `node --test 'tests/*.test.mjs'`.
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { test } from 'node:test'

const source = readFileSync(new URL('../src/fetch.js', import.meta.url), 'utf8')

const MACOS = 'leptos://localhost'
const ANDROID = 'http://leptos.localhost'
const encoder = new TextEncoder()
const decoder = new TextDecoder()

// A response frame as `src/commands.rs` builds it.
function frame(status, headers = [], id = null, initial = '') {
  const head = encoder.encode(JSON.stringify({ status, headers, id }))
  const bytes = encoder.encode(initial)
  const out = new Uint8Array(4 + head.length + bytes.length)
  new DataView(out.buffer).setUint32(0, head.length)
  out.set(head, 4)
  out.set(bytes, 4 + head.length)
  return out.buffer
}

// A body chunk as `Chunk::into_bytes` builds it: 0 data, 1 last, 2 idle.
function chunk(flag, text = '') {
  const bytes = encoder.encode(text)
  const out = new Uint8Array(1 + bytes.length)
  out[0] = flag
  out.set(bytes, 1)
  return out.buffer
}

function deferred() {
  let resolve
  let reject
  const promise = new Promise((res, rej) => {
    resolve = res
    reject = rej
  })
  return { promise, resolve, reject }
}

// `commands` answers each IPC command; every call is recorded.
function page(origin, href, commands = {}) {
  const native = []
  const calls = []
  const window = {
    fetch: async (input, init) => {
      native.push({ input, init })
      return new Response('native')
    },
    __TAURI_INTERNALS__: {
      invoke: async (cmd, payload, options) => {
        calls.push({ cmd, payload, options })
        const name = cmd.replace('plugin:leptos-ssr|', '')
        if (!commands[name]) {
          throw new Error(`unexpected ${cmd}`)
        }
        return commands[name](payload, options)
      }
    }
  }
  const run = new Function(
    'window',
    'location',
    source.replace('__LEPTOS_SSR_ORIGIN__', JSON.stringify(origin))
  )
  run(window, new URL(href))
  const named = (name) => calls.filter((call) => call.cmd === `plugin:leptos-ssr|${name}`)
  return { window, native, calls, named }
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0))

test('other origins keep the native fetch', () => {
  const { window } = page(MACOS, 'tauri://localhost/')
  assert.equal(window.fetch.name, 'fetch')
})

for (const [origin, href] of [
  [MACOS, 'leptos://localhost/page'],
  [ANDROID, 'http://leptos.localhost/page']
]) {
  test(`GET stays native on ${origin}`, async () => {
    const { window, native, calls } = page(origin, href)
    await window.fetch('/pkg/app.wasm')
    await window.fetch(new Request(`${origin}/api/get_fn?x=1`))
    assert.equal(native.length, 2)
    assert.equal(calls.length, 0)
  })

  test(`a POST goes over IPC with a raw body on ${origin}`, async () => {
    const { window, native, named } = page(origin, href, {
      fetch: () => frame(200, [['content-type', 'text/plain'], ['serverfnredirect', '1']], null, 'hi')
    })
    const response = await window.fetch('/api/greet', {
      method: 'post',
      body: new URLSearchParams({ name: 'Ada' })
    })
    assert.equal(native.length, 0)
    const [{ payload, options }] = named('fetch')
    assert.ok(payload instanceof Uint8Array)
    assert.equal(decoder.decode(payload), 'name=Ada')
    assert.equal(Object.getPrototypeOf(options.headers), Object.prototype)
    const head = JSON.parse(decodeURIComponent(options.headers['leptos-ssr-request']))
    assert.equal(head.method, 'POST')
    assert.equal(head.url, `${origin}/api/greet`)
    assert.match(Object.fromEntries(head.headers)['content-type'], /^application\/x-www-form-urlencoded/)
    assert.equal(response.status, 200)
    assert.equal(response.headers.get('serverfnredirect'), '1')
    assert.equal(await response.text(), 'hi')
  })
}

test('complete bodies take one round trip', async () => {
  const { window, calls } = page(MACOS, 'leptos://localhost/', {
    fetch: () => frame(200, [], null, 'whole')
  })
  const response = await window.fetch('/api/x', { method: 'POST', body: 'x' })
  assert.equal(await response.text(), 'whole')
  assert.equal(calls.length, 1)
})

test('streamed bodies arrive chunk by chunk', async () => {
  const reads = [deferred(), deferred(), deferred(), deferred()]
  let next = 0
  const { window, named } = page(MACOS, 'leptos://localhost/', {
    fetch: () => frame(200, [['content-type', 'text/plain']], 7, 'a'),
    fetch_read_body: ({ id }) => {
      assert.equal(id, 7)
      return reads[next++].promise
    }
  })
  const response = await window.fetch('/api/stream', { method: 'POST', body: 'x' })
  const reader = response.body.getReader()

  assert.equal(decoder.decode((await reader.read()).value), 'a')
  const second = reader.read()
  await settle()
  reads[0].resolve(chunk(2))
  await settle()
  reads[1].resolve(chunk(0, 'b'))
  assert.equal(decoder.decode((await second).value), 'b')
  assert.equal(named('fetch_read_body').length, 2)

  const third = reader.read()
  await settle()
  reads[2].resolve(chunk(1, 'c'))
  assert.equal(decoder.decode((await third).value), 'c')
  assert.equal((await reader.read()).done, true)
  assert.equal(named('fetch_read_body').length, 3)
})

test('cancelling the body cancels the stream', async () => {
  const { window, named } = page(MACOS, 'leptos://localhost/', {
    fetch: () => frame(200, [], 3),
    fetch_cancel_body: () => null
  })
  const response = await window.fetch('/api/stream', { method: 'POST', body: 'x' })
  await response.body.cancel()
  assert.deepEqual(named('fetch_cancel_body').map((call) => call.payload), [{ id: 3 }])
})

test('an abort before the request rejects without IPC', async () => {
  const { window, calls } = page(MACOS, 'leptos://localhost/')
  const controller = new AbortController()
  controller.abort()
  await assert.rejects(
    window.fetch('/api/x', { method: 'POST', body: 'x', signal: controller.signal }),
    { name: 'AbortError' }
  )
  assert.equal(calls.length, 0)
})

test('an abort during the request cancels the stream that arrives late', async () => {
  const sent = deferred()
  const { window, named } = page(MACOS, 'leptos://localhost/', {
    fetch: () => sent.promise,
    fetch_cancel_body: () => null
  })
  const controller = new AbortController()
  const response = window.fetch('/api/x', { method: 'POST', body: 'x', signal: controller.signal })
  await settle()
  controller.abort()
  await assert.rejects(response, { name: 'AbortError' })
  sent.resolve(frame(200, [], 9))
  await settle()
  assert.deepEqual(named('fetch_cancel_body').map((call) => call.payload), [{ id: 9 }])
})

test('an abort after the response errors the body and drops late chunks', async () => {
  const read = deferred()
  const { window, named } = page(MACOS, 'leptos://localhost/', {
    fetch: () => frame(200, [], 4),
    fetch_read_body: () => read.promise,
    fetch_cancel_body: () => null
  })
  const controller = new AbortController()
  const response = await window.fetch('/api/x', { method: 'POST', body: 'x', signal: controller.signal })
  const reader = response.body.getReader()
  const pending = reader.read()
  await settle()
  controller.abort()
  await assert.rejects(pending, { name: 'AbortError' })
  read.resolve(chunk(0, 'late'))
  await settle()
  assert.deepEqual(named('fetch_cancel_body').map((call) => call.payload), [{ id: 4 }])
})

test('null-body statuses drop their stream', async () => {
  const { window, named } = page(MACOS, 'leptos://localhost/', {
    fetch: () => frame(204, [], 5),
    fetch_cancel_body: () => null
  })
  const response = await window.fetch('/api/nothing', { method: 'DELETE' })
  assert.equal(response.status, 204)
  assert.equal(response.body, null)
  assert.deepEqual(named('fetch_cancel_body').map((call) => call.payload), [{ id: 5 }])
})

test('plain arrays after the postMessage fallback decode the same', async () => {
  let reads = 0
  const { window } = page(ANDROID, 'http://leptos.localhost/', {
    fetch: () => Array.from(new Uint8Array(frame(201, [], 2, 'a'))),
    fetch_read_body: () => Array.from(new Uint8Array(reads++ === 0 ? chunk(1, 'b') : chunk(1)))
  })
  const response = await window.fetch('/api/create', { method: 'POST', body: 'x' })
  assert.equal(response.status, 201)
  assert.equal(await response.text(), 'ab')
})

test('a failing read errors the body', async () => {
  const { window } = page(MACOS, 'leptos://localhost/', {
    fetch: () => frame(200, [], 6),
    fetch_read_body: () => {
      throw 'the response body failed'
    }
  })
  const response = await window.fetch('/api/x', { method: 'POST', body: 'x' })
  await assert.rejects(response.text(), TypeError)
})

test('a failing request rejects like a network error', async () => {
  const { window } = page(MACOS, 'leptos://localhost/', {
    fetch: () => {
      throw 'not allowed'
    }
  })
  await assert.rejects(window.fetch('/api/x', { method: 'POST', body: 'x' }), TypeError)
})

test('Request objects keep their method, URL and body', async () => {
  const { window, named } = page(MACOS, 'leptos://localhost/', {
    fetch: () => frame(200)
  })
  await window.fetch(new Request(`${MACOS}/api/echo`, { method: 'PUT', body: 'raw' }))
  const [{ payload, options }] = named('fetch')
  const head = JSON.parse(decodeURIComponent(options.headers['leptos-ssr-request']))
  assert.equal(head.method, 'PUT')
  assert.equal(head.url, `${MACOS}/api/echo`)
  assert.equal(decoder.decode(payload), 'raw')
})

test('IPC and foreign origins stay native', async () => {
  const { window, native, calls } = page(MACOS, 'leptos://localhost/')
  await window.fetch('ipc://localhost/plugin%3Aleptos-ssr%7Cfetch', { method: 'POST', body: '{}' })
  await window.fetch('http://ipc.localhost/x', { method: 'POST', body: '{}' })
  await window.fetch('https://example.com/api', { method: 'POST', body: '{}' })
  assert.equal(native.length, 3)
  assert.equal(calls.length, 0)
})
