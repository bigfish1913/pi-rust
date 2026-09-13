
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import readline from 'node:readline';
import { createRequire } from 'node:module';
import { Module } from 'node:module';
console.log = (...args) => console.error(...args);
// Runtime requests can be emitted while an extension factory is loading, so
// initialize the JSON-lines writer before any registration or lifecycle hook.
process.stdout.on('error', () => {});
const write = value => process.stdout.write(JSON.stringify(value) + '\n');
const paths = JSON.parse(process.env.RPI_JS_EXTENSION_PATHS || '[]');
const hostContext = JSON.parse(process.env.RPI_JS_EXTENSION_CONTEXT || '{}');
// The one-shot discovery host has already run `before_agent_start` to obtain
// the initial active-tool projection. The persistent host receives that
// projection in `hostContext.activeTools` and skips the duplicate startup
// lifecycle pass; subsequent `set_runtime_context` requests still run the
// hook when the real runtime context changes.
const skipInitialBeforeAgentStart = process.env.RPI_JS_EXTENSION_SKIP_INITIAL_HOOK === '1';
// The one-shot discovery host already collected package resources. The
// persistent host must retain those results without invoking handlers again,
// since resource discovery can have registration side effects.
const skipInitialDiscovery = process.env.RPI_JS_EXTENSION_SKIP_INITIAL_DISCOVERY === '1';
let runtimeModels = Array.isArray(hostContext.models) ? hostContext.models : [];
let currentModel = hostContext.currentModel || null;
let runtimeThinkingLevel = hostContext.thinkingLevel || 'medium';
let runtimeSession = hostContext.session || {};
const tools = new Map();
const commands = new Map();
const shortcuts = new Map();
const flags = new Map();
const messageRenderers = new Map();
const entryRenderers = new Map();
const extensionEvents = new Map();
const resourceHandlers = [];
let activeToolNames = [];
const customs = new Map();
const customStack = [];
// Terminal input listeners and components are allowed to be async. Keep the
// dispatch order for each custom instance even when Rust forwards visible
// overlay keys without waiting for an acknowledgement.
const customInputQueues = new Map();
const terminalInputListeners = new Map();
let nextCustomId = 1;
let nextTerminalInputListenerId = 1;
const pendingRuntimeRequests = new Map();
const activeHostRequests = new Map();
let nextRuntimeRequestId = 1;
let nextUiDialogId = 1;
// Context updates carry the active-tool projection and run user lifecycle
// hooks. Queue only this stateful operation so an older, slower hook cannot
// finish after a newer update and restore stale tools.
let runtimeContextQueue = Promise.resolve();
let runtimeContextRevision = -1;
// The Rust side may provide capabilities before the lazy host starts (for
// example `ui.custom` in interactive mode). Seed the host's capability set
// from that initial context so the first lifecycle/tool callback observes the
// same view as later `set_runtime_context` updates.
const hostCapabilities = [...new Set([
  'tools', 'commands', 'resources', 'models', 'session', 'ui.notify', 'ui.editor',
  ...(Array.isArray(hostContext.capabilities) ? hostContext.capabilities : []),
].map(value => String(value)))];

// Start consuming stdin before extension factories and lifecycle hooks run.
// Those hooks may await `runtimeRequest`, so waiting until after the init
// envelope would leave their promise unresolved and prevent the host from
// ever publishing initialization. Function declarations are hoisted, while
// the first line event can only arrive after this module yields to the event
// loop.
const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
rl.on('line', line => { void handleLine(line); });
rl.on('close', () => process.exit(0));

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
      const name = String(capability);
      if (!hostCapabilities.includes(name)) hostCapabilities.push(name);
    }
  }
  if (Array.isArray(next.models)) runtimeModels = next.models;
  if (Object.hasOwn(next, 'currentModel')) currentModel = next.currentModel;
  if (next.thinkingLevel) runtimeThinkingLevel = next.thinkingLevel;
  if (next.session) runtimeSession = next.session;
  if (Array.isArray(next.activeTools)) {
    activeToolNames = [...new Set(next.activeTools.map(name => String(name)))];
  }
}
// Apply the environment-provided context before extension factories and the
// discovery lifecycle run. This keeps the first callback consistent with the
// later `set_runtime_context` path (including active tools and session state).
updateRuntimeContext(hostContext);
function unsupportedCapability(name) {
  const error = new Error(`unsupported capability: ${name}`);
  error.code = 'unsupported_capability';
  return error;
}

function addExtensionEvent(channel, handler) {
  if (typeof handler !== 'function') return () => {};
  const handlers = extensionEvents.get(String(channel)) || new Set();
  handlers.add(handler);
  extensionEvents.set(String(channel), handlers);
  return () => handlers.delete(handler);
}

async function emitExtensionEvent(channel, event, context) {
  let result;
  for (const handler of [...(extensionEvents.get(String(channel)) || [])]) {
    try {
      const value = await handler(event, context);
      if (value !== undefined) result = value;
    } catch (error) {
      // A single optional extension must not prevent the agent turn from
      // starting. Native Pi reports the handler error and continues.
      console.error(`JS ${channel} handler failed: ${error?.stack || error}`);
    }
  }
  return result;
}

async function invokeTerminalInput(data) {
  let current = String(data ?? '');
  for (const listener of [...terminalInputListeners.values()]) {
    try {
      const result = await listener(current);
      if (result?.consume === true) {
        return { consume: true, data: current };
      }
      if (result?.data !== undefined) current = String(result.data);
    } catch (error) {
      console.error(`JS terminal input listener failed: ${error?.stack || error}`);
    }
  }
  return { consume: false, data: current };
}

function serializeOverlayOptions(options) {
  const source = options?.overlayOptions;
  if (!source || typeof source !== 'object') return undefined;
  const result = {};
  for (const key of [
    'width', 'minWidth', 'maxHeight', 'anchor', 'offsetX', 'offsetY',
    'row', 'col', 'margin', 'nonCapturing',
  ]) {
    const value = source[key];
    if (typeof value === 'number' || typeof value === 'string' ||
        (key === 'margin' && value && typeof value === 'object')) {
      result[key] = value;
    }
  }
  return result;
}

// Dialogs are serviced by the Rust TUI through the bidirectional runtime
// channel. Keep a dialog id separate from the transport request id: a timeout
// or AbortSignal may send a cancellation while the original request is still
// waiting for a key press.
function requestUiDialog(method, fields, signal, defaultValue) {
  if (signal?.aborted) return Promise.resolve(defaultValue);
  const dialogId = `dialog-${nextUiDialogId++}`;
  let settled = false;
  let timeoutId;
  let abortHandler;

  const cancel = () => {
    if (settled) return;
    settled = true;
    if (timeoutId) clearTimeout(timeoutId);
    if (abortHandler) signal?.removeEventListener('abort', abortHandler);
    // The host may already be shutting down. Cancellation is best effort, but
    // attach a rejection handler so a closed pipe never becomes unhandled.
    void runtimeRequest('ui.dialog.cancel', { dialogId }).catch(() => {});
  };

  return new Promise((resolve, reject) => {
    const finish = value => {
      if (settled) return;
      settled = true;
      if (timeoutId) clearTimeout(timeoutId);
      if (abortHandler) signal?.removeEventListener('abort', abortHandler);
      resolve(value);
    };
    const fail = error => {
      if (settled) return;
      settled = true;
      if (timeoutId) clearTimeout(timeoutId);
      if (abortHandler) signal?.removeEventListener('abort', abortHandler);
      reject(error);
    };

    abortHandler = () => {
      cancel();
      resolve(defaultValue);
    };
    signal?.addEventListener('abort', abortHandler, { once: true });
    const timeout = Number(fields?.timeout);
    if (Number.isFinite(timeout) && timeout > 0) {
      timeoutId = setTimeout(() => {
        cancel();
        resolve(defaultValue);
      }, timeout);
    }

    runtimeRequest('ui.dialog', {
      dialogId,
      method,
      ...fields,
    }).then(result => {
      if (settled) return;
      if (result?.cancelled) {
        finish(defaultValue);
      } else if (method === 'confirm') {
        const confirmed = typeof result?.confirmed === 'boolean'
          ? result.confirmed
          : typeof result?.value === 'boolean'
            ? result.value
            : typeof result === 'boolean'
              ? result
              : false;
        finish(confirmed);
      } else if (result && Object.hasOwn(result, 'value')) {
        finish(result.value == null ? defaultValue : String(result.value));
      } else {
        finish(result == null ? defaultValue : String(result));
      }
    }).catch(fail);
  });
}

function customTheme() {
  const passthrough = (...args) => String(args.length ? args[args.length - 1] ?? '' : '');
  return new Proxy({ fg: passthrough, bg: passthrough, bold: passthrough, inverse: passthrough, underline: passthrough }, {
    get(target, key) { return target[key] || passthrough; },
  });
}
function customKeybindings() {
  const defaults = {
    'tui.editor.cursorUp': ['up'],
    'tui.editor.cursorDown': ['down'],
    'tui.editor.cursorLeft': ['left', 'ctrl+b'],
    'tui.editor.cursorRight': ['right', 'ctrl+f'],
    'tui.editor.cursorWordLeft': ['alt+left', 'ctrl+left', 'alt+b'],
    'tui.editor.cursorWordRight': ['alt+right', 'ctrl+right', 'alt+f'],
    'tui.editor.cursorLineStart': ['home', 'ctrl+a'],
    'tui.editor.cursorLineEnd': ['end', 'ctrl+e'],
    'tui.editor.pageUp': ['pageUp'],
    'tui.editor.pageDown': ['pageDown'],
    'tui.editor.deleteCharBackward': ['backspace'],
    'tui.editor.deleteCharForward': ['delete', 'ctrl+d'],
    'tui.editor.deleteWordBackward': ['ctrl+w', 'alt+backspace'],
    'tui.editor.deleteWordForward': ['alt+d', 'alt+delete'],
    'tui.editor.deleteToLineStart': ['ctrl+u'],
    'tui.editor.deleteToLineEnd': ['ctrl+k'],
    'tui.editor.yank': ['ctrl+y'],
    'tui.editor.yankPop': ['alt+y'],
    'tui.editor.undo': ['ctrl+-'],
    'tui.select.confirm': ['enter'],
    'tui.select.cancel': ['escape', 'ctrl+c'],
    'tui.select.up': ['up'],
    'tui.select.down': ['down'],
    'tui.select.pageUp': ['pageUp'],
    'tui.select.pageDown': ['pageDown'],
    'tui.input.submit': ['enter'],
    'tui.input.newLine': ['shift+enter', 'ctrl+j'],
    'tui.input.tab': ['tab'],
    'tui.input.copy': ['ctrl+c'],
    'tui.altScreen.pageUp': ['pageUp'],
    'tui.altScreen.pageDown': ['pageDown'],
    'tui.altScreen.previousPrompt': ['ctrl+shift+up'],
    'tui.altScreen.nextPrompt': ['ctrl+shift+down'],
    'tui.altScreen.top': ['home'],
    'tui.altScreen.bottom': ['end'],
    'app.interrupt': ['escape'],
    'app.clear': ['ctrl+c'],
    'app.exit': ['ctrl+d'],
    'app.thinking.cycle': ['shift+tab'],
    'app.tools.expand': ['ctrl+o'],
  };

  const modifiers = { shift: 1, alt: 2, ctrl: 4, super: 8 };
  const named = {
    enter: 13, return: 13, escape: 27, esc: 27, tab: 9,
    backspace: 127, space: 32, up: -1, down: -2, right: -3, left: -4,
    home: -14, end: -15, insert: -11, delete: -10, pageup: -12, pagedown: -13,
  };
  const kittyAliases = new Map([
    [57414, 13], // keypad Enter
    [57417, -4], [57418, -3], [57419, -1], [57420, -2],
    [57421, -12], [57422, -13], [57423, -14], [57424, -15],
    [57425, -11], [57426, -10],
  ]);
  const legacy = {
    up: ['\x1b[A', '\x1bOA'], down: ['\x1b[B', '\x1bOB'],
    right: ['\x1b[C', '\x1bOC'], left: ['\x1b[D', '\x1bOD'],
    home: ['\x1b[H', '\x1bOH', '\x1b[1~', '\x1b[7~'],
    end: ['\x1b[F', '\x1bOF', '\x1b[4~', '\x1b[8~'],
    insert: ['\x1b[2~'], delete: ['\x1b[3~'],
    pageup: ['\x1b[5~', '\x1b[[5~'], pagedown: ['\x1b[6~', '\x1b[[6~'],
  };
  const parse = key => {
    const parts = String(key || '').toLowerCase().split('+');
    const name = parts.pop();
    if (!name) return null;
    let modifier = 0;
    for (const part of parts) {
      if (!Object.hasOwn(modifiers, part)) return null;
      modifier |= modifiers[part];
    }
    return { name, modifier };
  };
  const kitty = (data, codepoint, modifier) => {
    const match = String(data).match(/^\x1b\[(\d+);(\d+)u$/);
    if (!match || Number(match[2]) - 1 !== modifier) return false;
    const actual = Number(match[1]);
    return actual === codepoint || kittyAliases.get(actual) === codepoint;
  };
  const modifyOtherKeys = (data, codepoint, modifier) => {
    const match = String(data).match(/^\x1b\[27;(\d+);(\d+)~$/);
    return Boolean(match && Number(match[2]) === codepoint && Number(match[1]) - 1 === modifier);
  };
  const ctrlChar = name => {
    if (name === '-') return '\x1f';
    if (name.length !== 1) return null;
    const code = name.charCodeAt(0);
    if ((code >= 97 && code <= 122) || '[]\\_^'.includes(name)) return String.fromCharCode(code & 0x1f);
    return null;
  };
  const matchesOne = (data, key) => {
    const parsed = parse(key);
    if (!parsed) return false;
    const actual = String(data ?? '');
    const { name, modifier } = parsed;
    if (actual.toLowerCase() === String(key).toLowerCase()) return true;

    if (name === 'escape') return modifier === 0 && (actual === '\x1b' || kitty(actual, 27, 0) || modifyOtherKeys(actual, 27, 0));
    if (name === 'enter' || name === 'return') {
      if (modifier === 0) return actual === '\r' || actual === '\n' || actual === '\x1bOM' || kitty(actual, 13, 0) || modifyOtherKeys(actual, 13, 0);
      // Terminals without CSI-u may map Shift+Enter to ESC+CR (Kitty's
      // custom mapping) or LF (Ghostty's text mapping). Rust normally emits
      // CSI-u, but accepting these forms keeps extension keybindings useful
      // when input is forwarded by a nested terminal implementation.
      if (modifier === modifiers.shift && (actual === '\x1b\r' || actual === '\n')) return true;
      return kitty(actual, 13, modifier) || modifyOtherKeys(actual, 13, modifier);
    }
    if (name === 'tab') {
      if (modifier === 0) return actual === '\t' || kitty(actual, 9, 0);
      if (modifier === modifiers.shift && actual === '\x1b[Z') return true;
      return kitty(actual, 9, modifier) || modifyOtherKeys(actual, 9, modifier);
    }
    if (name === 'backspace') {
      if (modifier === 0 && actual === '\x7f') return true;
      if (modifier === modifiers.alt && (actual === '\x1b\x7f' || actual === '\x1b\b')) return true;
      return kitty(actual, 127, modifier) || modifyOtherKeys(actual, 127, modifier);
    }

    const codepoint = Object.hasOwn(named, name) ? named[name] : name.length === 1 ? name.charCodeAt(0) : undefined;
    if (codepoint === undefined) return false;
    if (modifier === 0) {
      if (name.length === 1) return actual === name;
      if (Object.hasOwn(legacy, name)) return legacy[name].includes(actual) || kitty(actual, codepoint, 0);
      if (name === 'space') return actual === ' ' || kitty(actual, 32, 0);
      return kitty(actual, codepoint, 0);
    }

    if (name.length === 1) {
      if (modifier === modifiers.ctrl) {
        const raw = ctrlChar(name);
        if (raw && actual === raw) return true;
      }
      if (modifier === modifiers.alt && actual === `\x1b${name}`) return true;
      if (modifier === modifiers.shift && name >= 'a' && name <= 'z' && actual === name.toUpperCase()) return true;
      return kitty(actual, codepoint, modifier) || modifyOtherKeys(actual, codepoint, modifier);
    }

    if (Object.hasOwn(legacy, name)) {
      const navigation = actual.match(/^\x1b\[1;(\d+)([ABCDHF])$/);
      if (navigation && Number(navigation[1]) - 1 === modifier) return true;
      const functional = actual.match(/^\x1b\[(\d+);(\d+)(~)$/);
      if (functional && Number(functional[2]) - 1 === modifier) return true;
      if (modifier === modifiers.alt) {
        const altLegacy = { up: '\x1bp', down: '\x1bn', left: '\x1bb', right: '\x1bf' };
        if (altLegacy[name] === actual) return true;
      }
    }
    return kitty(actual, codepoint, modifier) || modifyOtherKeys(actual, codepoint, modifier);
  };
  return {
    getKeys(action) { return defaults[action] || []; },
    matches(data, action) {
      return this.getKeys(action).some(key => matchesOne(data, key));
    },
  };
}
function customTerminal(customId, initialSize = {}) {
  const dimensions = {
    columns: Number(initialSize?.columns) || 120,
    rows: Number(initialSize?.rows) || 40,
  };
  const send = data => { void runtimeRequest('ui.custom.write', { customId, data: String(data ?? '') }).catch(() => {}); };
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
    setTitle(title) { void runtimeRequest('ui.custom.write', { customId, data: `\x1b]0;${String(title)}\x07` }).catch(() => {}); },
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

function renderCustomFrame(customId) {
  const custom = customs.get(customId);
  if (!custom?.component || custom.ownsTerminal || custom.hidden) return;
  if (typeof custom.component.render !== 'function') return;
  try {
    custom.component.invalidate?.();
    const width = Number(custom.terminal?.columns || custom.terminal?.info?.().columns || 120);
    const frame = custom.component.render(width);
    const prefix = '\x1b[2J\x1b[H';
    if (Array.isArray(frame) && frame.length) {
      void runtimeRequest('ui.custom.write', { customId, data: prefix + frame.join('\r\n') + '\r\n' }).catch(() => {});
    } else if (typeof frame === 'string' && frame.length) {
      void runtimeRequest('ui.custom.write', { customId, data: prefix + frame + '\r\n' }).catch(() => {});
    }
  } catch (error) {
    console.error(`JS custom component render failed: ${error?.stack || error}`);
  }
}

function customParent(customId, initialSize) {
  const terminal = customTerminal(customId, initialSize);
  return {
    mode: 'tui',
    terminal,
    altScreen: false,
    getShowHardwareCursor() { return false; },
    requestRender() {
      renderCustomFrame(customId);
      void runtimeRequest('ui.custom.invalidate', { customId }).catch(() => {});
    },
    renderNow() {
      renderCustomFrame(customId);
      void runtimeRequest('ui.custom.invalidate', { customId }).catch(() => {});
    },
    start() {},
    stop() {},
    addChild() {},
    removeChild() {},
    clear() {},
    setFocus() {},
    getFocus() { return null; },
    showOverlay() {
      return makeCustomHandle(customId);
    },
    hideOverlay() {},
    hasOverlay() { return false; },
  };
}

function disposeCustomComponent(customId) {
  const state = customs.get(customId);
  try { state?.component?.dispose?.(); } catch (error) {
    console.error(`JS custom component dispose failed: ${error?.stack || error}`);
  }
}

function makeCustomHandle(customId) {
  const state = { hidden: false, focused: true, removed: false };
  const send = operation => void runtimeRequest('ui.custom.handle', {
    customId,
    operation,
    hidden: state.hidden,
    removed: state.removed,
  }).catch(() => {});
  return {
    hide() {
      if (state.removed) return;
      // Native pi-tui treats hide() as permanent removal. setHidden() is the
      // reversible visibility control used by overlays such as the question
      // tool's collapse shortcut.
      state.removed = true;
      state.hidden = true;
      const custom = customs.get(customId);
      if (custom) {
        custom.hidden = true;
        custom.removed = true;
      }
      send('hide');
    },
    setHidden(hidden) {
      if (state.removed) return;
      state.hidden = Boolean(hidden);
      const custom = customs.get(customId);
      if (custom) {
        custom.hidden = state.hidden;
        custom.removed = false;
      }
      send('setHidden');
      if (!state.hidden) renderCustomFrame(customId);
    },
    isHidden() { return customs.get(customId)?.hidden ?? state.hidden; },
    focus() {
      if (state.removed) return;
      state.focused = true;
      send('focus');
    },
    isFocused() { return !state.removed && !this.isHidden() && state.focused; },
    unfocus() {
      if (state.removed) return;
      state.focused = false;
      send('unfocus');
    },
  };
}
async function invokeCustomInput(customId, data, inputId, hiddenHint) {
  const customAtDispatch = customs.get(customId);
  const hiddenAtDispatch = typeof hiddenHint === 'boolean'
    ? hiddenHint
    : customAtDispatch?.hidden === true;
  const inputResult = await invokeTerminalInput(data);
  const consumed = inputResult.consume === true;
  if (inputId != null) {
    // Rust uses this acknowledgement to decide whether a hidden overlay's
    // key should fall through to the outer editor. Keep it before component
    // dispatch so a slow component cannot block the key loop.
    await runtimeRequest('ui.custom.input', {
      customId,
      inputId: Number(inputId),
      consumed,
    }).catch(() => {});
  }
  // The overlay may become visible while the async listener is running, but a
  // key dispatched while it was hidden must never be replayed into the
  // component after the acknowledgement. A visible caller may also use the
  // acknowledgement path, so use the dispatch-time visibility snapshot rather
  // than treating every inputId as hidden.
  // Match native TUI input dispatch: a listener may transform the terminal
  // data to an empty string to swallow it without reporting `consume`. In
  // that case the focused component must not receive a synthetic empty key.
  if (consumed || (inputId != null && hiddenAtDispatch) || inputResult.data.length === 0) return;
  const custom = customs.get(customId);
  // Hidden overlays remain alive for their raw input listener, but their
  // component does not own focus while hidden.
  if (!custom?.component || custom.hidden) return;
  try {
    const input = inputResult.data;
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
    if (typeof handler === 'function') {
      await handler.call(component.fullscreen ? component.fullscreen : component, input);
      renderCustomFrame(customId);
    }
  } catch (error) {
    console.error(`JS custom component input failed: ${error?.stack || error}`);
  }
}

function queueCustomInput(customId, data, inputId, hiddenHint) {
  const previous = customInputQueues.get(customId) || Promise.resolve();
  const current = previous
    .catch(() => {})
    .then(() => invokeCustomInput(customId, data, inputId, hiddenHint));
  customInputQueues.set(customId, current);
  current
    .finally(() => {
      if (customInputQueues.get(customId) === current) customInputQueues.delete(customId);
    })
    .catch(() => {});
  return current;
}

function closeCustoms() {
  for (const [customId, custom] of customs.entries()) {
    try { custom.component?.dispose?.(); } catch (error) {
      console.error(`JS custom component dispose failed: ${error?.stack || error}`);
    }
    void runtimeRequest('ui.custom.close', { customId, restoreCustomId: null }).catch(() => {});
  }
  customs.clear();
  customStack.length = 0;
  customInputQueues.clear();
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
function commandContext(initial = {}, signal = new AbortController().signal, options = {}) {
  const notifications = [];
  const listenerIds = new Set();
  let editorText = String(initial.editorText ?? '');
  const ui = {
    notify(message, level = 'info') {
      const notification = { message: String(message), level: String(level) };
      notifications.push(notification);
      // Tool execution has no command-result envelope through which a
      // notification can return. Forward it immediately when the interactive
      // host installed the runtime handler; command handlers keep their
      // existing result-envelope path to avoid duplicate notices.
      if (options.deliverRuntime === true && (hostContext.hasUI === true || hostContext.mode === 'tui')) {
        void runtimeRequest('ui.notify', notification).catch(() => {});
      }
    },
    async select(title, options, opts = {}) {
      return requestUiDialog(
        'select',
        { title: String(title ?? ''), options: Array.isArray(options) ? options.map(String) : [], timeout: opts?.timeout },
        signal,
        undefined,
      );
    },
    async confirm(title, message, opts = {}) {
      return requestUiDialog(
        'confirm',
        { title: String(title ?? ''), message: String(message ?? ''), timeout: opts?.timeout },
        signal,
        false,
      );
    },
    async input(title, placeholder, opts = {}) {
      return requestUiDialog(
        'input',
        { title: String(title ?? ''), placeholder: placeholder == null ? undefined : String(placeholder), timeout: opts?.timeout },
        signal,
        undefined,
      );
    },
    async editor(title, prefill, opts = {}) {
      return requestUiDialog(
        'editor',
        { title: String(title ?? ''), prefill: prefill == null ? undefined : String(prefill), timeout: opts?.timeout },
        signal,
        undefined,
      );
    },
    onTerminalInput(handler) {
      if (typeof handler !== 'function') return () => {};
      const id = nextTerminalInputListenerId++;
      terminalInputListeners.set(id, handler);
      listenerIds.add(id);
      const remove = () => {
        terminalInputListeners.delete(id);
        listenerIds.delete(id);
      };
      if (signal?.aborted) remove();
      else signal?.addEventListener('abort', remove, { once: true });
      return remove;
    },
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
      if (signal?.aborted) return undefined;
      const customId = `custom-${nextCustomId++}`;
      let settled = false;
      let resolveResult;
      let rejectResult;
      let abortHandler;
      const result = new Promise((resolve, reject) => { resolveResult = resolve; rejectResult = reject; });
      const done = value => {
        if (settled) return;
        settled = true;
        if (abortHandler) signal?.removeEventListener('abort', abortHandler);
        disposeCustomComponent(customId);
        customs.delete(customId);
        const index = customStack.lastIndexOf(customId);
        if (index >= 0) customStack.splice(index, 1);
        const restoreCustomId = customStack.at(-1) || null;
        const restoreHidden = restoreCustomId
          ? customs.get(restoreCustomId)?.hidden === true
          : false;
        void runtimeRequest('ui.custom.close', { customId, restoreCustomId, restoreHidden })
          .catch(() => {})
          .then(() => {
            // The Rust host clears the terminal when a custom layer closes.
            // Repaint the still-live parent before resolving its promise so a
            // nested custom does not leave the child frame on screen.
            if (restoreCustomId) renderCustomFrame(restoreCustomId);
          })
          .finally(() => resolveResult(value));
      };
      customs.set(customId, { component: null, done, hidden: false, removed: false });
      abortHandler = () => done(undefined);
      signal?.addEventListener('abort', abortHandler, { once: true });
      try {
        customStack.push(customId);
        const openResult = await runtimeRequest('ui.custom.open', {
          customId,
          options: {
            overlay: Boolean(options?.overlay),
            overlayOptions: serializeOverlayOptions(options),
          },
        });
        if (settled) return result;
        const parent = customParent(customId, openResult);
        const component = await factory(parent, customTheme(), customKeybindings(), done);
        if (settled) {
          try { component?.dispose?.(); } catch (disposeError) {
            console.error(`JS custom component dispose failed: ${disposeError?.stack || disposeError}`);
          }
          return result;
        }
        const state = customs.get(customId);
        if (state) {
          state.terminal = parent.terminal;
          state.component = component;
          state.ownsTerminal = /FullscreenHost|TuiAltScreen/.test(String(component?.constructor?.name || ''));
          if (typeof options?.onHandle === 'function') {
            state.handle = makeCustomHandle(customId);
            await options.onHandle(state.handle);
          }
        }
        if (settled) return result;
        // Components that own a nested TUI (for example pi-btw's
        // BtwFullscreenHost) render directly through the terminal proxy. Their
        // lightweight placeholder render must not race and overwrite the
        // nested screen after it starts.
        const ownsTerminal = state?.ownsTerminal;
        if (component && typeof component.render === 'function' && !ownsTerminal) {
          renderCustomFrame(customId);
        }
      } catch (error) {
        if (settled) return result;
        settled = true;
        if (abortHandler) signal?.removeEventListener('abort', abortHandler);
        disposeCustomComponent(customId);
        customs.delete(customId);
        const index = customStack.lastIndexOf(customId);
        if (index >= 0) customStack.splice(index, 1);
        const restoreCustomId = customStack.at(-1) || null;
        const restoreHidden = restoreCustomId
          ? customs.get(restoreCustomId)?.hidden === true
          : false;
        void runtimeRequest('ui.custom.close', { customId, restoreCustomId, restoreHidden })
          .catch(() => {})
          .then(() => {
            if (restoreCustomId) renderCustomFrame(restoreCustomId);
          });
        rejectResult(error);
      }
      return result;
    },
  };
  const dispose = () => {
    for (const id of listenerIds) terminalInputListeners.delete(id);
    listenerIds.clear();
  };
  return {
    context: {
      mode: hostContext.mode || (hostContext.hasUI ? 'tui' : 'print'),
      hasUI: hostContext.hasUI === true || hostContext.mode === 'tui',
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
    dispose,
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
  const isExtensionFile = p => /[.](m?js|cjs|ts|tsx)$/.test(p);
  const visit = (p, seen = new Set(), discoverChildren = true) => {
    if (!fs.existsSync(p)) return;
    const stat = fs.statSync(p);
    if (stat.isFile()) {
      if (isExtensionFile(p)) out.push(p);
      return;
    }
    if (!stat.isDirectory()) return;
    const resolved = path.resolve(p);
    if (seen.has(resolved)) return;
    const nextSeen = new Set(seen).add(resolved);

    // Match Pi's package entry resolution. A manifest takes precedence, then
    // an index module, and only then the direct children of an extension dir.
    let manifest;
    try { manifest = JSON.parse(fs.readFileSync(path.join(p, 'package.json'), 'utf8')); } catch {}
    const declared = manifest?.rpi?.extensions ?? manifest?.pi?.extensions;
    if (Array.isArray(declared) && declared.length) {
      let resolvedAny = false;
      for (const entry of declared) {
        const target = path.resolve(p, String(entry));
        if (fs.existsSync(target)) {
          const before = out.length;
          visit(target, nextSeen, true);
          resolvedAny ||= out.length > before;
        }
      }
      if (resolvedAny) return;
    }
    for (const index of ['index.ts', 'index.js']) {
      const target = path.join(p, index);
      if (fs.existsSync(target)) {
        out.push(target);
        return;
      }
    }
    if (!discoverChildren) return;

    // Directory entries are intentionally inspected only one level deep, as
    // in native Pi. This prevents test/support modules under a package from
    // being treated as independent extensions.
    let entries;
    try { entries = fs.readdirSync(p, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name)); }
    catch { return; }
    for (const entry of entries) {
      if (entry.name.startsWith('.') || entry.name === 'node_modules') continue;
      const target = path.join(p, entry.name);
      if (entry.isFile() && isExtensionFile(target)) out.push(target);
      else if (entry.isDirectory()) visit(target, nextSeen, false);
    }
  };
  for (const p of value) visit(p);
  return out;
}

// jiti's alias resolver expects a concrete filesystem target. Mapping a
// package name to its directory works on POSIX for CommonJS packages, but on
// Windows it bypasses package `exports` resolution and eventually calls
// `require.resolve("C:/.../typebox")`. ESM packages such as `typebox` then
// fail before an extension can register its tools. Resolve package exports to
// their actual files while retaining package-prefix aliases for subpaths.
function firstExportTarget(value) {
  if (typeof value === 'string') return value;
  if (Array.isArray(value)) {
    for (const item of value) {
      const target = firstExportTarget(item);
      if (target) return target;
    }
    return undefined;
  }
  if (!value || typeof value !== 'object') return undefined;
  for (const key of ['import', 'node', 'default', 'require', 'development', 'production']) {
    const target = firstExportTarget(value[key]);
    if (target) return target;
  }
  return undefined;
}

function addPackageAliases(aliases, root, manifest) {
  const name = typeof manifest?.name === 'string' ? manifest.name : '';
  if (!name || Object.hasOwn(aliases, name)) return;
  const add = (specifier, target) => {
    if (!target || target.includes('*') || target.startsWith('#')) return;
    const resolved = path.resolve(root, target);
    if (fs.existsSync(resolved)) aliases[specifier] = resolved;
  };
  const exportsField = manifest.exports;
  if (typeof exportsField === 'string' || Array.isArray(exportsField)) {
    add(name, firstExportTarget(exportsField));
  } else if (exportsField && typeof exportsField === 'object') {
    const rootExport = Object.hasOwn(exportsField, '.')
      ? exportsField['.']
      : exportsField;
    add(name, firstExportTarget(rootExport));
    for (const [subpath, target] of Object.entries(exportsField)) {
      if (subpath.startsWith('./') && subpath.length > 2) {
        add(`${name}/${subpath.slice(2)}`, firstExportTarget(target));
      }
    }
  }
  if (!Object.hasOwn(aliases, name)) {
    add(name, manifest.module || manifest.main || './index.js');
  }
}

let jiti;
let piCodingAgent;
try {
  const roots = paths.map(p => fs.existsSync(p) && fs.statSync(p).isDirectory() ? p : path.dirname(p));
  const moduleRoots = [];
  const seenModuleRoots = new Set();
  const seenCollectionRoots = new Set();
  const seenPackageRoots = new Set();
  const addModuleRoot = root => {
    const resolved = path.resolve(root);
    if (!seenModuleRoots.has(resolved)) {
      seenModuleRoots.add(resolved);
      moduleRoots.push(resolved);
    }
  };
  // Walk package boundaries only. The previous generic directory recursion
  // descended through every `dist`/source directory below node_modules, which
  // made startup spend seconds scanning unrelated files on Windows. A Node
  // resolver only needs each node_modules root and its direct package roots;
  // recurse into a package's own node_modules for nested dependencies.
  const collect = (root, depth) => {
    if (depth < 0 || !fs.existsSync(root)) return;
    const resolvedRoot = path.resolve(root);
    if (seenCollectionRoots.has(resolvedRoot)) return;
    seenCollectionRoots.add(resolvedRoot);
    addModuleRoot(resolvedRoot);
    let entries;
    try { entries = fs.readdirSync(root, { withFileTypes: true }); }
    catch { return; }
    const addPackage = packageRoot => {
      const resolvedPackage = path.resolve(packageRoot);
      if (seenPackageRoots.has(resolvedPackage)
          || !fs.existsSync(path.join(resolvedPackage, 'package.json'))) return;
      seenPackageRoots.add(resolvedPackage);
      addModuleRoot(resolvedPackage);
      if (depth > 0) collect(path.join(resolvedPackage, 'node_modules'), depth - 1);
    };
    for (const entry of entries) {
      if (entry.name === '.bin' || !entry.isDirectory()) continue;
      const candidate = path.join(root, entry.name);
      if (entry.name.startsWith('@')) {
        let scoped;
        try { scoped = fs.readdirSync(candidate, { withFileTypes: true }); }
        catch { continue; }
        for (const packageEntry of scoped) {
          if (packageEntry.isDirectory()) addPackage(path.join(candidate, packageEntry.name));
        }
      } else {
        addPackage(candidate);
      }
    }
  };
  for (const root of [...new Set(roots.map(value => path.resolve(value)))]) {
    // Pi resolves dependencies through the package directory and a finite
    // number of parent node_modules folders. Walking to the filesystem root
    // makes startup scan unrelated user/project trees on Windows.
    let current = root;
    // Include the user's immediate node_modules parent. Native Pi packages
    // often resolve their peer runtime from there (for example the globally
    // installed coding-agent package), while the extension itself lives under
    // ~/.pi or ~/.rpi and therefore needs one more ancestor than a plain
    // package-local lookup.
    for (let i = 0; i < 7; i++) {
      collect(path.join(current, 'node_modules'), 4);
      const parent = path.dirname(current);
      if (parent === current) break;
      current = parent;
    }
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
      addPackageAliases(aliases, root, manifest);
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
  if (/[.]tsx?$/i.test(file)) {
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
  registerShortcut(shortcut, def = {}) {
    if (shortcut == null || String(shortcut).length === 0) throw new Error('registerShortcut requires a shortcut');
    shortcuts.set(String(shortcut), { ...def, shortcut: String(shortcut) });
  },
  registerFlag(name, def = {}) {
    if (!name || typeof name !== 'string') throw new Error('registerFlag requires a name');
    flags.set(name, { ...def, name });
  },
  getFlag(name) { return flags.get(String(name))?.default; },
  registerMessageRenderer(type, renderer) {
    if (type) messageRenderers.set(String(type), renderer);
  },
  registerEntryRenderer(type, renderer) {
    if (type) entryRenderers.set(String(type), renderer);
  },
  getActiveTools() { return [...activeToolNames]; },
  getAllTools() { return [...tools.keys()].map(name => ({ name })); },
  setActiveTools(names) {
    if (!Array.isArray(names)) throw new TypeError('setActiveTools requires an array');
    // Keep unknown names: the real API receives built-in tools that are
    // registered by the Rust host rather than this Node process. Rust filters
    // the returned JS subset when it applies the list to the full harness.
    activeToolNames = [...new Set(names.map(name => String(name)))];
  },
  events: {
    on(channel, handler) {
      return addExtensionEvent(channel, handler);
    },
    off(channel, handler) {
      extensionEvents.get(String(channel))?.delete(handler);
    },
    once(channel, handler) {
      const off = this.on(channel, value => { off(); return handler(value); });
      return off;
    },
    async emit(channel, value) {
      for (const handler of extensionEvents.get(String(channel)) || []) await handler(value);
    },
  },
  getThinkingLevel() { return runtimeThinkingLevel; },
  setLabel() {},
  runtimeRequest,
  on(event, handler) {
    if (event === 'resources_discover') resourceHandlers.push(handler);
    else return addExtensionEvent(event, handler);
  },
};

function snapshotRegistrations() {
  return {
    tools: new Map(tools),
    commands: new Map(commands),
    shortcuts: new Map(shortcuts),
    flags: new Map(flags),
    messageRenderers: new Map(messageRenderers),
    entryRenderers: new Map(entryRenderers),
    extensionEvents: new Map(
      [...extensionEvents].map(([channel, handlers]) => [channel, {
        handlers,
        values: [...handlers],
      }]),
    ),
    resourceHandlers: [...resourceHandlers],
    activeToolNames: [...activeToolNames],
  };
}

function restoreMap(target, snapshot) {
  target.clear();
  for (const [key, value] of snapshot) target.set(key, value);
}

function restoreRegistrations(snapshot) {
  restoreMap(tools, snapshot.tools);
  restoreMap(commands, snapshot.commands);
  restoreMap(shortcuts, snapshot.shortcuts);
  restoreMap(flags, snapshot.flags);
  restoreMap(messageRenderers, snapshot.messageRenderers);
  restoreMap(entryRenderers, snapshot.entryRenderers);
  extensionEvents.clear();
  for (const [channel, entry] of snapshot.extensionEvents) {
    // Keep the original Set alive: previously returned off() callbacks close
    // over this object and must remain able to remove their handler.
    entry.handlers.clear();
    for (const handler of entry.values) entry.handlers.add(handler);
    extensionEvents.set(channel, entry.handlers);
  }
  resourceHandlers.splice(0, resourceHandlers.length, ...snapshot.resourceHandlers);
  activeToolNames = snapshot.activeToolNames;
}

for (const file of files(paths)) {
  const snapshot = snapshotRegistrations();
  try {
    const mod = await loadFile(file);
    const factory = mod.default || mod;
    if (typeof factory !== 'function') {
      throw new Error('extension does not export a factory function');
    }
    await factory(api);
  } catch (error) {
    restoreRegistrations(snapshot);
    console.error(`Failed to load JS extension ${file}: ${error?.stack || error}`);
  }
}
activeToolNames = Array.isArray(hostContext.activeTools)
  ? hostContext.activeTools.map(name => String(name))
  : [...tools.keys()];
const resources = { skillPaths: [], promptPaths: [], themePaths: [] };
// Pi passes both the discovery event and an extension context to each
// resources_discover handler. Keep the same shape and isolate handler errors:
// one optional package must not prevent every other extension from loading.
const resourceEvent = {
  type: 'resources_discover',
  cwd: String(hostContext.cwd || process.cwd()),
  reason: 'startup',
};
const resourceRuntime = commandContext();
const resourceContext = resourceRuntime.context;
try {
  // This startup lifecycle pass exists to reconcile Node-side active tools and
  // discover package resources before Rust builds the harness. Its return
  // value is intentionally not treated as a per-turn prompt override; the
  // real agent turn owns that contract.
  if (!skipInitialBeforeAgentStart) {
    await emitExtensionEvent('before_agent_start', {
      type: 'before_agent_start',
      systemPrompt: String(hostContext.systemPrompt || ''),
      toolNames: [...activeToolNames],
    }, resourceContext);
  }
  if (!skipInitialDiscovery) {
    for (const handler of resourceHandlers) {
      try {
        const result = await handler(resourceEvent, resourceContext);
        if (result) {
          for (const key of Object.keys(resources)) {
            if (Array.isArray(result[key])) resources[key].push(...result[key]);
          }
        }
      } catch (error) {
        console.error(`JS resources_discover handler failed: ${error?.stack || error}`);
      }
    }
  }
} finally {
  resourceRuntime.dispose();
}
const toolSummaries = [...tools.values()].map(t => ({
  name: t.name,
  label: t.label || t.name,
  description: t.description || '',
  parameters: t.parameters || { type: 'object', properties: {} },
  executionMode: t.executionMode === 'sequential' ? 'sequential' : 'parallel'
}));
const initMessage = { id: 0, ok: true, result: {
  apiVersion: 1,
  capabilities: hostCapabilities,
  tools: toolSummaries,
  commands: [...commands.keys()],
  shortcuts: [...shortcuts.values()].map(({ shortcut, description }) => ({ shortcut, description: description || '' })),
  flags: [...flags.values()].map(({ name, description, type, default: defaultValue }) => ({ name, description: description || '', type, default: defaultValue })),
  messageRenderers: [...messageRenderers.keys()],
  entryRenderers: [...entryRenderers.keys()],
  activeTools: [...activeToolNames],
  resources
} };
write(initMessage);
if (process.env.RPI_JS_EXTENSION_ONESHOT === '1') {
  // Discovery only needs registrations. Do not enter the persistent request
  // loop, which keeps startup free of a long-lived Node runtime.
  setImmediate(() => process.exit(0));
}
async function handleLine(line) {
  if (!line.trim()) return;
  let request; try { request = JSON.parse(line); } catch { return; }
  if (request.type === 'host_event') {
    if (request.event === 'cancel_request') {
      activeHostRequests.get(request.id)?.abort();
      return;
    }
    if (request.event === 'custom_input') {
      void queueCustomInput(request.customId, request.data, request.inputId, request.hidden)
        .then(() => {
          if (request.id != null) write({ id: request.id, ok: true, result: null });
        })
        .catch(error => {
          if (request.id != null) write({ id: request.id, ok: false, error: String(error?.stack || error) });
        });
    }
    if (request.event === 'custom_resize') {
      const custom = customs.get(request.customId);
      custom?.terminal?._setSize(request.width, request.height);
      custom?.component?.handleResize?.(Number(request.width), Number(request.height));
      custom?.component?.invalidate?.();
      renderCustomFrame(request.customId);
      if (request.id != null) write({ id: request.id, ok: true, result: null });
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
  let runtimeCleanup;
  try {
    let result;
    if (request.method === 'invoke_tool') {
      const tool = tools.get(request.tool); if (!tool) throw new Error(`unknown JS tool: ${request.tool}`);
      // Pi passes the live extension context as the fifth execute argument.
      // The previous empty object made tool handlers silently lose `hasUI`,
      // `capabilities`, and every `ctx.ui.*` bridge even though commands got a
      // fully populated context from commandContext().
      const runtime = commandContext(request.context || {}, controller.signal, { deliverRuntime: true });
      runtimeCleanup = runtime.dispose;
      // Keep the native Pi tool contract: onUpdate is a synchronous callback
      // scoped to this execute invocation. Rust routes these host events by
      // toolCallId, so progress from parallel JS tools cannot cross streams.
      const onUpdate = partialResult => {
        try {
          write({
            type: 'host_event',
            event: 'tool_update',
            toolCallId: request.toolCallId || 'rpi',
            partialResult: partialResult ?? { content: [], details: null },
          });
        } catch (error) {
          // A malformed/circular update must not turn a successful tool call
          // into a failed invocation. Native Pi treats updates as best effort.
          console.error(`JS tool update could not be sent: ${error?.message || error}`);
        }
      };
      result = await tool.execute(
        request.toolCallId || 'rpi',
        request.args || {},
        controller.signal,
        onUpdate,
        runtime.context,
      );
    } else if (request.method === 'set_runtime_context') {
      const run = async () => {
        // A request can be cancelled while waiting behind an earlier hook.
        // Do not apply its stale context after cancellation.
        if (controller.signal.aborted) return { activeTools: [...activeToolNames] };
        const revision = Number(request.revision);
        if (Number.isSafeInteger(revision) && revision < runtimeContextRevision) {
          return { activeTools: [...activeToolNames] };
        }
        updateRuntimeContext(request.context || {});
        if (Number.isSafeInteger(revision)) runtimeContextRevision = revision;
        const lifecycleRuntime = commandContext(request.context || {}, controller.signal);
        try {
          await emitExtensionEvent('before_agent_start', {
            type: 'before_agent_start',
            systemPrompt: String(hostContext.systemPrompt || ''),
            toolNames: [...activeToolNames],
          }, lifecycleRuntime.context);
        } finally {
          lifecycleRuntime.dispose();
        }
        return { activeTools: [...activeToolNames] };
      };
      // `then(run, run)` also recovers if a prior hook unexpectedly rejects;
      // one failed lifecycle update must not permanently poison the queue.
      const queued = runtimeContextQueue.then(run, run);
      runtimeContextQueue = queued.catch(() => {});
      result = await queued;
    } else if (request.method === 'invoke_command') {
      const command = commands.get(request.command); if (!command) throw new Error(`unknown JS command: ${request.command}`);
      const runtime = commandContext(request.context || {}, controller.signal);
      runtimeCleanup = runtime.dispose;
      result = await command.handler(request.args || '', runtime.context);
      result = {
        result: result ?? null,
        notifications: runtime.notifications,
        editorText: runtime.context.ui.getEditorText(),
        activeTools: [...activeToolNames],
      };
    } else throw new Error(`unknown method: ${request.method}`);
    write({ id: request.id, ok: true, result: result ?? null });
  } catch (error) {
    if (request.method === 'invoke_command') closeCustoms();
    write({ id: request.id, ok: false, error: String(error?.stack || error) });
  } finally {
    runtimeCleanup?.();
    activeHostRequests.delete(request.id);
  }
}
// Keep the persistent host alive without an unresolved top-level await. Node
// can now terminate cleanly on signals/stdin close instead of warning about an
// unsettled promise during normal shutdown.
const keepAlive = setInterval(() => {}, 0x7fffffff);
process.once('SIGINT', () => { clearInterval(keepAlive); process.exit(0); });
process.once('SIGTERM', () => { clearInterval(keepAlive); process.exit(0); });
