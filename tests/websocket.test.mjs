// Runs the WebSocket part of `src/fetch.js` against a stubbed page and Tauri
// IPC: `node --test 'tests/*.test.mjs'`.
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { test } from 'node:test'

import { batch, bytes, wire } from './wire.mjs'

const source = readFileSync(new URL('../src/fetch.js', import.meta.url), 'utf8')

const MACOS = 'leptos://localhost'
const ANDROID = 'http://leptos.localhost'
const decoder = new TextDecoder()

// Node 22 has no CloseEvent.
globalThis.CloseEvent ??= class CloseEvent extends Event {
  constructor(type, init = {}) {
    super(type, init)
    this.wasClean = Boolean(init.wasClean)
    this.code = init.code ?? 0
    this.reason = init.reason ?? ''
  }
}

class NativeWebSocket {
  constructor(url, protocols) {
    this.url = url
    this.protocols = protocols
    this.native = true
  }
}

function deferred() {
  let resolve
  let reject
  const promise = new Promise((yes, no) => {
    resolve = yes
    reject = no
  })
  return { promise, resolve, reject }
}

// The records of a `ws_send` body, as ['text', string] or ['binary', bytes].
function decodeSent(body) {
  const view = new DataView(body.buffer, body.byteOffset, body.byteLength)
  const out = []
  for (let offset = 0; offset < body.length; ) {
    const length = view.getUint32(offset + 1)
    const payload = body.subarray(offset + 5, offset + 5 + length)
    out.push(body[offset] === 0 ? ['text', decoder.decode(payload)] : ['binary', [...payload]])
    offset += 5 + length
  }
  return out
}

// A page whose `ws_open`, `ws_read` and `ws_send` calls wait until the test
// settles them; `ws_close` answers at once.
function page(origin = MACOS) {
  const calls = []
  const opens = []
  const reads = []
  const sends = []
  const window = {
    WebSocket: NativeWebSocket,
    EventSource: class {},
    fetch: async () => new Response('native'),
    __TAURI_INTERNALS__: {
      invoke: (cmd, payload, options) => {
        calls.push({ cmd, payload, options })
        const next = deferred()
        if (cmd === 'plugin:leptos-ssr|ws_open') {
          opens.push(next)
        } else if (cmd === 'plugin:leptos-ssr|ws_read') {
          reads.push(next)
        } else if (cmd === 'plugin:leptos-ssr|ws_send') {
          sends.push(next)
        } else if (cmd === 'plugin:leptos-ssr|ws_close') {
          next.resolve(null)
        } else {
          throw new Error(`unexpected ${cmd}`)
        }
        return next.promise
      }
    }
  }
  const run = new Function('window', 'location', source.replace('__LEPTOS_SSR_ORIGIN__', JSON.stringify(origin)))
  run(window, new URL(`${origin}/page`))
  const called = (cmd) => calls.filter((call) => call.cmd === `plugin:leptos-ssr|${cmd}`)
  return { window, called, opens, reads, sends }
}

async function opened(window, opens, url = '/ws', protocols) {
  const socket = new window.WebSocket(url, protocols)
  opens.at(-1).resolve({ id: 7, protocol: '', extensions: '' })
  await until(() => socket.readyState === window.WebSocket.OPEN)
  return socket
}

async function until(predicate) {
  for (let i = 0; i < 200; i++) {
    if (predicate()) {
      return
    }
    await new Promise((resolve) => setTimeout(resolve, 1))
  }
  assert.fail('condition not reached')
}

test('own URLs get the stand-in, foreign ones the native class, and instanceof holds for both', () => {
  const { window } = page()
  const foreign = new window.WebSocket('wss://example.com/live')
  const reload = new window.WebSocket('ws://localhost:3001/live_reload')
  const own = new window.WebSocket('/ws')
  assert.equal(foreign.native, true)
  assert.equal(reload.native, true)
  assert.equal(own.native, undefined)
  assert.equal(own.url, `${MACOS}/ws`)
  assert.ok(foreign instanceof window.WebSocket)
  assert.ok(own instanceof window.WebSocket)
  assert.equal(own.readyState, window.WebSocket.CONNECTING)
  assert.equal(window.WebSocket.CLOSED, 3)
  assert.equal(own.CLOSING, 2)
  assert.equal(own.extensions, '')
})

test('Android pages reach the stand-in over http and ws on leptos.localhost', () => {
  const { window, called } = page(ANDROID)
  assert.equal(new window.WebSocket('/ws').url, 'ws://leptos.localhost/ws')
  assert.equal(new window.WebSocket('ws://leptos.localhost/a').url, 'ws://leptos.localhost/a')
  assert.equal(new window.WebSocket('ws://leptos.localhost:3001/live_reload').native, true)
  assert.deepEqual(
    called('ws_open').map((call) => call.payload.url),
    ['ws://leptos.localhost/ws', 'ws://leptos.localhost/a']
  )
})

test('subprotocols are unique tokens, and a fragment is refused', () => {
  const { window } = page()
  assert.throws(() => new window.WebSocket('/ws', ['a b']), { name: 'SyntaxError' })
  assert.throws(() => new window.WebSocket('/ws', ['chat', 'chat']), { name: 'SyntaxError' })
  assert.throws(() => new window.WebSocket('/ws#part'), { name: 'SyntaxError' })
})

test('it opens with its protocol, then delivers text and binary by binaryType', async () => {
  const { window, called, opens, reads } = page()
  const socket = new window.WebSocket('/ws', 'chat')
  const events = []
  socket.onopen = () => events.push(['open', socket.readyState, socket.protocol])
  socket.addEventListener('message', (event) => events.push(['message', event.data, event.origin]))
  const { call, ...open } = called('ws_open')[0].payload
  assert.deepEqual(open, { url: `${MACOS}/ws`, protocols: ['chat'] })
  assert.match(call, /^[0-9a-z]+\.1$/)
  opens[0].resolve({ id: 7, protocol: 'chat', extensions: '' })
  await until(() => reads.length === 1)
  assert.deepEqual(called('ws_read')[0].payload, { id: 7 })

  reads[0].resolve(batch(['text', 'hi'], ['binary', [1, 2]]))
  await until(() => events.length === 3)
  assert.deepEqual(events.slice(0, 2), [
    ['open', 1, 'chat'],
    ['message', 'hi', MACOS]
  ])
  assert.ok(events[2][1] instanceof Blob)
  assert.deepEqual([...new Uint8Array(await events[2][1].arrayBuffer())], [1, 2])

  socket.binaryType = 'arraybuffer'
  socket.binaryType = 'nonsense'
  assert.equal(socket.binaryType, 'arraybuffer')
  await until(() => reads.length === 2)
  reads[1].resolve(batch(['binary', [3]]))
  await until(() => events.length === 4)
  assert.ok(events[3][1] instanceof ArrayBuffer)
  assert.deepEqual([...new Uint8Array(events[3][1])], [3])
})

test('sends wait for the open, copy their data, and go out in order and in batches', async () => {
  const { window, called, opens, sends } = page()
  const socket = new window.WebSocket('/ws')
  assert.throws(() => socket.send('early'), { name: 'InvalidStateError' })
  opens[0].resolve({ id: 7, protocol: '', extensions: '' })
  await until(() => socket.readyState === window.WebSocket.OPEN)

  const view = Uint8Array.from([1, 2])
  socket.send('a')
  socket.send(view)
  view[0] = 9
  socket.send('b')
  assert.equal(socket.bufferedAmount, 4)
  await until(() => sends.length === 1)
  const [first] = called('ws_send')
  assert.deepEqual(decodeSent(first.payload), [['text', 'a']])
  assert.deepEqual(JSON.parse(decodeURIComponent(first.options.headers['leptos-ssr-request'])), { id: 7 })

  sends[0].resolve(null)
  await until(() => sends.length === 2)
  assert.equal(socket.bufferedAmount, 3)
  assert.deepEqual(decodeSent(called('ws_send')[1].payload), [
    ['binary', [1, 2]],
    ['text', 'b']
  ])
  sends[1].resolve(null)
  await until(() => socket.bufferedAmount === 0)
})

test('sends match the vectors of tests/wire.json', async () => {
  const { window, called, opens, sends } = page()
  const socket = await opened(window, opens)
  for (const [n, vector] of wire.sends.entries()) {
    socket.send('text' in vector ? vector.text : Uint8Array.from(vector.binary))
    await until(() => sends.length === n + 1)
    sends[n].resolve(null)
  }
  assert.deepEqual(
    called('ws_send').map(({ payload }) => payload),
    wire.sends.map((vector) => bytes(vector.bytes))
  )
})

test('close checks its arguments, waits for the sends, and ends cleanly', async () => {
  const { window, called, opens, reads, sends } = page()
  const socket = await opened(window, opens)
  assert.throws(() => socket.close(1001), { name: 'InvalidAccessError' })
  assert.throws(() => socket.close(1000, 'x'.repeat(124)), { name: 'SyntaxError' })
  const events = []
  socket.onmessage = (event) => events.push(['message', event.data])
  socket.onclose = (event) => events.push(['close', event.wasClean, event.code, event.reason, socket.readyState])

  socket.send('last')
  socket.close(4000, 'bye')
  assert.equal(socket.readyState, window.WebSocket.CLOSING)
  socket.close()
  socket.send('dropped')
  await until(() => sends.length === 1)
  assert.equal(called('ws_close').length, 0)
  sends[0].resolve(null)
  await until(() => called('ws_close').length === 1)
  assert.deepEqual(called('ws_close')[0].payload, { id: 7, code: 4000, reason: 'bye' })
  assert.equal(called('ws_send').length, 1)

  await until(() => reads.length === 1)
  reads[0].resolve(batch(['text', 'late'], ['close', 4000, 'bye']))
  await until(() => events.length === 1)
  assert.deepEqual(events, [['close', true, 4000, 'bye', window.WebSocket.CLOSED]])
})

test('a reason without a code closes with 1000', async () => {
  const { window, called, opens } = page()
  const socket = await opened(window, opens)
  socket.close(undefined, 'done')
  await until(() => called('ws_close').length === 1)
  assert.deepEqual(called('ws_close')[0].payload, { id: 7, code: 1000, reason: 'done' })
})

test('a failed open fires error, then an unclean close', async () => {
  const { window, opens } = page()
  const socket = new window.WebSocket('/ws')
  const events = []
  socket.onopen = () => events.push(['open'])
  socket.onerror = () => events.push(['error', socket.readyState])
  socket.onclose = (event) => events.push(['close', event.wasClean, event.code])
  opens[0].reject('the websocket failed: the app answered 403 Forbidden')
  await until(() => events.length === 2)
  assert.deepEqual(events, [
    ['error', window.WebSocket.CLOSED],
    ['close', false, 1006]
  ])
})

test('an error record fails an open socket', async () => {
  const { window, opens, reads } = page()
  const socket = await opened(window, opens)
  const events = []
  socket.onerror = () => events.push('error')
  socket.onclose = (event) => events.push(`close ${event.code} ${event.wasClean}`)
  await until(() => reads.length === 1)
  reads[0].resolve(batch(['error', 'the connection closed without a close frame']))
  await until(() => events.length === 2)
  assert.deepEqual(events, ['error', 'close 1006 false'])
})

test('a close while connecting fails the connection once it opens', async () => {
  const { window, called, opens } = page()
  const socket = new window.WebSocket('/ws')
  const events = []
  socket.onopen = () => events.push('open')
  socket.onerror = () => events.push('error')
  socket.onclose = (event) => events.push(`close ${event.code}`)
  socket.close()
  assert.equal(socket.readyState, window.WebSocket.CLOSING)
  opens[0].resolve({ id: 5, protocol: '', extensions: '' })
  await until(() => events.length === 2)
  assert.deepEqual(events, ['error', 'close 1006'])
  assert.deepEqual(called('ws_close')[0].payload, { id: 5, code: null, reason: null })
})

test('an idle read polls again, and number arrays decode like buffers', async () => {
  const { window, opens, reads } = page()
  const socket = await opened(window, opens)
  const messages = []
  socket.onmessage = (event) => messages.push(event.data)
  await until(() => reads.length === 1)
  reads[0].resolve(new ArrayBuffer(0))
  await until(() => reads.length === 2)
  reads[1].resolve([...new Uint8Array(batch(['text', 'over postMessage']))])
  await until(() => messages.length === 1)
  assert.deepEqual(messages, ['over postMessage'])
})
