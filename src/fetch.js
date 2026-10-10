// Sends same-origin requests with a body from the plugin's pages over IPC:
// custom-protocol requests reach the app without a body on Android.
;(function () {
  const origin = new URL(__LEPTOS_SSR_ORIGIN__)
  // URL.origin is "null" for non-special schemes such as leptos:
  const isOwn = (url) =>
    url.protocol === origin.protocol && url.host === origin.host
  if (!isOwn(location)) {
    return
  }

  const nativeFetch = window.fetch
  const invoke = (cmd, payload, options) =>
    window.__TAURI_INTERNALS__.invoke(cmd, payload, options)
  // The ids that src/calls.rs runs once.
  const page = Array.from(crypto.getRandomValues(new Uint32Array(2)), (n) => n.toString(36)).join('')
  let calls = 0
  const nextCall = () => `${page}.${++calls}`
  const nullBodyStatuses = [101, 103, 204, 205, 304]
  const LAST = 1
  const IDLE = 2

  // An ArrayBuffer, or a plain array after Tauri falls back to postMessage.
  const bytesOf = (raw) =>
    Array.isArray(raw) ? Uint8Array.from(raw) : new Uint8Array(raw)

  const cancelBody = (id) =>
    invoke('plugin:leptos-ssr|fetch_cancel_body', { id }).catch(() => {})

  const abortReason = (signal) =>
    signal.reason ?? new DOMException('The operation was aborted.', 'AbortError')

  function methodOf(input, init) {
    if (init && init.method) {
      return init.method.toUpperCase()
    }
    return input instanceof Request ? input.method : 'GET'
  }

  function urlOf(input) {
    return new URL(input instanceof Request ? input.url : String(input), location.href)
  }

  function signalOf(input, init) {
    if (init && init.signal) {
      return init.signal
    }
    return input instanceof Request ? input.signal : undefined
  }

  // Settles with `promise`, or rejects as soon as `signal` aborts.
  function untilAborted(promise, signal) {
    if (!signal) {
      return promise
    }
    return new Promise((resolve, reject) => {
      const onAbort = () => reject(abortReason(signal))
      signal.addEventListener('abort', onAbort, { once: true })
      promise.then(
        (value) => {
          signal.removeEventListener('abort', onAbort)
          resolve(value)
        },
        (error) => {
          signal.removeEventListener('abort', onAbort)
          reject(error)
        }
      )
    })
  }

  // A frame as `frame` in src/commands.rs builds it.
  function decode(raw) {
    const frame = bytesOf(raw)
    const headLength = new DataView(
      frame.buffer,
      frame.byteOffset,
      frame.byteLength
    ).getUint32(0)
    const head = JSON.parse(
      new TextDecoder().decode(frame.subarray(4, 4 + headLength))
    )
    return { ...head, initial: frame.slice(4 + headLength) }
  }

  function bodyStream(id, initial, signal) {
    let closed = false
    let pending = initial.length > 0 ? initial : null
    return new ReadableStream({
      start(controller) {
        if (!signal) {
          return
        }
        signal.addEventListener(
          'abort',
          () => {
            if (!closed) {
              closed = true
              cancelBody(id)
              controller.error(abortReason(signal))
            }
          },
          { once: true }
        )
      },
      async pull(controller) {
        if (pending) {
          controller.enqueue(pending)
          pending = null
          return
        }
        let chunk
        do {
          try {
            chunk = bytesOf(
              await invoke('plugin:leptos-ssr|fetch_read_body', { id })
            )
          } catch (error) {
            if (!closed) {
              closed = true
              controller.error(new TypeError(String(error)))
            }
            return
          }
          if (closed) {
            return
          }
        } while (chunk[0] === IDLE)
        if (chunk.length > 1) {
          controller.enqueue(chunk.subarray(1))
        }
        if (chunk[0] === LAST) {
          closed = true
          controller.close()
        }
      },
      cancel() {
        if (!closed) {
          closed = true
          return cancelBody(id)
        }
      }
    })
  }

  async function viaIpc(input, init) {
    const signal = signalOf(input, init)
    if (signal && signal.aborted) {
      throw abortReason(signal)
    }
    const request = new Request(
      input instanceof Request ? input : urlOf(input).href,
      init
    )
    const body = new Uint8Array(await request.arrayBuffer())
    const head = encodeURIComponent(
      JSON.stringify({
        method: request.method,
        url: request.url,
        headers: Array.from(request.headers.entries()),
        call: nextCall()
      })
    )
    // A plain object: Tauri sends a Headers instance as `{}` over postMessage.
    const sent = invoke('plugin:leptos-ssr|fetch', body, {
      headers: { 'leptos-ssr-request': head }
    })

    let response
    try {
      response = decode(await untilAborted(sent, signal))
    } catch (error) {
      if (signal && signal.aborted) {
        sent.then(
          (late) => {
            const { id } = decode(late)
            if (id !== null) {
              cancelBody(id)
            }
          },
          () => {}
        )
        throw abortReason(signal)
      }
      throw new TypeError(String(error))
    }

    const { status, headers, id, initial } = response
    if (nullBodyStatuses.includes(status)) {
      if (id !== null) {
        cancelBody(id)
      }
      return new Response(null, { status, headers })
    }
    if (id === null) {
      return new Response(initial, { status, headers })
    }
    return new Response(bodyStream(id, initial, signal), { status, headers })
  }

  function acceptsEventStream(input, init) {
    const headers =
      init && init.headers !== undefined
        ? init.headers
        : input instanceof Request
          ? input.headers
          : undefined
    const accept = new Headers(headers).get('accept') || ''
    return accept.toLowerCase().includes('text/event-stream')
  }

  // Requests with a body need IPC; event streams need it to arrive before
  // they end, since the scheme buffers.
  function needsIpc(input, init) {
    const method = methodOf(input, init)
    if (method === 'HEAD') {
      return false
    }
    return method !== 'GET' || acceptsEventStream(input, init)
  }

  window.fetch = function (input, init) {
    if (!isOwn(urlOf(input)) || !needsIpc(input, init)) {
      return nativeFetch.call(window, input, init)
    }
    return viaIpc(input, init)
  }

  // Fires `event` at its listeners, then at its `on*` handler.
  function fire(target, event) {
    target.dispatchEvent(event)
    const handler = target[`on${event.type}`]
    if (typeof handler === 'function') {
      try {
        handler.call(target, event)
      } catch (error) {
        // Reported like a throwing listener, without stopping the stream.
        queueMicrotask(() => {
          throw error
        })
      }
    }
  }

  // Server-sent events from the plugin's origin, read through the `fetch`
  // above (WHATWG EventSource processing model).
  const NativeEventSource = window.EventSource
  const CONNECTING = 0
  const OPEN = 1
  const CLOSED = 2
  const eventOrigin = `${origin.protocol}//${origin.host}`

  class EventSource extends EventTarget {
    static CONNECTING = CONNECTING
    static OPEN = OPEN
    static CLOSED = CLOSED

    static [Symbol.hasInstance](value) {
      return (
        EventSource.prototype.isPrototypeOf(value) ||
        (NativeEventSource !== undefined && value instanceof NativeEventSource)
      )
    }

    #controller = null
    #timer = null
    #lastEventId = ''
    #retry = 3000

    constructor(url, init) {
      const resolved = new URL(String(url), location.href)
      if (!isOwn(resolved) && NativeEventSource !== undefined) {
        return new NativeEventSource(url, init)
      }
      super()
      this.url = resolved.href
      this.withCredentials = Boolean(init && init.withCredentials)
      this.readyState = CONNECTING
      this.onopen = null
      this.onmessage = null
      this.onerror = null
      this.#connect()
    }

    close() {
      this.readyState = CLOSED
      if (this.#timer !== null) {
        clearTimeout(this.#timer)
        this.#timer = null
      }
      if (this.#controller) {
        this.#controller.abort()
      }
    }

    async #connect() {
      const controller = new AbortController()
      this.#controller = controller
      const headers = { Accept: 'text/event-stream', 'Cache-Control': 'no-cache' }
      if (this.#lastEventId) {
        headers['Last-Event-ID'] = this.#lastEventId
      }
      let response
      try {
        response = await window.fetch(this.url, { headers, signal: controller.signal })
      } catch {
        this.#reconnect(controller)
        return
      }
      if (controller.signal.aborted) {
        return
      }
      const type = (response.headers.get('content-type') || '')
        .split(';')[0]
        .trim()
        .toLowerCase()
      if (response.status !== 200 || type !== 'text/event-stream') {
        if (response.body) {
          response.body.cancel().catch(() => {})
        }
        this.readyState = CLOSED
        this.#dispatch(new Event('error'))
        return
      }
      this.readyState = OPEN
      this.#dispatch(new Event('open'))
      try {
        await this.#read(response.body, controller)
      } catch {
        // A failed stream reconnects like an ended one.
      }
      this.#reconnect(controller)
    }

    #reconnect(controller) {
      if (this.readyState === CLOSED || controller.signal.aborted) {
        return
      }
      this.readyState = CONNECTING
      this.#dispatch(new Event('error'))
      if (this.readyState === CLOSED) {
        return
      }
      this.#timer = setTimeout(() => {
        this.#timer = null
        this.#connect()
      }, this.#retry)
    }

    async #read(body, controller) {
      const reader = body.getReader()
      const decoder = new TextDecoder()
      let buffer = ''
      let started = false
      let data = ''
      let type = ''
      let lastEventId = this.#lastEventId

      const line = (text) => {
        if (text === '') {
          this.#lastEventId = lastEventId
          if (data !== '' && !controller.signal.aborted) {
            const event = new MessageEvent(type || 'message', {
              data: data.endsWith('\n') ? data.slice(0, -1) : data,
              origin: eventOrigin,
              lastEventId
            })
            this.#dispatch(event)
          }
          data = ''
          type = ''
          return
        }
        if (text[0] === ':') {
          return
        }
        const colon = text.indexOf(':')
        const field = colon === -1 ? text : text.slice(0, colon)
        let value = colon === -1 ? '' : text.slice(colon + 1)
        if (value[0] === ' ') {
          value = value.slice(1)
        }
        if (field === 'event') {
          type = value
        } else if (field === 'data') {
          data += `${value}\n`
        } else if (field === 'id') {
          if (!value.includes('\0')) {
            lastEventId = value
          }
        } else if (field === 'retry' && /^\d+$/.test(value)) {
          this.#retry = Number(value)
        }
      }

      for (;;) {
        const { value, done } = await reader.read()
        if (done || controller.signal.aborted) {
          return
        }
        let text = decoder.decode(value, { stream: true })
        if (!started && text.length > 0) {
          started = true
          if (text.charCodeAt(0) === 0xfeff) {
            text = text.slice(1)
          }
        }
        buffer += text
        let start = 0
        for (let i = 0; i < buffer.length; i++) {
          const c = buffer[i]
          if (c !== '\n' && c !== '\r') {
            continue
          }
          // A CR at the end may be the first half of a CRLF still in flight.
          if (c === '\r' && i + 1 === buffer.length) {
            break
          }
          line(buffer.slice(start, i))
          if (c === '\r' && buffer[i + 1] === '\n') {
            i++
          }
          start = i + 1
        }
        buffer = buffer.slice(start)
      }
    }

    #dispatch(event) {
      fire(this, event)
    }
  }

  for (const [name, value] of [
    ['CONNECTING', CONNECTING],
    ['OPEN', OPEN],
    ['CLOSED', CLOSED]
  ]) {
    Object.defineProperty(EventSource.prototype, name, { value })
  }

  window.EventSource = EventSource

  // Websockets to the plugin's origin, over the `ws_*` commands (WHATWG
  // WebSocket interface). Both directions carry records as `Record::encode`
  // in src/sockets.rs writes them.
  const NativeWebSocket = window.WebSocket
  const TEXT = 0
  const BINARY = 1
  const CLOSE_RECORD = 2
  const SEND_BATCH = 64 * 1024
  const SOCKET_CLOSING = 2
  const SOCKET_CLOSED = 3
  const encoder = new TextEncoder()
  const token = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/

  // The live-reload socket has a port, so it stays native.
  const isOwnSocket = (url) =>
    isOwn(url) ||
    (['http:', 'https:', 'ws:', 'wss:'].includes(url.protocol) &&
      url.host === 'leptos.localhost')

  function records(raw) {
    const bytes = bytesOf(raw)
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
    const out = []
    for (let offset = 0; offset < bytes.length; ) {
      const kind = bytes[offset]
      const length = view.getUint32(offset + 1)
      const payload = bytes.subarray(offset + 5, offset + 5 + length)
      out.push({ kind, payload, view: new DataView(payload.buffer, payload.byteOffset, payload.byteLength) })
      offset += 5 + length
    }
    return out
  }

  // The bytes of one message, copied at once: a view can point into wasm
  // memory that changes after `send` returns.
  function outgoing(data) {
    if (typeof data === 'string') {
      const bytes = encoder.encode(data)
      return { kind: TEXT, bytes, size: bytes.length }
    }
    if (data instanceof ArrayBuffer) {
      return { kind: BINARY, bytes: new Uint8Array(data.slice(0)), size: data.byteLength }
    }
    if (ArrayBuffer.isView(data)) {
      const bytes = new Uint8Array(data.buffer, data.byteOffset, data.byteLength).slice()
      return { kind: BINARY, bytes, size: bytes.length }
    }
    if (data instanceof Blob) {
      return { kind: BINARY, blob: data, size: data.size }
    }
    return outgoing(String(data))
  }

  class WebSocket extends EventTarget {
    static CONNECTING = CONNECTING
    static OPEN = OPEN
    static CLOSING = SOCKET_CLOSING
    static CLOSED = SOCKET_CLOSED

    static [Symbol.hasInstance](value) {
      return (
        WebSocket.prototype.isPrototypeOf(value) ||
        (NativeWebSocket !== undefined && value instanceof NativeWebSocket)
      )
    }

    #url
    #origin
    #readyState = CONNECTING
    #bufferedAmount = 0
    #protocol = ''
    #binaryType = 'blob'
    #id = null
    #queue = []
    #flushing = false

    constructor(url, protocols) {
      let resolved = null
      try {
        resolved = new URL(String(url), location.href)
      } catch {
        // The native class reports the bad URL.
      }
      if (!resolved || !isOwnSocket(resolved)) {
        return protocols === undefined
          ? new NativeWebSocket(url)
          : new NativeWebSocket(url, protocols)
      }
      super()
      if (resolved.hash) {
        throw new DOMException('A websocket URL has no fragment.', 'SyntaxError')
      }
      const list =
        protocols === undefined
          ? []
          : typeof protocols === 'string'
            ? [protocols]
            : Array.from(protocols, String)
      if (list.some((protocol) => !token.test(protocol)) || new Set(list).size !== list.length) {
        throw new DOMException('The subprotocols are not unique tokens.', 'SyntaxError')
      }
      if (resolved.protocol === 'http:') {
        resolved.protocol = 'ws:'
      } else if (resolved.protocol === 'https:') {
        resolved.protocol = 'wss:'
      }
      this.#url = resolved.href
      this.#origin = `${resolved.protocol}//${resolved.host}`
      this.onopen = null
      this.onmessage = null
      this.onerror = null
      this.onclose = null
      this.#open(list)
    }

    get url() {
      return this.#url
    }

    get readyState() {
      return this.#readyState
    }

    get bufferedAmount() {
      return this.#bufferedAmount
    }

    get protocol() {
      return this.#protocol
    }

    get extensions() {
      return ''
    }

    get binaryType() {
      return this.#binaryType
    }

    set binaryType(value) {
      if (value === 'blob' || value === 'arraybuffer') {
        this.#binaryType = value
      }
    }

    send(data) {
      if (this.#readyState === CONNECTING) {
        throw new DOMException('The websocket is still connecting.', 'InvalidStateError')
      }
      const item = outgoing(data)
      this.#bufferedAmount += item.size
      if (this.#readyState === OPEN) {
        this.#queue.push(item)
        this.#flush()
      }
    }

    close(code, reason) {
      if (code !== undefined) {
        // WebIDL's [Clamp] unsigned short.
        code = Math.min(Math.max(Math.round(Number(code)) || 0, 0), 65535)
        if (code !== 1000 && (code < 3000 || code > 4999)) {
          throw new DOMException(`${code} is not a close code a page sends.`, 'InvalidAccessError')
        }
      }
      if (reason !== undefined && encoder.encode(String(reason)).length > 123) {
        throw new DOMException('A close reason is at most 123 bytes.', 'SyntaxError')
      }
      if (this.#readyState === SOCKET_CLOSING || this.#readyState === SOCKET_CLOSED) {
        return
      }
      const failing = this.#readyState === CONNECTING
      this.#readyState = SOCKET_CLOSING
      if (failing) {
        return
      }
      const text = reason === undefined ? '' : String(reason)
      this.#queue.push({
        close: true,
        code: code === undefined && text !== '' ? 1000 : (code ?? null),
        reason: text === '' ? null : text
      })
      this.#flush()
    }

    async #open(protocols) {
      let opened
      try {
        opened = await invoke('plugin:leptos-ssr|ws_open', { url: this.#url, protocols, call: nextCall() })
      } catch {
        this.#fail()
        return
      }
      this.#id = opened.id
      if (this.#readyState !== CONNECTING) {
        // `close` came while connecting: the connection fails.
        invoke('plugin:leptos-ssr|ws_close', { id: opened.id, code: null, reason: null }).catch(() => {})
        this.#fail()
        this.#read(true)
        return
      }
      this.#protocol = opened.protocol
      this.#readyState = OPEN
      fire(this, new Event('open'))
      this.#read(false)
    }

    // Reads until the last record; `quiet` only drains it.
    async #read(quiet) {
      for (;;) {
        let batch
        try {
          batch = records(await invoke('plugin:leptos-ssr|ws_read', { id: this.#id }))
        } catch {
          if (!quiet) {
            this.#fail()
          }
          return
        }
        for (const { kind, payload, view } of batch) {
          if (quiet) {
            if (kind !== TEXT && kind !== BINARY) {
              return
            }
            continue
          }
          if (kind === TEXT || kind === BINARY) {
            if (this.#readyState === OPEN) {
              const data =
                kind === TEXT
                  ? new TextDecoder().decode(payload)
                  : this.#binaryType === 'arraybuffer'
                    ? payload.slice().buffer
                    : new Blob([payload])
              fire(this, new MessageEvent('message', { data, origin: this.#origin }))
            }
          } else if (kind === CLOSE_RECORD) {
            this.#readyState = SOCKET_CLOSED
            fire(
              this,
              new CloseEvent('close', {
                wasClean: true,
                code: view.getUint16(0),
                reason: new TextDecoder().decode(payload.subarray(2))
              })
            )
            return
          } else {
            this.#fail()
            return
          }
          await null
        }
      }
    }

    #fail() {
      if (this.#readyState === SOCKET_CLOSED) {
        return
      }
      this.#readyState = SOCKET_CLOSED
      fire(this, new Event('error'))
      fire(this, new CloseEvent('close', { wasClean: false, code: 1006, reason: '' }))
    }

    // One `ws_send` or `ws_close` at a time, in order: Tauri does not order
    // concurrent commands.
    async #flush() {
      if (this.#flushing) {
        return
      }
      this.#flushing = true
      try {
        while (this.#queue.length > 0) {
          if (this.#queue[0].close) {
            const { code, reason } = this.#queue.shift()
            await invoke('plugin:leptos-ssr|ws_close', { id: this.#id, code, reason }).catch(() => {})
            continue
          }
          const batch = []
          let length = 0
          let sent = 0
          while (this.#queue.length > 0 && !this.#queue[0].close) {
            const item = this.#queue[0]
            if (item.blob) {
              item.bytes = new Uint8Array(await item.blob.arrayBuffer())
              item.blob = null
            }
            if (batch.length > 0 && length + 5 + item.bytes.length > SEND_BATCH) {
              break
            }
            this.#queue.shift()
            batch.push(item)
            length += 5 + item.bytes.length
            sent += item.size
          }
          const body = new Uint8Array(length)
          const view = new DataView(body.buffer)
          let offset = 0
          for (const { kind, bytes } of batch) {
            body[offset] = kind
            view.setUint32(offset + 1, bytes.length)
            body.set(bytes, offset + 5)
            offset += 5 + bytes.length
          }
          const head = encodeURIComponent(JSON.stringify({ id: this.#id }))
          // A failed send shows up in the reads, which end the socket.
          await invoke('plugin:leptos-ssr|ws_send', body, {
            headers: { 'leptos-ssr-request': head }
          }).catch(() => {})
          this.#bufferedAmount -= sent
        }
      } finally {
        this.#flushing = false
      }
    }
  }

  for (const [name, value] of [
    ['CONNECTING', CONNECTING],
    ['OPEN', OPEN],
    ['CLOSING', SOCKET_CLOSING],
    ['CLOSED', SOCKET_CLOSED]
  ]) {
    Object.defineProperty(WebSocket.prototype, name, { value })
  }

  window.WebSocket = WebSocket
})()
