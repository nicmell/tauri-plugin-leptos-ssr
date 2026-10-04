// Runs the EventSource part of `src/fetch.js` against a stubbed page and Tauri
// IPC: `node --test 'tests/*.test.mjs'`.
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { test } from 'node:test'

const source = readFileSync(new URL('../src/fetch.js', import.meta.url), 'utf8')

const MACOS = 'leptos://localhost'
const encoder = new TextEncoder()

function frame(status, headers, id) {
  const head = encoder.encode(JSON.stringify({ status, headers, id }))
  const out = new Uint8Array(4 + head.length)
  new DataView(out.buffer).setUint32(0, head.length)
  out.set(head, 4)
  return out.buffer
}

function chunk(flag, bytes = new Uint8Array()) {
  const out = new Uint8Array(1 + bytes.length)
  out[0] = flag
  out.set(bytes, 1)
  return out.buffer
}

class NativeEventSource {
  constructor(url) {
    this.url = url
    this.native = true
  }
}

const EVENT_STREAM = [['content-type', 'text/event-stream']]

// `streams` lists, per connection, the response head and the chunks its body
// yields: strings or byte arrays as data, then `end`, or `hold` to stay open.
function page(streams) {
  const requests = []
  const cancelled = []
  const bodies = new Map()
  let next = 0
  const window = {
    EventSource: NativeEventSource,
    fetch: async () => new Response('native'),
    __TAURI_INTERNALS__: {
      invoke: async (cmd, payload, options) => {
        if (cmd === 'plugin:leptos-ssr|fetch') {
          const head = JSON.parse(decodeURIComponent(options.headers['leptos-ssr-request']))
          requests.push(head)
          const stream = streams[next++]
          if (!stream) {
            return new Promise(() => {})
          }
          bodies.set(next, [...stream.chunks])
          return frame(stream.status ?? 200, stream.headers ?? EVENT_STREAM, next)
        }
        if (cmd === 'plugin:leptos-ssr|fetch_read_body') {
          const queue = bodies.get(payload.id)
          const item = queue.shift()
          if (item === undefined || item === 'hold') {
            return new Promise(() => {})
          }
          if (item === 'end') {
            return chunk(1)
          }
          return chunk(0, typeof item === 'string' ? encoder.encode(item) : item)
        }
        if (cmd === 'plugin:leptos-ssr|fetch_cancel_body') {
          cancelled.push(payload.id)
          return null
        }
        throw new Error(`unexpected ${cmd}`)
      }
    }
  }
  const run = new Function('window', 'location', source.replace('__LEPTOS_SSR_ORIGIN__', JSON.stringify(MACOS)))
  run(window, new URL(`${MACOS}/page`))
  const header = (request, name) =>
    Object.fromEntries(request.headers.map(([key, value]) => [key.toLowerCase(), value]))[name]
  return { window, requests, cancelled, header }
}

// Records events as [type, data, lastEventId, readyState].
function record(source, types) {
  const events = []
  for (const type of types) {
    source.addEventListener(type, (event) =>
      events.push([type, event.data, event.lastEventId, source.readyState])
    )
  }
  return events
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

test('event streams parse like WHATWG', async () => {
  const cafe = encoder.encode('data: café\n\n')
  const split = cafe.indexOf(0xc3) + 1
  const { window } = page([
    {
      chunks: [
        '﻿data: one\n\n',
        ': a comment\r\nevent: named\r\ndata: two\r\ndata:lines\r',
        '\nid: 7\r\n\r\n',
        cafe.slice(0, split),
        cafe.slice(split),
        'data: no newline at the end',
        'hold'
      ]
    }
  ])
  const source = new window.EventSource('/events')
  const events = record(source, ['message', 'named'])
  await until(() => events.length === 3)
  assert.deepEqual(events, [
    ['message', 'one', '', 1],
    ['named', 'two\nlines', '7', 1],
    ['message', 'café', '7', 1]
  ])
  source.close()
})

test('state changes come before their events, and reconnects resume', async () => {
  const { window, requests, header } = page([
    { chunks: ['retry: 5\nid: 41\ndata: first\n\n', 'end'] },
    { chunks: ['data: second\n\n', 'hold'] }
  ])
  const source = new window.EventSource(`${MACOS}/events`)
  assert.equal(source.readyState, window.EventSource.CONNECTING)
  const events = record(source, ['open', 'message', 'error'])
  await until(() => events.length === 5)
  assert.deepEqual(events, [
    ['open', undefined, undefined, 1],
    ['message', 'first', '41', 1],
    ['error', undefined, undefined, 0],
    ['open', undefined, undefined, 1],
    ['message', 'second', '41', 1]
  ])
  assert.equal(header(requests[0], 'accept'), 'text/event-stream')
  assert.equal(header(requests[0], 'last-event-id'), undefined)
  assert.equal(header(requests[1], 'last-event-id'), '41')
  source.close()
})

test('close stops the stream without an event', async () => {
  const { window, cancelled } = page([{ chunks: ['data: one\n\n', 'hold'] }])
  const source = new window.EventSource('/events')
  const events = record(source, ['message', 'error'])
  await until(() => events.length === 1)
  source.close()
  assert.equal(source.readyState, window.EventSource.CLOSED)
  await until(() => cancelled.length === 1)
  assert.deepEqual(events, [['message', 'one', '', 1]])
})

test('a wrong content type fails without a reconnect', async () => {
  const { window, requests } = page([
    { headers: [['content-type', 'text/plain']], chunks: ['end'] },
    { chunks: ['hold'] }
  ])
  const source = new window.EventSource('/events')
  const events = record(source, ['open', 'error'])
  await until(() => events.length === 1)
  assert.deepEqual(events, [['error', undefined, undefined, 2]])
  await new Promise((resolve) => setTimeout(resolve, 20))
  assert.equal(requests.length, 1)
})

test('handlers run after listeners', async () => {
  const { window } = page([{ chunks: ['data: one\n\n', 'hold'] }])
  const source = new window.EventSource('/events')
  const order = []
  source.onmessage = () => order.push('handler')
  source.addEventListener('message', () => order.push('listener'))
  await until(() => order.length === 2)
  assert.deepEqual(order, ['listener', 'handler'])
  source.close()
})

test('foreign URLs get the native EventSource, and instanceof holds for both', () => {
  const { window } = page([{ chunks: ['hold'] }])
  const foreign = new window.EventSource('https://example.com/events')
  assert.equal(foreign.native, true)
  assert.ok(foreign instanceof window.EventSource)
  const own = new window.EventSource('/events')
  assert.ok(own instanceof window.EventSource)
  assert.equal(own.url, `${MACOS}/events`)
  assert.equal(own.OPEN, 1)
  own.close()
})

test('only GETs that accept an event stream go over IPC', async () => {
  const { window, requests } = page([{ chunks: ['end'] }])
  const streamed = await window.fetch('/events', { headers: { Accept: 'text/event-stream' } })
  assert.equal(requests.length, 1)
  assert.equal(await streamed.text(), '')

  const head = await window.fetch('/events', { method: 'HEAD', headers: { Accept: 'text/event-stream' } })
  const json = await window.fetch('/events', { headers: { Accept: 'application/json' } })
  assert.equal(requests.length, 1)
  assert.equal(await head.text(), 'native')
  assert.equal(await json.text(), 'native')
})
