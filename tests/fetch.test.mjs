// Runs `src/fetch.js` against a stubbed page and Tauri IPC: `node --test 'tests/*.test.mjs'`.
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { test } from 'node:test'

const source = readFileSync(new URL('../src/fetch.js', import.meta.url), 'utf8')

const MACOS = 'leptos://localhost'
const ANDROID = 'http://leptos.localhost'

// A frame as `src/commands.rs` builds it.
function frame(status, headers, body) {
  const head = new TextEncoder().encode(JSON.stringify({ status, headers }))
  const bytes = new TextEncoder().encode(body)
  const out = new Uint8Array(4 + head.length + bytes.length)
  new DataView(out.buffer).setUint32(0, head.length)
  out.set(head, 4)
  out.set(bytes, 4 + head.length)
  return out.buffer
}

function page(origin, href, reply = () => frame(200, [], '')) {
  const native = []
  const invoked = []
  const window = {
    fetch: async (input, init) => {
      native.push({ input, init })
      return new Response('native')
    },
    __TAURI_INTERNALS__: {
      invoke: async (cmd, args) => {
        invoked.push({ cmd, args })
        return reply()
      }
    }
  }
  const run = new Function('window', 'location', source.replace('__LEPTOS_SSR_ORIGIN__', JSON.stringify(origin)))
  run(window, new URL(href))
  return { window, native, invoked }
}

test('other origins keep the native fetch', () => {
  const { window } = page(MACOS, 'tauri://localhost/')
  assert.equal(window.fetch.name, 'fetch')
})

for (const [origin, href] of [
  [MACOS, 'leptos://localhost/page'],
  [ANDROID, 'http://leptos.localhost/page']
]) {
  test(`GET stays native on ${origin}`, async () => {
    const { window, native, invoked } = page(origin, href)
    await window.fetch('/pkg/app.wasm')
    await window.fetch(new Request(`${origin}/api/get_fn?x=1`))
    assert.equal(native.length, 2)
    assert.equal(invoked.length, 0)
  })

  test(`same-origin POST goes over IPC on ${origin}`, async () => {
    const { window, native, invoked } = page(origin, href, () =>
      frame(200, [['content-type', 'text/plain'], ['serverfnredirect', '1']], 'hi')
    )
    const response = await window.fetch('/api/greet', {
      method: 'post',
      body: new URLSearchParams({ name: 'Ada' })
    })
    assert.equal(native.length, 0)
    assert.equal(invoked.length, 1)
    assert.equal(invoked[0].cmd, 'plugin:leptos-ssr|fetch')
    const { request } = invoked[0].args
    assert.equal(request.method, 'POST')
    assert.equal(request.url, `${origin}/api/greet`)
    assert.match(Object.fromEntries(request.headers)['content-type'], /^application\/x-www-form-urlencoded/)
    assert.equal(new TextDecoder().decode(Uint8Array.from(request.body)), 'name=Ada')
    assert.equal(response.status, 200)
    assert.equal(response.headers.get('serverfnredirect'), '1')
    assert.equal(await response.text(), 'hi')
  })
}

test('Request objects keep their method, URL and body', async () => {
  const { window, invoked } = page(MACOS, 'leptos://localhost/')
  const request = new Request(`${MACOS}/api/echo`, { method: 'PUT', body: 'raw' })
  await window.fetch(request)
  assert.equal(invoked[0].args.request.method, 'PUT')
  assert.equal(invoked[0].args.request.url, `${MACOS}/api/echo`)
  assert.equal(new TextDecoder().decode(Uint8Array.from(invoked[0].args.request.body)), 'raw')
})

test('IPC and foreign origins stay native', async () => {
  const { window, native, invoked } = page(MACOS, 'leptos://localhost/')
  await window.fetch('ipc://localhost/plugin%3Aleptos-ssr%7Cfetch', { method: 'POST', body: '{}' })
  await window.fetch('http://ipc.localhost/x', { method: 'POST', body: '{}' })
  await window.fetch('https://example.com/api', { method: 'POST', body: '{}' })
  assert.equal(native.length, 3)
  assert.equal(invoked.length, 0)
})

test('frames arrive as plain arrays after the postMessage fallback', async () => {
  const { window } = page(ANDROID, 'http://leptos.localhost/', () =>
    Array.from(new Uint8Array(frame(201, [], 'made')))
  )
  const response = await window.fetch('/api/create', { method: 'POST', body: 'x' })
  assert.equal(response.status, 201)
  assert.equal(await response.text(), 'made')
})

test('null-body statuses have no body', async () => {
  const { window } = page(MACOS, 'leptos://localhost/', () => frame(204, [], ''))
  const response = await window.fetch('/api/nothing', { method: 'DELETE' })
  assert.equal(response.status, 204)
  assert.equal(response.body, null)
})

test('IPC failures reject like a network error', async () => {
  const { window } = page(MACOS, 'leptos://localhost/', () => {
    throw 'not allowed'
  })
  await assert.rejects(window.fetch('/api/x', { method: 'POST', body: 'x' }), TypeError)
})
