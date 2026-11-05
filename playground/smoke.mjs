// Headless check of the playground Worker, run after playground/build.sh:
//   node playground/smoke.mjs
// Loads worker.js as a Worker would (its fetches served from playground/),
// then sends the messages main.js sends: stdlib, LSP session, compile per
// output tab, and format.
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';

const dir = path.dirname(new URL(import.meta.url).pathname);
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failures.push(what);
};

const replies = new Map();
const self = {
  postMessage: (data) => replies.get(data.id)(data),
};
const fetchFile = async (url) => {
  if (url.includes('stdlib.json')) throw new Error('worker must not fetch stdlib.json');
  const bytes = fs.readFileSync(path.join(dir, url));
  const type = url.endsWith('.wasm') ? 'application/wasm' : 'application/json';
  return new Response(bytes, { headers: { 'content-type': type } });
};
const context = vm.createContext({
  self, fetch: fetchFile, WebAssembly, TextEncoder, TextDecoder, Promise, JSON, Uint8Array, Error, String,
});
vm.runInContext(fs.readFileSync(path.join(dir, 'worker.js'), 'utf8'), context, { filename: 'worker.js' });

let nextId = 1;
function post(op, body = {}) {
  const id = nextId++;
  return new Promise((resolve) => {
    replies.set(id, resolve);
    self.onmessage({ data: { id, op, ...body } });
  }).then((data) => data.response);
}

const stdlib = await post('stdlib', {});
check(stdlib.ok && typeof stdlib.files === 'object' && 'unified.rock' in stdlib.files,
  'stdlib through the Worker');

const uri = 'file:///playground/main.rock';
const source = 'fn main() {\n    let n: int = "text"\n}\n';
check((await post('lsp', { message: { jsonrpc: '2.0', id: 1, method: 'initialize', params: { capabilities: {} } } })).ok,
  'initialize through the Worker');
const opened = await post('lsp', { message: { jsonrpc: '2.0', method: 'textDocument/didOpen',
  params: { textDocument: { uri, languageId: 'rock', version: 1, text: source } } } });
check(opened.messages.some((m) => m.method === 'textDocument/publishDiagnostics'
  && m.params.diagnostics.some((d) => d.severity === 1)), 'didOpen with a type error publishes an error');

// The page's own sample, so a stdlib change that breaks it fails here.
const mainJs = fs.readFileSync(path.join(dir, 'main.js'), 'utf8');
const sample = mainJs.match(/const SAMPLE = `([^`]*)`/)[1];
const gl = await post('compile', { file: 'main.rock', source: sample, target: 'gl' });
check(gl.ok && gl.lua.length > 0 && gl.shaders.some((s) => s.language === 'glsl'), 'sample compiles to Lua and GLSL');
const formatted = await post('format', { source: 'fn main(){print("hi")}' });
check(formatted.ok && formatted.source.includes('fn main() {'), 'format');

if (failures.length) {
  console.error(`${failures.length} failure(s)`);
  process.exit(1);
}
console.log('playground worker smoke passed');
