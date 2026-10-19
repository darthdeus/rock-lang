// The playground page: a Monaco editor wired to rock-lsp and the compiler,
// both running in worker.js. Monaco comes from the CDN loaded in index.html.
import { rockLanguageConfiguration, rockMonarch } from './rock-language.js';

const MAIN_URI = 'file:///playground/main.rock';
const STDLIB_PREFIX = 'file:///stdlib/';
const SAMPLE = `struct Point { x: float, y: float }

fn length(p: Point) -> float {
    return sqrt(p.x * p.x + p.y * p.y)
}

value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
value Vary { position: Vec4, uv: Vec2 }

@shader(vertex)
fn quad_vert(@attr a: QuadVert) -> Vary {
    return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, uv: a.aTexCoord }
}

@shader(fragment)
fn gradient_frag(v: Vary) -> Vec4 {
    return Vec4 { x: v.uv.x, y: v.uv.y, z: 0.5, w: 1.0 }
}

fn main() {
    let p = Point { x: 3.0, y: 4.0 }
    print("length = {length(p)}")
    print("shader {rock_shader(quad_vert, gradient_frag)}")
}
`;

const $ = (id) => document.getElementById(id);
const setStatus = (text) => { $('status').textContent = text; };

// --- Worker calls -----------------------------------------------------------

const worker = new Worker('worker.js');
const pending = new Map();
let nextCall = 1;
let sessionLost = false;

worker.onmessage = ({ data }) => {
  if (data.restarted) {
    sessionLost = true;
    setStatus(`compiler restarted after a crash: ${data.response.panic ?? data.response.error}`);
  }
  pending.get(data.id)(data.response);
  pending.delete(data.id);
};

function call(op, body = {}) {
  return new Promise((resolve) => {
    const id = nextCall++;
    pending.set(id, resolve);
    worker.postMessage({ id, op, ...body });
  });
}

// --- Language server client -------------------------------------------------

let nextLspId = 1;
let documentVersion = 1;

async function lspSend(message) {
  const response = await call('lsp', { message });
  const messages = response.messages ?? [];
  for (const m of messages) if (m.method) onServerMessage(m);
  if (!response.ok) setStatus(`language server: ${response.error ?? response.panic}`);
  return messages;
}

async function lspRequest(method, params) {
  await flushChange();
  const id = nextLspId++;
  const messages = await lspSend({ jsonrpc: '2.0', id, method, params });
  const reply = messages.find((m) => m.id === id && !m.method);
  if (reply?.error) throw new Error(`${method}: ${reply.error.message}`);
  return reply?.result ?? null;
}

const lspNotify = (method, params) => lspSend({ jsonrpc: '2.0', method, params });

async function startSession(text) {
  sessionLost = false;
  await lspSend({ jsonrpc: '2.0', id: nextLspId++, method: 'initialize', params: { capabilities: {} } });
  await lspNotify('initialized', {});
  await lspNotify('textDocument/didOpen',
    { textDocument: { uri: MAIN_URI, languageId: 'rock', version: documentVersion, text } });
}

function onServerMessage(message) {
  if (message.method === 'textDocument/publishDiagnostics') {
    const model = monaco.editor.getModel(monaco.Uri.parse(message.params.uri));
    if (model) monaco.editor.setModelMarkers(model, 'rock', message.params.diagnostics.map(toMarker));
  } else if (message.method === 'window/showMessage') {
    setStatus(message.params.message);
  }
}

// LSP positions are 0-based; Monaco's are 1-based. Both count UTF-16 units.
const toLspPosition = (p) => ({ line: p.lineNumber - 1, character: p.column - 1 });
const toRange = (r) => new monaco.Range(r.start.line + 1, r.start.character + 1, r.end.line + 1, r.end.character + 1);

function toMarker(d) {
  const severity = { 1: 'Error', 2: 'Warning', 3: 'Info', 4: 'Hint' }[d.severity ?? 1];
  const range = toRange(d.range);
  return {
    severity: monaco.MarkerSeverity[severity],
    message: d.message,
    source: d.source,
    code: d.code == null ? undefined : String(d.code),
    startLineNumber: range.startLineNumber,
    startColumn: range.startColumn,
    endLineNumber: range.endLineNumber,
    endColumn: range.endColumn,
  };
}

// --- Editor -----------------------------------------------------------------

let editor;
let output;
let mainModel;
let stdlibSources = {};
let changeTimer = null;
let activeTab = 'lua';

// Send the edit (and recompile) a moment after typing stops; requests flush
// it first so hover and definition see the current text.
function scheduleChange() {
  clearTimeout(changeTimer);
  changeTimer = setTimeout(flushChange, 250);
}

async function flushChange() {
  if (changeTimer === null) return;
  clearTimeout(changeTimer);
  changeTimer = null;
  const text = mainModel.getValue();
  documentVersion += 1;
  const started = performance.now();
  if (sessionLost) {
    await startSession(text);
  } else {
    await lspNotify('textDocument/didChange',
      { textDocument: { uri: MAIN_URI, version: documentVersion }, contentChanges: [{ text }] });
  }
  setStatus(`checked in ${Math.round(performance.now() - started)} ms`);
  compile();
}

function stdlibModel(uri) {
  const existing = monaco.editor.getModel(uri);
  if (existing) return existing;
  const name = uri.toString().slice(STDLIB_PREFIX.length);
  const text = stdlibSources[decodeURIComponent(name)];
  return text === undefined ? null : monaco.editor.createModel(text, 'rock', uri);
}

function showModel(model) {
  editor.setModel(model);
  const isMain = model === mainModel;
  editor.updateOptions({ readOnly: !isMain });
  $('file').value = isMain ? 'main.rock' : model.uri.toString().slice(STDLIB_PREFIX.length);
}

function registerProviders() {
  monaco.languages.register({ id: 'rock', extensions: ['.rock'] });
  monaco.languages.setLanguageConfiguration('rock', rockLanguageConfiguration);
  monaco.languages.setMonarchTokensProvider('rock', rockMonarch);

  monaco.languages.registerHoverProvider('rock', {
    async provideHover(model, position) {
      const hover = await lspRequest('textDocument/hover',
        { textDocument: { uri: model.uri.toString() }, position: toLspPosition(position) });
      if (!hover) return null;
      const parts = Array.isArray(hover.contents) ? hover.contents : [hover.contents];
      return {
        contents: parts.map((c) => ({ value: typeof c === 'string' ? c : c.value })),
        range: hover.range ? toRange(hover.range) : undefined,
      };
    },
  });

  monaco.languages.registerDefinitionProvider('rock', {
    async provideDefinition(model, position) {
      const result = await lspRequest('textDocument/definition',
        { textDocument: { uri: model.uri.toString() }, position: toLspPosition(position) });
      const locations = result == null ? [] : Array.isArray(result) ? result : [result];
      return locations.map((l) => {
        const uri = monaco.Uri.parse(l.uri ?? l.targetUri);
        if (uri.toString().startsWith(STDLIB_PREFIX)) stdlibModel(uri);
        return { uri, range: toRange(l.range ?? l.targetSelectionRange) };
      });
    },
  });

  monaco.languages.registerDocumentFormattingEditProvider('rock', {
    async provideDocumentFormattingEdits(model) {
      const response = await call('format', { source: model.getValue() });
      if (!response.ok) {
        setStatus(`format: ${response.error}`);
        return [];
      }
      return [{ range: model.getFullModelRange(), text: response.source }];
    },
  });

  // Go-to-definition into the stdlib switches the one editor to that file.
  monaco.editor.registerEditorOpener({
    openCodeEditor(_source, resource, selectionOrPosition) {
      const model = monaco.editor.getModel(resource) ?? stdlibModel(resource);
      if (!model) return false;
      showModel(model);
      if (selectionOrPosition && 'startLineNumber' in selectionOrPosition) {
        editor.setSelection(selectionOrPosition);
        editor.revealRangeInCenterIfOutsideViewport(selectionOrPosition);
      } else if (selectionOrPosition) {
        editor.setPosition(selectionOrPosition);
        editor.revealPositionInCenterIfOutsideViewport(selectionOrPosition);
      }
      return true;
    },
  });
}

// --- Generated code ---------------------------------------------------------

const TAB_TARGET = { lua: 'gl', glsl: 'gl', wgsl: 'wgpu' };

async function compile() {
  const response = await call('compile',
    { file: 'main.rock', source: mainModel.getValue(), target: TAB_TARGET[activeTab] });
  let text;
  if (!response.ok && !response.stage) {
    text = `-- ${response.error ?? response.panic}`;
  } else if (!response.ok) {
    const lines = response.diagnostics.map((d) => `-- ${d.file}:${d.line}:${d.column}: ${d.message}`);
    text = [`-- ${response.stage} failed`, ...lines].join('\n');
  } else if (activeTab === 'lua') {
    text = response.lua;
  } else {
    const shaders = response.shaders.filter((s) => s.language === activeTab);
    text = shaders.length === 0
      ? `// no shaders (mark functions with @shader(vertex) / @shader(fragment))`
      : shaders.map((s) => `// ${s.key}\n${s.source}`).join('\n\n');
  }
  monaco.editor.setModelLanguage(output.getModel(), response.ok && activeTab === 'lua' ? 'lua' : 'plaintext');
  output.setValue(text);
}

function selectTab(tab) {
  activeTab = tab;
  for (const button of document.querySelectorAll('[data-tab]')) {
    button.setAttribute('aria-selected', String(button.dataset.tab === tab));
  }
  compile();
}

// --- Startup ----------------------------------------------------------------

async function main() {
  await window.monacoReady;
  stdlibSources = await fetch('dist/stdlib.json').then((r) => r.json());
  registerProviders();

  const dark = matchMedia('(prefers-color-scheme: dark)').matches;
  monaco.editor.setTheme(dark ? 'vs-dark' : 'vs');
  mainModel = monaco.editor.createModel(SAMPLE, 'rock', monaco.Uri.parse(MAIN_URI));
  editor = monaco.editor.create($('editor'), { model: mainModel, automaticLayout: true, minimap: { enabled: false } });
  output = monaco.editor.create($('output'), {
    value: '', language: 'lua', readOnly: true, automaticLayout: true, minimap: { enabled: false },
  });

  const files = $('file');
  for (const name of Object.keys(stdlibSources).sort()) {
    files.append(new Option(`stdlib/${name}`, name));
  }
  files.onchange = () => {
    const model = files.value === 'main.rock'
      ? mainModel
      : stdlibModel(monaco.Uri.parse(STDLIB_PREFIX + files.value));
    if (model) showModel(model);
  };
  $('format').onclick = () => {
    showModel(mainModel);
    editor.getAction('editor.action.formatDocument').run();
  };
  for (const button of document.querySelectorAll('[data-tab]')) {
    button.onclick = () => selectTab(button.dataset.tab);
  }
  mainModel.onDidChangeContent(scheduleChange);

  setStatus('loading the compiler...');
  const started = performance.now();
  await startSession(mainModel.getValue());
  setStatus(`ready in ${Math.round(performance.now() - started)} ms`);
  compile();
}

main().catch((error) => setStatus(`failed to start: ${error}`));
