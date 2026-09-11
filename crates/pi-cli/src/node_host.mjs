
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import readline from 'node:readline';
import { createRequire } from 'node:module';
import { Module } from 'node:module';
console.log = (...args) => console.error(...args);
const paths = JSON.parse(process.env.RPI_JS_EXTENSION_PATHS || '[]');
const hostContext = JSON.parse(process.env.RPI_JS_EXTENSION_CONTEXT || '{}');
let runtimeModels = Array.isArray(hostContext.models) ? hostContext.models : [];
let currentModel = hostContext.currentModel || null;
let runtimeThinkingLevel = hostContext.thinkingLevel || 'medium';
let runtimeSession = hostContext.session || {};
const tools = new Map();
const commands = new Map();
const resourceHandlers = [];
const customs = new Map();
const customStack = [];
let nextCustomId = 1;
const pendingRuntimeRequests = new Map();
const activeHostRequests = new Map();
let nextRuntimeRequestId = 1;
const hostCapabilities = ['tools', 'commands', 'resources', 'models', 'session', 'ui.notify', 'ui.editor'];
function runtimeRequest(action, args = {}) {
  return new Promise((resolve, reject) => {
    const requestId = nextRuntimeRequestId++;
    pendingRuntimeRequests.set(requestId, { resolve, reject });
    write({ type: 'runtime_request', requestId, action, args });
  });
}
function updateRuntimeContext(next = {}) {
  Object.assign(hostContext, next);
  if (Array.isArray(next.capabilities)) {
    for (const capability of next.capabilities) {
      if (!hostCapabilities.includes(capability)) hostCapabilities.push(capability);
    }
  }
  if (Array.isArray(next.models)) runtimeModels = next.models;
  if (Object.hasOwn(next, 'currentModel')) currentModel = next.currentModel;
  if (next.thinkingLevel) runtimeThinkingLevel = next.thinkingLevel;
  if (next.session) runtimeSession = next.session;
}
function unsupportedCapability(name) {
  const error = new Error(`unsupported capability: ${name}`);
  error.code = 'unsupported_capability';
  return error;
}
function customTheme() {
  const passthrough = (...args) => String(args.length ? args[args.length - 1] ?? '' : '');
  return new Proxy({ fg: passthrough, bg: passthrough, bold: passthrough, inverse: passthrough, underline: passthrough }, {
    get(target, key) { return target[key] || passthrough; },
  });
}
function customKeybindings() {
  const defaults = {
    'tui.select.confirm': ['enter'],
    'tui.select.cancel': ['escape'],
    'tui.select.up': ['up'],
    'tui.select.down': ['down'],
    'tui.select.pageUp': ['pageup'],
    'tui.select.pageDown': ['pagedown'],
    'tui.input.submit': ['enter'],
    'tui.input.newLine': ['shift+enter'],
    'tui.input.tab': ['tab'],
  };
  const raw = key => {
    const value = String(key || '').toLowerCase();
    const named = {
      enter: '\r', return: '\r', escape: '\x1b', esc: '\x1b',
      tab: '\t', backspace: '\x7f', space: ' ',
      up: '\x1b[A', down: '\x1b[B', right: '\x1b[C', left: '\x1b[D',
      home: '\x1b[H', end: '\x1b[F', pageup: '\x1b[5~', pagedown: '\x1b[6~',
      delete: '\x1b[3~', insert: '\x1b[2~',
    };
    if (Object.hasOwn(named, value)) return named[value];
    const ctrl = value.match(/^ctrl[+]([a-z[\\\\\\]_])$/);
    if (ctrl) return String.fromCharCode(ctrl[1].charCodeAt(0) & 0x1f);
    if (value.length === 1) return value;
    return value;
  };
  return {
    getKeys(action) { return defaults[action] || []; },
    matches(data, action) {
      return this.getKeys(action).some(key => {
        const expected = raw(key);
        const actual = String(data ?? '');
        return actual === expected || String(key).toLowerCase() === actual.toLowerCase();
      });
    },
  };
}
function customTerminal(customId) {
  const dimensions = { columns: 120, rows: 40 };
  const send = data => { void runtimeRequest('ui.custom.write', { customId, data: String(data ?? '') }); };
  return {
    info() { return { ...dimensions }; },
    // pi-tui's Terminal contract exposes dimensions as getters. Keep info()
    // as well for extensions that use the older helper API.
    get columns() { return dimensions.columns; },
    get rows() { return dimensions.rows; },
    write: send,
    hideCursor() { send('\x1b[?25l'); },
    showCursor() { send('\x1b[?25h'); },
    moveCursor(row, col) { send(`\x1b[${Number(row) + 1};${Number(col) + 1}H`); },
    moveBy(lines) {
      const amount = Number(lines) || 0;
      if (amount > 0) send(`\x1b[${amount}B`);
      else if (amount < 0) send(`\x1b[${-amount}A`);
    },
    clearScreen() { send('\x1b[2J\x1b[H'); },
    setTitle(title) { void runtimeRequest('ui.custom.write', { customId, data: `\x1b]0;${String(title)}\x07` }); },
    enableMouse() { send('\x1b[?1000h\x1b[?1006h'); },
    disableMouse() { send('\x1b[?1000l\x1b[?1006l'); },
    enterRawMode() {},
    refreshSize() {},
    start() {},
    stop() {},
    isTty() { return true; },
    setProgress() {},
    flush() {},
    _setSize(width, height) { dimensions.columns = Number(width) || 120; dimensions.rows = Number(height) || 40; },
  };
}
function customParent(customId) {
  const terminal = customTerminal(customId);
  return {
    mode: 'tui',
    terminal,
    altScreen: false,
    getShowHardwareCursor() { return false; },
    requestRender() { void runtimeRequest('ui.custom.invalidate', { customId }); },
    renderNow() { void runtimeRequest('ui.custom.invalidate', { customId }); },
    start() {},
    stop() {},
    addChild() {},
    removeChild() {},
    clear() {},
    setFocus() {},
    getFocus() { return null; },
    showOverlay() { return { hide() {}, setHidden() {}, isHidden() { return false; }, focus() {}, isFocused() { return true; } }; },
    hideOverlay() {},
    hasOverlay() { return false; },
  };
}
async function invokeCustomInput(customId, data) {
  const custom = customs.get(customId);
  if (!custom?.component) return;
  try {
    const input = String(data ?? '');
    // Regular Pi components expose handleInput(). Some extensions (notably
    // pi-btw) return a host component that owns a nested TUI instead; route
    // input through that TUI's public terminal dispatcher when available.
    const component = custom.component;
    const handler = typeof component.handleInput === 'function'
      ? component.handleInput
      : typeof component.handleTerminalInput === 'function'
        ? component.handleTerminalInput
        : typeof component.fullscreen?.handleTerminalInput === 'function'
          ? component.fullscreen.handleTerminalInput
          : undefined;
    if (typeof handler === 'function') handler.call(component.fullscreen ? component.fullscreen : component, input);
  } catch (error) {
    console.error(`JS custom component input failed: ${error?.stack || error}`);
  }
}
function closeCustoms() {
  for (const customId of customs.keys()) void runtimeRequest('ui.custom.close', { customId, restoreCustomId: null });
  customs.clear();
  customStack.length = 0;
}
function createEventStream(runtimePromise) {
  let eventsPromise;
  let index = 0;
  const loadEvents = () => {
    if (!eventsPromise) eventsPromise = runtimePromise.then(value => Array.isArray(value?.events) ? value.events : []);
    return eventsPromise;
  };
  const stream = {
    async next() {
      const events = await loadEvents();
      if (index >= events.length) return { done: true, value: undefined };
      return { done: false, value: events[index++] };
    },
    [Symbol.asyncIterator]() { return this; },
    result() { return runtimePromise.then(value => value?.result ?? value); },
  };
  return stream;
}
function createModelRegistry() {
  const provider = (providerId) => ({
    id: providerId,
    streamSimple(model, context, options = {}) {
      return createEventStream(runtimeRequest('provider.stream', { model, context, options }));
    },
    complete(model, context, options = {}) {
      return runtimeRequest('provider.complete', { model, context, options });
    },
  });
  return {
    getAll() { return runtimeModels; },
    getAvailable() { return runtimeModels; },
    find(provider, id) {
      return runtimeModels.find(model => model?.provider === provider && model?.id === id);
    },
    getError() { return undefined; },
    hasConfiguredAuth(model) {
      return Boolean(model?.headers || process.env.ANTHROPIC_API_KEY || process.env.OPENAI_API_KEY || process.env.ANTHROPIC_AUTH_TOKEN);
    },
    getProvider(providerId) {
      return runtimeModels.some(model => model?.provider === providerId) ? provider(providerId) : null;
    },
    async getApiKeyAndHeaders(model) {
      const providerId = String(model?.provider || '').toLowerCase();
      const openaiLike = providerId.includes('openai') || String(model?.api || '').includes('openai');
      const apiKey = openaiLike
        ? (process.env.OPENAI_API_KEY || '')
        : (process.env.ANTHROPIC_API_KEY || process.env.OPENAI_API_KEY || '');
      const authToken = providerId.includes('anthropic') ? (process.env.ANTHROPIC_AUTH_TOKEN || '') : '';
      const headers = authToken ? { authorization: `Bearer ${authToken}` } : {};
      if (!apiKey && !authToken && !model?.headers) {
        return { ok: false, error: 'No request credentials available' };
      }
      return {
        ok: true,
        apiKey: apiKey || undefined,
        headers: { ...(model?.headers || {}), ...headers },
        baseUrl: model?.baseUrl,
      };
    },
  };
}
function commandContext(initial = {}, signal = new AbortController().signal) {
  const notifications = [];
  let editorText = String(initial.editorText ?? '');
  const ui = {
    notify(message, level = 'info') {
      notifications.push({ message: String(message), level: String(level) });
    },
    async select() { throw unsupportedCapability('ui.select'); },
    async confirm() { throw unsupportedCapability('ui.confirm'); },
    async input() { throw unsupportedCapability('ui.input'); },
    async editor() { throw unsupportedCapability('ui.editor_dialog'); },
    onTerminalInput() { return () => {}; },
    setStatus() {},
    setWorkingMessage() {},
    setWorkingVisible() {},
    setWorkingIndicator() {},
    setHiddenThinkingLabel() {},
    setWidget() {},
    setFooter() {},
    setHeader() {},
    setTitle() {},
    getEditorText() { return editorText; },
    setEditorText(value) { editorText = String(value ?? ''); },
    pasteToEditor(value) { editorText += String(value ?? ''); },
    addAutocompleteProvider() {},
    setEditorComponent() {},
    async custom(factory, options = {}) {
      if (typeof factory !== 'function') throw new TypeError('ui.custom requires a factory');
      const customId = `custom-${nextCustomId++}`;
      let settled = false;
      let resolveResult;
      let rejectResult;
      const result = new Promise((resolve, reject) => { resolveResult = resolve; rejectResult = reject; });
      const done = value => {
        if (settled) return;
        settled = true;
        customs.delete(customId);
        const index = customStack.lastIndexOf(customId);
        if (index >= 0) customStack.splice(index, 1);
        void runtimeRequest('ui.custom.close', { customId, restoreCustomId: customStack.at(-1) || null }).finally(() => resolveResult(value));
      };
      customs.set(customId, { component: null, done });
      try {
        customStack.push(customId);
        await runtimeRequest('ui.custom.open', { customId, options: { overlay: Boolean(options?.overlay) } });
        const parent = customParent(customId);
        const component = await factory(parent, customTheme(), customKeybindings(), done);
        const state = customs.get(customId);
        if (state) {
          state.terminal = parent.terminal;
          state.component = component;
        }
        // Components that own a nested TUI (for example pi-btw's
        // BtwFullscreenHost) render directly through the terminal proxy. Their
        // lightweight placeholder render must not race and overwrite the
        // nested screen after it starts.
        const ownsTerminal = /FullscreenHost|TuiAltScreen/.test(String(component?.constructor?.name || ''));
        if (component && typeof component.render === 'function' && !ownsTerminal) {
          const frame = component.render(120);
          if (Array.isArray(frame) && frame.length) {
            void runtimeRequest('ui.custom.write', { customId, data: frame.join('\r\n') + '\r\n' });
          }
        }
      } catch (error) {
        settled = true;
        customs.delete(customId);
        const index = customStack.lastIndexOf(customId);
        if (index >= 0) customStack.splice(index, 1);
        void runtimeRequest('ui.custom.close', { customId, restoreCustomId: customStack.at(-1) || null });
        rejectResult(error);
      }
      return result;
    },
  };
  return {
    context: {
      mode: 'tui',
      hasUI: true,
      capabilities: new Set(hostCapabilities),
      cwd: String(hostContext.cwd || process.cwd()),
      ui,
      signal,
      model: currentModel,
      modelRegistry: createModelRegistry(),
      scopedModels: Array.isArray(hostContext.scopedModels) ? hostContext.scopedModels : runtimeModels,
      thinkingLevel: runtimeThinkingLevel,
      isIdle() { return true; },
      isProjectTrusted() { return true; },
      abort() {},
      hasPendingMessages() { return false; },
      shutdown() {},
      getContextUsage() { return undefined; },
      compact() {},
      getSystemPrompt() { return String(hostContext.systemPrompt || ''); },
      sessionManager: {
        getBranch() { return runtimeSession.branch || []; },
        getTree() { return runtimeSession.tree || []; },
        getLeafId() { return runtimeSession.leafId || null; },
        getSessionId() { return runtimeSession.id || 'rpi'; },
        getEntry(id) {
          return (runtimeSession.entries || []).find(entry => entry?.id === id) || null;
        },
      },
    },
    notifications,
    pi: {
      getThinkingLevel() { return runtimeThinkingLevel; },
      setThinkingLevel() {},
      setLabel() {},
      sendMessage: async () => { throw unsupportedCapability('session.sendMessage'); },
      sendUserMessage: async () => { throw unsupportedCapability('session.sendUserMessage'); },
    },
  };
}
function files(value) {
  const out = [];
  const visit = p => {
    if (fs.existsSync(p) && fs.statSync(p).isDirectory()) {
      for (const name of fs.readdirSync(p).sort()) visit(path.join(p, name));
    } else if (/[.](m?js|cjs|ts|tsx)$/.test(p)) out.push(p);
  };
  for (const p of value) visit(p);
  return out;
}
let jiti;
let piCodingAgent;
try {
  const roots = paths.map(p => fs.existsSync(p) && fs.statSync(p).isDirectory() ? p : path.dirname(p));
  const moduleRoots = [];
  const collect = (root, depth) => {
    if (depth < 0 || !fs.existsSync(root)) return;
    moduleRoots.push(root);
    for (const name of fs.readdirSync(root)) {
      const nested = path.join(root, name);
      if (name !== '.bin' && fs.existsSync(nested) && fs.statSync(nested).isDirectory()) {
        collect(nested, depth - 1);
      }
    }
  };
    for (const root of roots) {
        let current = root;
    for (let i = 0; i < 4; i++) { collect(path.join(current, 'node_modules'), 5); const parent = path.dirname(current); if (parent === current) break; current = parent; }
  }
  try {
    const npm = process.platform === 'win32' ? 'npm.cmd' : 'npm';
    const globalRoot = (await import('node:child_process')).execFileSync(npm, ['root', '-g'], { encoding: 'utf8' }).trim();
    collect(globalRoot, 3);
  } catch {}
  if (moduleRoots.length) {
    process.env.NODE_PATH = [...new Set([process.env.NODE_PATH || '', ...moduleRoots].filter(Boolean))].join(path.delimiter);
    // Refresh CommonJS lookup paths after setting NODE_PATH so jiti can see
    // peer packages installed below the host runtime's nested node_modules.
    Module._initPaths();
  }
  const aliases = {};
  for (const root of moduleRoots) {
    try {
      const manifest = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));
      if (manifest.name) aliases[manifest.name] = root;
    } catch {}
  }
  for (const root of moduleRoots) {
    try {
      const req = createRequire(pathToFileURL(path.join(root, 'rpi-extension-host.js')));
      if (!piCodingAgent) {
        try {
          const entry = req.resolve('@earendil-works/pi-coding-agent');
          piCodingAgent = await import(pathToFileURL(entry).href);
        } catch {}
      }
      const mod = req('jiti');
      const create = mod.createJiti || mod.default;
      jiti = create(pathToFileURL(path.join(root, 'index.js')).href, { alias: aliases });
      break;
    } catch {}
  }
  if (!jiti) { const mod = await import('jiti'); jiti = mod.createJiti ? mod.createJiti(import.meta.url) : mod.default(import.meta.url); }
} catch {}
if (!piCodingAgent && jiti) {
  try { piCodingAgent = await jiti.import('@earendil-works/pi-coding-agent'); } catch {}
}
try {
  if (piCodingAgent?.initTheme) {
    try { piCodingAgent.initTheme(hostContext.theme || 'dark', false); }
    catch { piCodingAgent.initTheme('dark', false); }
  }
} catch (error) {
  console.error(`Pi theme initialization failed: ${error?.stack || error}`);
}
async function loadFile(file) {
  if (/[.]tsx?$/.test(file)) {
    if (!jiti) {
      // Recent Node releases can type-strip simple .ts modules natively. Use
      // that path before requiring jiti, while retaining jiti for older Node.
      try { return await import(pathToFileURL(file).href + `?rpi=${Date.now()}`); }
      catch { throw new Error(`TypeScript extension requires Node type-stripping or the jiti dependency: ${file}`); }
    }
    return await jiti.import(file);
  }
  return await import(pathToFileURL(file).href + `?rpi=${Date.now()}`);
}
const api = {
  registerTool(def) { if (!def?.name || typeof def.execute !== 'function') throw new Error('registerTool requires name and execute'); tools.set(def.name, def); },
  registerCommand(name, def) { commands.set(name, typeof def === 'function' ? { handler: def } : { ...def, name }); },
  getThinkingLevel() { return runtimeThinkingLevel; },
  setLabel() {},
  runtimeRequest,
  on(event, handler) { if (event === 'resources_discover') resourceHandlers.push(handler); },
};
for (const file of files(paths)) {
  const mod = await loadFile(file);
  const factory = mod.default || mod;
  if (typeof factory === 'function') await factory(api);
}
const resources = { skillPaths: [], promptPaths: [], themePaths: [] };
for (const handler of resourceHandlers) { const result = await handler(); if (result) for (const key of Object.keys(resources)) if (Array.isArray(result[key])) resources[key].push(...result[key]); }
// Runtime requests can still be queued by a custom component while the host
// is shutting down. Rust closes the pipe first, so ignore the resulting EPIPE
// instead of turning a normal Ctrl+C exit into an uncaught Node exception.
process.stdout.on('error', () => {});
const write = value => process.stdout.write(JSON.stringify(value) + '\n');
const toolSummaries = [...tools.values()].map(t => ({
  name: t.name,
  label: t.label || t.name,
  description: t.description || '',
  parameters: t.parameters || { type: 'object', properties: {} }
}));
write({ id: 0, ok: true, result: {
  apiVersion: 1,
  capabilities: hostCapabilities,
  tools: toolSummaries,
  commands: [...commands.keys()],
  resources
} });
const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
async function handleLine(line) {
  if (!line.trim()) return;
  let request; try { request = JSON.parse(line); } catch { return; }
  if (request.type === 'host_event') {
    if (request.event === 'cancel_request') {
      activeHostRequests.get(request.id)?.abort();
      return;
    }
    if (request.event === 'custom_input') void invokeCustomInput(request.customId, request.data);
    if (request.event === 'custom_resize') {
      const custom = customs.get(request.customId);
      custom?.terminal?._setSize(request.width, request.height);
      custom?.component?.handleResize?.(Number(request.width), Number(request.height));
    }
    return;
  }
  if (request.type === 'runtime_response') {
    const pending = pendingRuntimeRequests.get(request.requestId);
    if (!pending) return;
    pendingRuntimeRequests.delete(request.requestId);
    if (request.ok) pending.resolve(request.result);
    else pending.reject(new Error(request.error || 'runtime request failed'));
    return;
  }
  const controller = new AbortController();
  activeHostRequests.set(request.id, controller);
  try {
    let result;
    if (request.method === 'invoke_tool') {
      const tool = tools.get(request.tool); if (!tool) throw new Error(`unknown JS tool: ${request.tool}`);
      result = await tool.execute(request.toolCallId || 'rpi', request.args || {}, controller.signal, () => {} , {});
    } else if (request.method === 'set_runtime_context') {
      updateRuntimeContext(request.context || {});
      result = true;
    } else if (request.method === 'invoke_command') {
      const command = commands.get(request.command); if (!command) throw new Error(`unknown JS command: ${request.command}`);
      const runtime = commandContext(request.context || {}, controller.signal);
      result = await command.handler(request.args || '', runtime.context);
      result = {
        result: result ?? null,
        notifications: runtime.notifications,
        editorText: runtime.context.ui.getEditorText(),
      };
    } else throw new Error(`unknown method: ${request.method}`);
    write({ id: request.id, ok: true, result: result ?? null });
  } catch (error) {
    if (request.method === 'invoke_command') closeCustoms();
    write({ id: request.id, ok: false, error: String(error?.stack || error) });
  } finally {
    activeHostRequests.delete(request.id);
  }
}
rl.on('line', line => { void handleLine(line); });
await new Promise(() => {});
