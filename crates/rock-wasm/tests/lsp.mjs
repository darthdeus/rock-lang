// Language-server test for the rock-wasm module, run from the repository root:
//   node crates/rock-wasm/tests/lsp.mjs <rock_wasm.wasm>
// Drives rock-lsp through the `lsp` op the way the playground Worker does:
// initialize, didOpen with a type error -> an error diagnostic, then hover and
// go-to-definition on a stdlib call, which must land in a parseable
// file:///stdlib/ URI. Prints each request's latency.
import fs from 'node:fs';

const [wasmPath] = process.argv.slice(2);
if (!wasmPath) {
  console.error('usage: node lsp.mjs <rock_wasm.wasm>');
  process.exit(2);
}
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failures.push(what);
};

const { instance } = await WebAssembly.instantiate(fs.readFileSync(wasmPath), {});
const ex = instance.exports;
const enc = new TextEncoder();
const dec = new TextDecoder();

function request(body) {
  const input = enc.encode(JSON.stringify(body));
  const ptr = ex.rock_alloc(input.length);
  new Uint8Array(ex.memory.buffer, ptr, input.length).set(input);
  ex.rock_request(ptr, input.length);
  return JSON.parse(dec.decode(new Uint8Array(ex.memory.buffer, ex.rock_output_ptr(), ex.rock_output_len())));
}

let nextId = 1;
// One JSON-RPC message in; the server's replies out, with the elapsed time.
function lsp(method, params, { notification = false } = {}) {
  const message = notification ? { jsonrpc: '2.0', method, params } : { jsonrpc: '2.0', id: nextId++, method, params };
  const t = performance.now();
  const response = request({ op: 'lsp', message });
  const ms = performance.now() - t;
  if (!response.ok) throw new Error(`${method}: ${JSON.stringify(response)}`);
  console.log(`     ${method} ${ms.toFixed(1)} ms`);
  const reply = notification ? undefined : response.messages.find((m) => m.id === message.id);
  return { messages: response.messages, reply, ms };
}

const stdlibResp = request({ op: 'stdlib' });
check(stdlibResp.ok && typeof stdlibResp.files === 'object' && 'unified.rock' in stdlibResp.files,
  'stdlib serves the embedded copy');
const stdlib = stdlibResp.files ?? {};

const early = lsp('textDocument/hover', { textDocument: { uri: 'file:///playground/main.rock' }, position: { line: 0, character: 0 } });
check(early.reply?.error?.code === -32002, 'a request before initialize is ServerNotInitialized');

const init = lsp('initialize', { capabilities: {}, rootUri: null });
check(init.reply?.result?.capabilities?.hoverProvider === true, 'initialize advertises hover');
lsp('initialized', {}, { notification: true });

const uri = 'file:///playground/main.rock';
const broken = 'fn main() {\n    let n: int = "text"\n    print("{n}")\n}\n';
const open = lsp('textDocument/didOpen', { textDocument: { uri, languageId: 'rock', version: 1, text: broken } },
  { notification: true });
const published = open.messages.filter((m) => m.method === 'textDocument/publishDiagnostics' && m.params.uri === uri);
const errors = published.flatMap((m) => m.params.diagnostics).filter((d) => d.severity === 1);
check(errors.some((d) => d.range.start.line === 1), `didOpen with a type error publishes it on line 2 (${errors.length} errors)`);

const fixed = 'fn main() {\n    let bad = is_nan(1.5)\n    print("{bad}")\n}\n';
const change = lsp('textDocument/didChange',
  { textDocument: { uri, version: 2 }, contentChanges: [{ text: fixed }] }, { notification: true });
const cleared = change.messages.filter((m) => m.method === 'textDocument/publishDiagnostics' && m.params.uri === uri);
check(cleared.length > 0 && cleared.every((m) => m.params.diagnostics.every((d) => d.severity !== 1)),
  'didChange to valid source clears the error');

const at = { textDocument: { uri }, position: { line: 1, character: 16 } };
const hover = lsp('textDocument/hover', at);
const hoverText = JSON.stringify(hover.reply?.result?.contents ?? null);
check(hoverText.includes('is_nan'), `hover on a stdlib call names it: ${hoverText.slice(0, 120)}`);

const definition = lsp('textDocument/definition', at);
const result = definition.reply?.result;
const location = Array.isArray(result) ? result[0] : result;
const target = location?.uri ?? location?.targetUri;
check(typeof target === 'string' && target.startsWith('file:///stdlib/') && URL.canParse(target),
  `definition lands in the stdlib: ${target}`);
const line = location?.range?.start?.line ?? location?.targetRange?.start?.line;
const stdlibName = target?.slice('file:///stdlib/'.length);
check(stdlib[stdlibName]?.split('\n')[line]?.includes('fn is_nan'), `definition points at the declaration (line ${line + 1})`);

const shutdown = lsp('shutdown', null);
check(shutdown.reply && shutdown.reply.result === null && !shutdown.reply.error, 'shutdown answers null');
lsp('exit', null, { notification: true });
const after = lsp('textDocument/hover', at);
check(after.reply?.error?.code === -32002, 'exit drops the server');

if (failures.length) {
  console.error(`${failures.length} failure(s)`);
  process.exit(1);
}
console.log('all lsp checks passed');
