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

  // A frame from the `fetch` command: a big-endian u32 head length, the JSON
  // head, then the bytes already available.
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
        headers: Array.from(request.headers.entries())
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
      this.dispatchEvent(event)
      const handler = this[`on${event.type}`]
      if (typeof handler === 'function') {
        try {
          handler.call(this, event)
        } catch (error) {
          // Reported like a throwing listener, without stopping the stream.
          queueMicrotask(() => {
            throw error
          })
        }
      }
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
})()
