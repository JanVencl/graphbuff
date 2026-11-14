// Loads graphbuff.wasm and wraps its exported functions.
//
//   const gb = await loadGraphbuff("graphbuff.wasm");
//   const db = gb.open();                       // in memory, with the movies demo graph
//   db.run("MATCH (n) RETURN count(n) AS n");   // {columns, rows, stats} or {error}
//   db.run("RETURN $x", {x: 1});
//   db.close();

export async function loadGraphbuff(url) {
  const imports = {
    env: {
      graphbuff_unix_ms: () => Date.now(),
      graphbuff_monotonic_ms: () => performance.now(),
    },
  };
  let instance;
  try {
    ({ instance } = await WebAssembly.instantiateStreaming(fetch(url), imports));
  } catch {
    // Servers that do not send `application/wasm`.
    const bytes = await (await fetch(url)).arrayBuffer();
    ({ instance } = await WebAssembly.instantiate(bytes, imports));
  }
  const wasm = instance.exports;
  const encoder = new TextEncoder();
  const decoder = new TextDecoder();

  // Copies `text` into the module's memory: [pointer, length].
  function put(text) {
    const bytes = encoder.encode(text);
    const ptr = wasm.gb_alloc(bytes.length);
    new Uint8Array(wasm.memory.buffer, ptr, bytes.length).set(bytes);
    return [ptr, bytes.length];
  }

  return {
    open() {
      let handle = wasm.gb_open();
      if (!handle) throw new Error("graphbuff: could not open the database");
      return {
        // The result as the HTTP API sends it: {columns, rows, stats}, or
        // {error: {code, message, offset?}}. `params`: an object or a JSON string.
        run(query, params) {
          if (!handle) throw new Error("graphbuff: the database is closed");
          const paramsText = params == null ? "" : typeof params === "string" ? params : JSON.stringify(params);
          const [q, qLen] = put(query);
          const [p, pLen] = put(paramsText);
          try {
            const len = wasm.gb_run(handle, q, qLen, p, pLen);
            const out = new Uint8Array(wasm.memory.buffer, wasm.gb_result(handle), len);
            return JSON.parse(decoder.decode(out));
          } finally {
            wasm.gb_free(q, qLen);
            wasm.gb_free(p, pLen);
          }
        },
        close() {
          if (handle) wasm.gb_close(handle);
          handle = 0;
        },
      };
    },
  };
}
