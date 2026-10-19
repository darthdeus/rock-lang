// The playground's compiler Worker: loads dist/rock_wasm.wasm (the stdlib is
// the copy embedded in the module) and answers one request at a time.
//
// The page posts {id, op, ...} with the rock-wasm ops (compile, format, lsp,
// stdlib; see crates/rock-wasm/src/lib.rs) and gets {id, response} back. A
// compiler panic traps the module; the Worker then starts a fresh instance and
// answers {id, response, restarted: true} so the page re-sends its LSP session.

let wasmModule = null;
let exports = null;
const encoder = new TextEncoder();
const decoder = new TextDecoder();

async function start() {
  wasmModule = await WebAssembly.compileStreaming(fetch('dist/rock_wasm.wasm'));
  instantiate();
}

function instantiate() {
  exports = new WebAssembly.Instance(wasmModule, {}).exports;
}

function call(body) {
  const input = encoder.encode(JSON.stringify(body));
  const ptr = exports.rock_alloc(input.length);
  new Uint8Array(exports.memory.buffer, ptr, input.length).set(input);
  exports.rock_request(ptr, input.length);
  return output();
}

function output() {
  return JSON.parse(decoder.decode(
    new Uint8Array(exports.memory.buffer, exports.rock_output_ptr(), exports.rock_output_len())));
}

const ready = start();

self.onmessage = async (event) => {
  const { id, ...body } = event.data;
  try {
    await ready;
  } catch (error) {
    self.postMessage({ id, response: { ok: false, error: `could not load the compiler: ${error}` } });
    return;
  }
  try {
    self.postMessage({ id, response: call(body) });
  } catch (trap) {
    // The panic hook leaves {"ok":false,"panic":...} in the output before the trap.
    let response;
    try {
      response = output();
    } catch {
      response = { ok: false, error: String(trap) };
    }
    instantiate();
    self.postMessage({ id, response, restarted: true });
  }
};
