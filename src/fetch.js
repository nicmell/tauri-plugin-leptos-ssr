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
  const nullBodyStatuses = [101, 103, 204, 205, 304]

  function methodOf(input, init) {
    if (init && init.method) {
      return init.method.toUpperCase()
    }
    return input instanceof Request ? input.method : 'GET'
  }

  function urlOf(input) {
    return new URL(input instanceof Request ? input.url : String(input), location.href)
  }

  async function viaIpc(input, init) {
    const request = new Request(
      input instanceof Request ? input : urlOf(input).href,
      init
    )
    const body = new Uint8Array(await request.arrayBuffer())
    let raw
    try {
      raw = await window.__TAURI_INTERNALS__.invoke('plugin:leptos-ssr|fetch', {
        request: {
          method: request.method,
          url: request.url,
          headers: Array.from(request.headers.entries()),
          body: Array.from(body)
        }
      })
    } catch (error) {
      throw new TypeError(String(error))
    }
    // An ArrayBuffer, or a plain array after Tauri falls back to postMessage.
    const frame = Array.isArray(raw) ? Uint8Array.from(raw) : new Uint8Array(raw)
    const headLength = new DataView(
      frame.buffer,
      frame.byteOffset,
      frame.byteLength
    ).getUint32(0)
    const head = JSON.parse(
      new TextDecoder().decode(frame.subarray(4, 4 + headLength))
    )
    return new Response(
      nullBodyStatuses.includes(head.status) ? null : frame.slice(4 + headLength),
      { status: head.status, headers: head.headers }
    )
  }

  window.fetch = function (input, init) {
    const method = methodOf(input, init)
    if (method === 'GET' || method === 'HEAD' || !isOwn(urlOf(input))) {
      return nativeFetch.call(window, input, init)
    }
    return viaIpc(input, init)
  }
})()
