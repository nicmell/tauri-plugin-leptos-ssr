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

  window.fetch = function (input, init) {
    const method = methodOf(input, init)
    if (method === 'GET' || method === 'HEAD' || !isOwn(urlOf(input))) {
      return nativeFetch.call(window, input, init)
    }
    return viaIpc(input, init)
  }
})()
