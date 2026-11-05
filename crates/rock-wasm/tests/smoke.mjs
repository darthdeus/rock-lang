// Smoke test for the rock-wasm module, run from the repository root:
//   node crates/rock-wasm/tests/smoke.mjs <rock_wasm.wasm> <rock>
// Loads the module the way a Worker does (the stdlib is the copy embedded in
// the module), compiles each fixture and requires its Lua to equal native
// `rock compile-single` output byte for byte. Then checks shader
// output, format, and that source nested past the limit is a diagnostic
// rather than a trap.
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';

const [wasmPath, rockBin] = process.argv.slice(2);
if (!wasmPath || !rockBin) {
  console.error('usage: node smoke.mjs <rock_wasm.wasm> <rock>');
  process.exit(2);
}
const fixtureDir = 'crates/rock-wasm/tests/fixtures';
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failures.push(what);
};

const bytes = fs.readFileSync(wasmPath);
let t = performance.now();
const { instance } = await WebAssembly.instantiate(bytes, {});
console.log(`wasm ${bytes.length} bytes, instantiate ${(performance.now() - t).toFixed(1)} ms`);
const ex = instance.exports;
const enc = new TextEncoder();
const dec = new TextDecoder();

function request(body) {
  const input = enc.encode(JSON.stringify(body));
  const ptr = ex.rock_alloc(input.length);
  new Uint8Array(ex.memory.buffer, ptr, input.length).set(input);
  const status = ex.rock_request(ptr, input.length);
  const out = dec.decode(new Uint8Array(ex.memory.buffer, ex.rock_output_ptr(), ex.rock_output_len()));
  const response = JSON.parse(out);
  if ((status === 0) !== (response.ok === true)) throw new Error(`status ${status} disagrees with ${out}`);
  return response;
}

const stdlibResp = request({ op: 'stdlib' });
check(stdlibResp.ok && typeof stdlibResp.files === 'object' && 'unified.rock' in stdlibResp.files,
  'stdlib serves the embedded copy');

for (const name of fs.readdirSync(fixtureDir).filter((n) => n.endsWith('.rock')).sort()) {
  const file = path.join(fixtureDir, name);
  const source = fs.readFileSync(file, 'utf8');
  t = performance.now();
  const wasm = request({ op: 'compile', file, source });
  const ms = (performance.now() - t).toFixed(1);
  if (!wasm.ok) {
    check(false, `${name} compiles in wasm: ${JSON.stringify(wasm.diagnostics ?? wasm)}`);
    continue;
  }
  const native = execFileSync(rockBin, ['compile-single', file], { encoding: 'utf8', maxBuffer: 1 << 28 });
  check(wasm.lua === native, `${name}: wasm Lua equals native (${wasm.lua.length} bytes, ${ms} ms)`);
}

const shader = fs.readFileSync(path.join(fixtureDir, 'shader.rock'), 'utf8');
const gl = request({ op: 'compile', file: 'shader.rock', source: shader });
check(gl.ok && gl.shaders.some((s) => s.language === 'glsl' && s.source.includes('void main')),
  'shader fixture reports its GLSL');

const embed = request({
  op: 'compile', file: 'embed.rock',
  source: 'fn main() { print(embed_string("data/note.txt")) }\n',
  files: { 'data/note.txt': 'note-from-the-host' },
});
check(embed.ok && embed.lua.includes('note-from-the-host'), 'embed_string reads host-supplied files');
const missing = request({ op: 'compile', file: 'embed.rock', source: 'fn main() { print(embed_string("absent.txt")) }\n' });
check(!missing.ok && missing.diagnostics.some((d) => d.message.includes('host supplied no file')),
  'embed_string of an unsupplied file is a diagnostic');

const bad = request({ op: 'compile', file: 'bad.rock', source: 'fn main() -> int { return "no" }\n' });
check(!bad.ok && bad.stage === 'check' && bad.diagnostics.length > 0 && bad.diagnostics[0].line === 0,
  'a type error comes back as a located diagnostic');
const formatted = request({ op: 'format', source: 'fn  main( ) {\nprint( "x" )\n}\n' });
check(formatted.ok && formatted.source === 'fn main() {\n    print("x")\n}\n', 'format');

// Every nesting construct at the limit must compile on the engine's stack
// (wasm calls run on it) and the module's linear-memory stack; past the
// limit it must be a diagnostic, never a trap.
const { nesting_limit: limit } = request({ op: 'info' });
const shapes = {
  'term sum': (n) => `fn main() -> int {\n  return ${Array(n).fill('1').join(' + ')}\n}\n`,
  'nested if': (n) => `fn main() {\n${'if true {\n'.repeat(n)}${'}\n'.repeat(n)}}\n`,
  'nested call': (n) => `fn f(x: int) -> int { return x }\nfn main() -> int {\n  return ${'f('.repeat(n)}1${')'.repeat(n)}\n}\n`,
  'parenthesized': (n) => `fn main() -> int {\n  return ${'('.repeat(n)}1${')'.repeat(n)}\n}\n`,
};
const deep = (source) => {
  try {
    const r = request({ op: 'compile', file: 'deep.rock', source });
    if (r.ok) return 'ok';
    return r.diagnostics.some((d) => d.message.includes('nested too deeply')) ? 'too deep' : 'other error';
  } catch (e) {
    return `trap (${e.message})`;
  }
};
for (const [name, gen] of Object.entries(shapes)) {
  // The deepest instance the parser accepts, found by bisection, compiles.
  let lo = 1, hi = 4 * limit;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (deep(gen(mid)) === 'too deep') hi = mid - 1; else lo = mid;
  }
  check(deep(gen(lo)) === 'ok', `${name}: ${lo} levels (deepest accepted, limit ${limit}) compiles`);
  check(deep(gen(4 * limit)) === 'too deep', `${name}: ${4 * limit} levels is a diagnostic`);
}

if (failures.length) {
  console.error(`${failures.length} failure(s)`);
  process.exit(1);
}
console.log('wasm smoke: all checks passed');
