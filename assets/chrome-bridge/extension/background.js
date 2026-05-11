// Glance Chrome Bridge — MV3 service worker.
//
// Owns a long-lived Chrome native messaging connection to `com.glance.chrome`.
// The native host bridges Chrome stdio <-> a unix socket where the glance MCP
// server listens. Requests come in as { id, method, params }; we answer with
// { id, result } or { id, error }.

const HOST_NAME = "com.glance.chrome";
const RECONNECT_BACKOFF_MS = [500, 1000, 2000, 5000, 10000];

let port = null;
let reconnectAttempt = 0;
let lastError = null;
const debuggerAttached = new Set();

function log(...args) {
  console.log("[glance-bridge]", ...args);
}

// MV3 service workers can be killed by Chrome between event handler ticks.
// chrome.* calls scheduled before death (alarms, setTimeout reconnects) then
// resume into a "No SW" error when the worker wakes. The error is harmless —
// our reconnect logic + chrome.alarms keepalive handle the restart. Swallow
// these so the extension page doesn't show red "Uncaught" lines.
self.addEventListener("unhandledrejection", (ev) => {
  const msg = String(ev.reason?.message || ev.reason || "");
  if (msg.includes("No SW") || msg.includes("Could not establish connection") ||
      msg.includes("message port closed") || msg.includes("Extension context invalidated")) {
    ev.preventDefault();
    log("ignored lifecycle race:", msg);
  }
});

function connect() {
  if (port) return port;
  try {
    port = chrome.runtime.connectNative(HOST_NAME);
    lastError = null;
    log("connected to native host");
    reconnectAttempt = 0;
    port.onMessage.addListener(handleRequest);
    port.onDisconnect.addListener(() => {
      const err = chrome.runtime.lastError?.message || "disconnected";
      lastError = err;
      log("disconnected:", err);
      port = null;
      scheduleReconnect();
    });
    port.postMessage({ kind: "hello", version: "0.1.0", ts: Date.now() });
  } catch (e) {
    lastError = String(e);
    log("connectNative threw:", e);
    port = null;
    scheduleReconnect();
  }
  return port;
}

function scheduleReconnect() {
  const delay = RECONNECT_BACKOFF_MS[Math.min(reconnectAttempt, RECONNECT_BACKOFF_MS.length - 1)];
  reconnectAttempt += 1;
  setTimeout(() => connect(), delay);
}

function send(msg) {
  const p = port || connect();
  if (!p) return;
  try {
    p.postMessage(msg);
  } catch (e) {
    // Port can die mid-send if the SW is being torn down. Silently mark
    // disconnected — the keepalive alarm will reconnect on next tick.
    log("postMessage failed (port dead?):", e?.message || e);
    port = null;
  }
}

async function handleRequest(msg) {
  if (!msg || typeof msg !== "object") return;
  if (msg.kind === "ping") {
    send({ kind: "pong", ts: Date.now() });
    return;
  }
  const { id, method, params } = msg;
  if (id == null || !method) return;
  try {
    const result = await dispatch(method, params || {});
    send({ id, result });
  } catch (e) {
    send({ id, error: String(e?.message || e) });
  }
}

// ----- method dispatch -----

const HANDLERS = {
  "tabs.list": tabsList,
  "tabs.activate": tabsActivate,
  "tabs.create": tabsCreate,
  "tabs.close": tabsClose,
  "tabs.navigate": tabsNavigate,
  "tabs.evaluate": tabsEvaluate,
  "tabs.screenshot": tabsScreenshot,
  "tabs.wait_load": tabsWaitLoad,
  "tabs.snapshot": tabsSnapshot,
  "tabs.wait_for": tabsWaitFor,
  "tabs.press_key": tabsPressKey,
  "tabs.type_text": tabsTypeText,
  "tabs.hover": tabsHover,
  "tabs.navigate_back": tabsNavigateBack,
  "tabs.navigate_forward": tabsNavigateForward,
  "tabs.select_option": tabsSelectOption,
  "tabs.fill_form": tabsFillForm,
  "tabs.drag": tabsDrag,
  "tabs.upload_file": tabsUploadFile,
  "tabs.resize": tabsResize,
  "tabs.emulate": tabsEmulate,
  "tabs.set_contenteditable": tabsSetContenteditable,
  "tabs.paste_text": tabsPasteText,
  "tabs.paste_keyboard": tabsPasteKeyboard,
  "tabs.type_multiline": tabsTypeMultiline,
  "tabs.submit_post": tabsSubmitPost,
  "tabs.click_native": tabsClickNative,
  "tabs.fill_native": tabsFillNative,
  "console.list": consoleList,
  "console.clear": consoleClear,
  "dialog.handle": dialogHandle,
  "dialog.list_pending": dialogListPending,
  "perf.start_trace": perfStartTrace,
  "perf.stop_trace": perfStopTrace,
  "perf.heap_snapshot": perfHeapSnapshot,
  "network.list": networkList,
  "network.get": networkGet,
  "cdp.send": cdpSend,
  "cdp.detach": cdpDetach,
};

async function dispatch(method, params) {
  const h = HANDLERS[method];
  if (!h) throw new Error(`unknown method: ${method}`);
  // Group the target tab into the Glance control group so users can see at a
  // glance which tabs we're driving. Best-effort — never fail the action.
  if (params?.tabId != null) {
    ensureGlanceGroup(params.tabId, method).catch((e) => log("group err", e));
  }
  return await h(params);
}

// ----- Chrome tab group control UI -----
//
// Codex-style visual indicator: every tab Glance touches is auto-added to a
// purple Chrome tab group titled "Glance · <last method>". Users see at a
// glance which tabs we're driving; right-click → Ungroup to evict.

let glanceGroupId = null;       // groupId per current Chrome session
let glanceGroupWindowId = null;
let groupActivityTimer = null;

async function ensureGlanceGroup(tabId, method) {
  try {
    const tab = await chrome.tabs.get(tabId);
    if (!tab) return;
    // Skip system tabs.
    if (tab.url?.startsWith("chrome://") || tab.url?.startsWith("chrome-extension://")) return;
    // Don't try to group if the tab is already in our group.
    if (tab.groupId === glanceGroupId && glanceGroupId != null && glanceGroupId !== -1) {
      // Update title to reflect latest method — keeps the pill informative.
      try {
        await chrome.tabGroups.update(glanceGroupId, { title: groupTitleFor(method) });
      } catch (_) {}
      bumpGroupActivity();
      return;
    }
    // (Re)create group in this tab's window when none / wrong window.
    if (glanceGroupId == null || glanceGroupId === -1 || glanceGroupWindowId !== tab.windowId) {
      const newGroupId = await chrome.tabs.group({ tabIds: [tabId] });
      glanceGroupId = newGroupId;
      glanceGroupWindowId = tab.windowId;
      await chrome.tabGroups.update(newGroupId, {
        title: groupTitleFor(method),
        color: "purple",
        collapsed: false,
      });
    } else {
      await chrome.tabs.group({ groupId: glanceGroupId, tabIds: [tabId] });
      await chrome.tabGroups.update(glanceGroupId, { title: groupTitleFor(method) });
    }
    bumpGroupActivity();
  } catch (e) {
    // Group may have been manually deleted; reset so next call recreates.
    glanceGroupId = null;
    glanceGroupWindowId = null;
    log("ensureGlanceGroup failed:", e?.message || e);
  }
}

function groupTitleFor(method) {
  // Keep it tight — Chrome shows ~20 chars before truncating.
  const short = (method || "").replace(/^tabs\./, "").replace(/_/g, " ");
  return short ? `Glance · ${short}` : "Glance";
}

function bumpGroupActivity() {
  // Auto-dissolve the group 5 min after the last MCP call so abandoned
  // sessions don't leave permanent groupings.
  if (groupActivityTimer) clearTimeout(groupActivityTimer);
  groupActivityTimer = setTimeout(dissolveGlanceGroup, 5 * 60 * 1000);
}

async function dissolveGlanceGroup() {
  if (glanceGroupId == null || glanceGroupId === -1) return;
  try {
    const tabs = await chrome.tabs.query({ groupId: glanceGroupId });
    if (tabs.length) {
      await chrome.tabs.ungroup(tabs.map((t) => t.id));
    }
  } catch (e) {
    log("dissolveGlanceGroup err:", e?.message || e);
  }
  glanceGroupId = null;
  glanceGroupWindowId = null;
}

async function tabsList() {
  const tabs = await chrome.tabs.query({});
  return tabs.map((t) => ({
    id: t.id,
    windowId: t.windowId,
    title: t.title || "",
    url: t.url || "",
    active: !!t.active,
    pinned: !!t.pinned,
    audible: !!t.audible,
    status: t.status || "",
  }));
}

async function tabsActivate({ tabId }) {
  await chrome.tabs.update(tabId, { active: true });
  const t = await chrome.tabs.get(tabId);
  await chrome.windows.update(t.windowId, { focused: true });
  return { ok: true };
}

async function tabsCreate({ url, windowId }) {
  const t = await chrome.tabs.create({ url, windowId, active: true });
  return { id: t.id, windowId: t.windowId };
}

async function tabsClose({ tabId }) {
  await chrome.tabs.remove(tabId);
  return { ok: true };
}

async function tabsNavigate({ tabId, url }) {
  await chrome.tabs.update(tabId, { url });
  return { ok: true };
}

async function tabsEvaluate({ tabId, expression, awaitPromise, world, raw }) {
  const w = (world || "main").toLowerCase();

  // CDP path: chrome.debugger.Runtime.evaluate. Debugger privilege bypasses the
  // page's CSP entirely, so sites like X.com that block `unsafe-eval` (which
  // would otherwise kill the MAIN-world `new Function` path) still work.
  if (w === "cdp") {
    await ensureAttached(tabId);
    // Three modes:
    //  1. raw=true       → expression passed verbatim, no wrap. Caller owns
    //                      the return shape (use console.log + list_console
    //                      or stick the value as the final expression).
    //  2. looksLikeIIFE  → starts with `(` / `async ` — already a self-
    //                      invoked expression that yields a value. Pass
    //                      through; old double-wrap created nested Promises
    //                      and risked SyntaxError on inner `const`/`let`.
    //  3. plain expression → wrap with `return (expr)` so a single-expression
    //                      caller (`document.body`, `location.href`) still
    //                      gets the value back. Critical: bare wrapping
    //                      without `return` yields undefined.
    const trimmed = (expression || "").trim();
    const looksLikeIIFE = trimmed.startsWith("(") || /^async\s/.test(trimmed);
    let finalExpr;
    if (raw) {
      finalExpr = expression;
    } else if (looksLikeIIFE) {
      finalExpr = expression;
    } else {
      finalExpr = `(async () => { return (${expression}); })()`;
    }
    const resp = await chrome.debugger.sendCommand({ tabId }, "Runtime.evaluate", {
      expression: finalExpr,
      awaitPromise: !!awaitPromise || !raw,
      returnByValue: true,
      userGesture: true,
    });
    if (resp?.exceptionDetails) {
      const ex = resp.exceptionDetails;
      throw new Error(ex.exception?.description || ex.text || "Runtime.evaluate threw");
    }
    const r = resp?.result || {};
    if (r.type === "undefined") return null;
    if (r.value !== undefined) return r.value;
    // Non-serializable result — give the caller diagnostic info instead of
    // silently returning null. Common cases: DOM nodes (Element), Functions,
    // Promises that didn't resolve, Symbols, circular objects.
    return {
      __glance_eval__: "non-serializable",
      type: r.type,
      subtype: r.subtype,
      className: r.className,
      description: r.description,
      hint: r.subtype === "node"
        ? "Returned a DOM node — extract serializable props (e.g. el.textContent, el.id, el.getAttribute(...)) instead."
        : (r.type === "function"
          ? "Returned a function — call it or describe it with .toString()."
          : "Cannot returnByValue; map to a plain JSON object first."),
    };
  }

  // MAIN world (default): chrome.scripting + new Function. Fast, no debugger
  // attach, but subject to page CSP.
  const wrapped = `(async () => { return (${expression}); })()`;
  const [res] = await chrome.scripting.executeScript({
    target: { tabId },
    world: "MAIN",
    func: (code, awaitIt) => {
      try {
        // eslint-disable-next-line no-new-func
        const v = new Function(`return (${code});`)();
        if (awaitIt && v && typeof v.then === "function") {
          return v.then(
            (r) => ({ ok: true, value: safe(r) }),
            (e) => ({ ok: false, error: String(e?.message || e) }),
          );
        }
        return { ok: true, value: safe(v) };
      } catch (e) {
        return { ok: false, error: String(e?.message || e) };
      }
      function safe(v) {
        if (v === undefined) return null;
        try {
          JSON.stringify(v);
          return v;
        } catch (_) {
          return String(v);
        }
      }
    },
    args: [wrapped, !!awaitPromise],
  });
  if (!res || !res.result) throw new Error("executeScript returned nothing");
  if (!res.result.ok) throw new Error(res.result.error);
  return res.result.value;
}

// Paste a string into a rich-text editor via the browser's paste pipeline.
// This is the *only* reliable path for multi-paragraph text into Draft.js
// (X.com) / Lexical / ProseMirror — newlines survive because the editor's
// `handlePastedText` runs, building proper block structure. Same shape as
// ReplyX AI's content-script flow.
async function tabsPasteText({ tabId, selector, value, clearFirst }) {
  const [res] = await chrome.scripting.executeScript({
    target: { tabId },
    world: "ISOLATED",
    func: async (sel, val, clearFirst) => {
      const el = document.querySelector(sel);
      if (!el) throw new Error(`paste target not found: ${sel}`);
      el.focus({ preventScroll: false });

      if (clearFirst) {
        // selectAll + delete via execCommand. NOT bulletproof on Draft.js
        // (it may leave residual block state) but works for most editors.
        try {
          const r = document.createRange();
          r.selectNodeContents(el);
          const s = window.getSelection();
          s.removeAllRanges();
          s.addRange(r);
          document.execCommand("delete", false);
        } catch (_) { /* fall through */ }
      }

      // DataTransfer carries the text; ClipboardEvent("paste") triggers the
      // editor's beforeinput("insertFromPaste") + paste handlers in one go.
      const dt = new DataTransfer();
      dt.setData("text/plain", val);
      const ev = new ClipboardEvent("paste", {
        bubbles: true,
        cancelable: true,
        clipboardData: dt,
      });
      const notCanceled = el.dispatchEvent(ev);
      // Draft.js/Lexical typically `preventDefault` (= dispatchEvent → false)
      // because they consume the paste internally — that's the success case.

      // Some editors only update on follow-up `input` / `change`.
      el.dispatchEvent(new Event("input",  { bubbles: true }));
      el.dispatchEvent(new Event("change", { bubbles: true }));

      // Give React state a tick to settle before the caller reads .innerText.
      await new Promise((r) => setTimeout(r, 200));

      return {
        ok: true,
        canceled: !notCanceled, // true = editor took it (the path we want)
        text: (el.innerText || "").slice(0, 4096),
      };
    },
    args: [selector, value, !!clearFirst],
  });
  if (!res || !res.result) throw new Error("paste_text returned nothing");
  return res.result;
}

// ISOLATED world: predefined func, no eval. Targets rich-text editors like
// Draft.js (X.com), Lexical, ProseMirror, Slate that listen for
// `beforeinput` / `input` with `inputType: "insertText"`. Path A: execCommand
// (the only browser-driven way to fire a real composition-style InputEvent).
// Path B: hand-rolled InputEvent fallback when execCommand returns false
// (some editors block execCommand).
async function tabsSetContenteditable({ tabId, selector, value, replaceAll }) {
  const replace = replaceAll !== false;
  const [res] = await chrome.scripting.executeScript({
    target: { tabId },
    world: "ISOLATED",
    func: (sel, val, replace) => {
      const el = document.querySelector(sel);
      if (!el) throw new Error(`contenteditable not found: ${sel}`);
      // execCommand needs the editor focused AND a live selection inside it.
      el.focus({ preventScroll: false });
      if (replace) {
        try {
          const r = document.createRange();
          r.selectNodeContents(el);
          const s = window.getSelection();
          s.removeAllRanges();
          s.addRange(r);
        } catch (_) { /* fall through */ }
      }
      let path = "execCommand";
      let ok = false;
      try { ok = document.execCommand("insertText", false, val); } catch (_) { ok = false; }
      if (!ok) {
        path = "inputEvent";
        try {
          el.dispatchEvent(new InputEvent("beforeinput", {
            inputType: "insertText", data: val, bubbles: true, cancelable: true,
          }));
          el.dispatchEvent(new InputEvent("input", {
            inputType: "insertText", data: val, bubbles: true, cancelable: true,
          }));
          ok = true;
        } catch (e) {
          throw new Error("insertText: execCommand and InputEvent both failed: " + (e?.message || e));
        }
      }
      return { ok, path, text: (el.innerText || "").slice(0, 4096) };
    },
    args: [selector, value, replace],
  });
  if (!res || !res.result) throw new Error("set_contenteditable returned nothing");
  return res.result;
}

async function tabsScreenshot({ tabId, format }) {
  // captureVisibleTab needs the tab to be active. Activate first if needed.
  if (tabId != null) {
    const t = await chrome.tabs.get(tabId);
    if (!t.active) await chrome.tabs.update(tabId, { active: true });
    var windowId = t.windowId;
  }
  const fmt = format === "jpeg" ? "jpeg" : "png";
  const dataUrl = await chrome.tabs.captureVisibleTab(windowId, { format: fmt });
  // Strip the data:image/png;base64, prefix; caller wants raw base64.
  const base64 = dataUrl.replace(/^data:image\/[a-z]+;base64,/, "");
  return { format: fmt, base64 };
}

async function tabsWaitLoad({ tabId, timeoutMs }) {
  const limit = timeoutMs || 15000;
  const start = Date.now();
  while (Date.now() - start < limit) {
    const t = await chrome.tabs.get(tabId);
    if (t.status === "complete") return { ok: true, elapsedMs: Date.now() - start };
    await new Promise((r) => setTimeout(r, 200));
  }
  return { ok: false, error: "timeout" };
}

// Per-tab idle timer for the chrome.debugger "started debugging this browser"
// yellow bar UX. Keep the attach alive across rapid CDP calls (one attach,
// one bar), but auto-detach after IDLE_DETACH_MS of silence so the bar goes
// away when Glance isn't actively driving the tab. The next CDP method
// will transparently re-attach (and pop the bar once more).
const IDLE_DETACH_MS = 60_000;
const debuggerIdleTimers = new Map(); // tabId -> setTimeout handle

async function ensureAttached(tabId) {
  if (!debuggerAttached.has(tabId)) {
    await chrome.debugger.attach({ tabId }, "1.3");
    debuggerAttached.add(tabId);
  }
  bumpDebuggerActivity(tabId);
}

function bumpDebuggerActivity(tabId) {
  const prev = debuggerIdleTimers.get(tabId);
  if (prev) clearTimeout(prev);
  const timer = setTimeout(() => idleDetach(tabId), IDLE_DETACH_MS);
  debuggerIdleTimers.set(tabId, timer);
}

async function idleDetach(tabId) {
  debuggerIdleTimers.delete(tabId);
  if (!debuggerAttached.has(tabId)) return;
  // Don't auto-detach if there are active CDP-event subscriptions — that would
  // silently drop the user's data stream (Network capture, console mirror,
  // dialog queue). Those are explicit opt-ins, keep them connected until the
  // user disables them.
  if (networkEnabled.has(tabId) || consoleEnabled.has(tabId) || dialogEnabled.has(tabId)) {
    bumpDebuggerActivity(tabId); // reschedule
    return;
  }
  try {
    await chrome.debugger.detach({ tabId });
    log("idle-detached", tabId);
  } catch (e) {
    log("idle-detach failed:", e?.message || e);
  }
  debuggerAttached.delete(tabId);
}

async function cdpSend({ tabId, method, params }) {
  await ensureAttached(tabId);
  return await chrome.debugger.sendCommand({ tabId }, method, params || {});
}

async function cdpDetach({ tabId }) {
  const t = debuggerIdleTimers.get(tabId);
  if (t) { clearTimeout(t); debuggerIdleTimers.delete(tabId); }
  if (!debuggerAttached.has(tabId)) return { ok: true, attached: false };
  await chrome.debugger.detach({ tabId });
  debuggerAttached.delete(tabId);
  return { ok: true, attached: true };
}

chrome.debugger.onDetach.addListener((source, _reason) => {
  if (source.tabId != null) {
    debuggerAttached.delete(source.tabId);
    const t = debuggerIdleTimers.get(source.tabId);
    if (t) { clearTimeout(t); debuggerIdleTimers.delete(source.tabId); }
    networkBuffers.delete(source.tabId);
    networkEnabled.delete(source.tabId);
  }
});

// ----- network capture (CDP Network domain) -----
//
// We keep one in-memory ring per tab capped at NETWORK_BUFFER_MAX. Each entry
// gets the CDP request id as key and accumulates the lifecycle: requestWillBeSent
// → responseReceived → loadingFinished | loadingFailed. Bodies are NOT cached
// up-front (Network.getResponseBody can refuse for streaming/large responses);
// `network.get` fetches them on demand.

const NETWORK_BUFFER_MAX = 500;
const networkBuffers = new Map();   // tabId -> Map<requestId, record>
const networkEnabled = new Set();   // tabIds where Network.enable has been sent

async function ensureNetworkEnabled(tabId) {
  await ensureAttached(tabId);
  if (networkEnabled.has(tabId)) return;
  await chrome.debugger.sendCommand({ tabId }, "Network.enable", {
    maxResourceBufferSize: 8 * 1024 * 1024,
    maxTotalBufferSize:    32 * 1024 * 1024,
  });
  networkEnabled.add(tabId);
  if (!networkBuffers.has(tabId)) networkBuffers.set(tabId, new Map());
}

function bufferFor(tabId) {
  let buf = networkBuffers.get(tabId);
  if (!buf) {
    buf = new Map();
    networkBuffers.set(tabId, buf);
  }
  return buf;
}

function trimBuffer(buf) {
  while (buf.size > NETWORK_BUFFER_MAX) {
    const oldest = buf.keys().next().value;
    if (oldest === undefined) break;
    buf.delete(oldest);
  }
}

chrome.debugger.onEvent.addListener((source, method, params) => {
  const tabId = source.tabId;
  if (tabId == null || !networkEnabled.has(tabId)) return;
  const buf = bufferFor(tabId);

  switch (method) {
    case "Network.requestWillBeSent": {
      const req = params.request || {};
      buf.set(params.requestId, {
        requestId: params.requestId,
        ts: params.timestamp,
        wallTime: params.wallTime,
        method: req.method || "GET",
        url: req.url || "",
        type: params.type || "",
        initiatorType: params.initiator?.type || "",
        requestHeaders: req.headers || {},
        requestBody: req.postData || null,
        status: null,
        mime: null,
        responseHeaders: null,
        encodedDataLength: null,
        finished: false,
        error: null,
      });
      trimBuffer(buf);
      break;
    }
    case "Network.responseReceived": {
      const r = buf.get(params.requestId);
      if (!r) return;
      const resp = params.response || {};
      r.status = resp.status;
      r.statusText = resp.statusText;
      r.mime = resp.mimeType;
      r.responseHeaders = resp.headers || {};
      r.remoteIp = resp.remoteIPAddress;
      r.fromCache = !!resp.fromDiskCache || !!resp.fromServiceWorker;
      break;
    }
    case "Network.loadingFinished": {
      const r = buf.get(params.requestId);
      if (!r) return;
      r.finished = true;
      r.encodedDataLength = params.encodedDataLength;
      r.tsFinished = params.timestamp;
      break;
    }
    case "Network.loadingFailed": {
      const r = buf.get(params.requestId);
      if (!r) return;
      r.finished = true;
      r.error = params.errorText || "loading failed";
      r.tsFinished = params.timestamp;
      break;
    }
  }
});

async function networkList({ tabId, urlContains, methodIs, statusIs, mimeContains, sinceSecs, limit, includePending }) {
  await ensureNetworkEnabled(tabId);
  const buf = networkBuffers.get(tabId) || new Map();
  const now = Date.now() / 1000;
  const sinceWall = sinceSecs ? now - sinceSecs : 0;
  const max = Math.max(1, Math.min(500, limit || 100));
  const out = [];
  for (const r of buf.values()) {
    if (!includePending && !r.finished) continue;
    if (sinceWall && r.wallTime && r.wallTime < sinceWall) continue;
    if (urlContains && !r.url.includes(urlContains)) continue;
    if (methodIs && r.method !== methodIs.toUpperCase()) continue;
    if (statusIs != null && r.status !== statusIs) continue;
    if (mimeContains && (!r.mime || !r.mime.includes(mimeContains))) continue;
    out.push({
      requestId: r.requestId,
      method: r.method,
      url: r.url,
      status: r.status,
      mime: r.mime,
      bytes: r.encodedDataLength,
      type: r.type,
      error: r.error,
      finished: r.finished,
      wallTime: r.wallTime,
    });
    if (out.length >= max) break;
  }
  // Sort newest first.
  out.sort((a, b) => (b.wallTime || 0) - (a.wallTime || 0));
  return { tabId, count: out.length, total: buf.size, requests: out };
}

async function networkGet({ tabId, requestId, includeBody }) {
  await ensureNetworkEnabled(tabId);
  const buf = networkBuffers.get(tabId);
  const r = buf?.get(requestId);
  if (!r) throw new Error(`requestId not in buffer (tab ${tabId}): ${requestId}`);
  let body = null;
  let bodyBase64 = false;
  let bodyError = null;
  if (includeBody !== false && r.finished && !r.error) {
    try {
      const resp = await chrome.debugger.sendCommand(
        { tabId },
        "Network.getResponseBody",
        { requestId },
      );
      body = resp.body || null;
      bodyBase64 = !!resp.base64Encoded;
    } catch (e) {
      bodyError = String(e?.message || e);
    }
  }
  return {
    ...r,
    body,
    bodyBase64,
    bodyError,
  };
}

// ----- snapshot -----

async function tabsSnapshot({ tabId, mode, maxChars }) {
  const limit = Math.max(200, Math.min(200_000, maxChars || 8000));
  const want = (mode || "text").toLowerCase();

  if (want === "a11y") {
    await ensureAttached(tabId);
    const tree = await chrome.debugger.sendCommand({ tabId }, "Accessibility.getFullAXTree", {});
    // Compress: keep role/name/value lines only, indent by depth.
    const lines = [];
    const byId = new Map();
    for (const n of tree.nodes || []) byId.set(n.nodeId, n);
    function walk(id, depth) {
      const n = byId.get(id);
      if (!n || n.ignored) return;
      const role = n.role?.value || "";
      const name = n.name?.value || "";
      const val  = n.value?.value || "";
      if (role || name) {
        lines.push("  ".repeat(depth) + `[${role}] ${name}${val ? ` = ${val}` : ""}`);
      }
      for (const c of n.childIds || []) walk(c, depth + 1);
    }
    if (tree.nodes && tree.nodes[0]) walk(tree.nodes[0].nodeId, 0);
    let s = lines.join("\n");
    const truncated = s.length > limit;
    if (truncated) s = s.slice(0, limit);
    return { mode: "a11y", chars: s.length, truncated, content: s };
  }

  // text / html via injected script (ISOLATED — predefined func, no eval, immune to page CSP)
  const [res] = await chrome.scripting.executeScript({
    target: { tabId },
    world: "ISOLATED",
    func: (mode, limit) => {
      function clean(t) {
        return (t || "").replace(/[ \t]+/g, " ").replace(/\n{3,}/g, "\n\n").trim();
      }
      if (mode === "html") {
        // Strip script/style/noscript content; preserve tags otherwise.
        const clone = document.documentElement.cloneNode(true);
        clone.querySelectorAll("script,style,noscript,template").forEach((el) => el.remove());
        const html = clone.outerHTML || "";
        return { html: html.length > limit ? html.slice(0, limit) : html, truncated: html.length > limit, total: html.length };
      }
      const txt = clean(document.body?.innerText || document.documentElement?.innerText || "");
      return { text: txt.length > limit ? txt.slice(0, limit) : txt, truncated: txt.length > limit, total: txt.length };
    },
    args: [want === "html" ? "html" : "text", limit],
  });
  if (!res || !res.result) throw new Error("snapshot script returned nothing");
  const data = res.result;
  return {
    mode: want === "html" ? "html" : "text",
    chars: (data.text || data.html || "").length,
    truncated: !!data.truncated,
    totalChars: data.total,
    content: data.text || data.html || "",
  };
}

// ----- tier-1: simple sugar (mostly executeScript / chrome.tabs / CDP Input) -----

async function tabsWaitFor({ tabId, selector, text, timeoutMs }) {
  const limit = Math.max(100, Math.min(60_000, timeoutMs || 10_000));
  const start = Date.now();
  while (Date.now() - start < limit) {
    const [res] = await chrome.scripting.executeScript({
      target: { tabId },
      world: "ISOLATED",
      func: (sel, t) => {
        if (sel) {
          const el = document.querySelector(sel);
          if (el && (el.offsetParent !== null || el.getClientRects().length))
            return { found: true, selector: sel, tag: el.tagName };
        }
        if (t) {
          if ((document.body?.innerText || "").includes(t)) return { found: true, text: t };
        }
        return { found: false };
      },
      args: [selector || null, text || null],
    });
    if (res?.result?.found) return { ok: true, elapsedMs: Date.now() - start, ...res.result };
    await new Promise((r) => setTimeout(r, 200));
  }
  return { ok: false, error: "timeout", elapsedMs: limit };
}

const KEY_MAP = {
  Enter: { code: "Enter", key: "Enter", windowsVirtualKeyCode: 13 },
  Tab: { code: "Tab", key: "Tab", windowsVirtualKeyCode: 9 },
  Escape: { code: "Escape", key: "Escape", windowsVirtualKeyCode: 27 },
  Backspace: { code: "Backspace", key: "Backspace", windowsVirtualKeyCode: 8 },
  Delete: { code: "Delete", key: "Delete", windowsVirtualKeyCode: 46 },
  ArrowUp: { code: "ArrowUp", key: "ArrowUp", windowsVirtualKeyCode: 38 },
  ArrowDown: { code: "ArrowDown", key: "ArrowDown", windowsVirtualKeyCode: 40 },
  ArrowLeft: { code: "ArrowLeft", key: "ArrowLeft", windowsVirtualKeyCode: 37 },
  ArrowRight: { code: "ArrowRight", key: "ArrowRight", windowsVirtualKeyCode: 39 },
  Home: { code: "Home", key: "Home", windowsVirtualKeyCode: 36 },
  End: { code: "End", key: "End", windowsVirtualKeyCode: 35 },
  PageUp: { code: "PageUp", key: "PageUp", windowsVirtualKeyCode: 33 },
  PageDown: { code: "PageDown", key: "PageDown", windowsVirtualKeyCode: 34 },
  Space: { code: "Space", key: " ", text: " ", windowsVirtualKeyCode: 32 },
};

function modifiersBitmask(mods) {
  // Chromium CDP: 1=Alt, 2=Ctrl, 4=Meta, 8=Shift
  let m = 0;
  for (const k of mods || []) {
    const lower = k.toLowerCase();
    if (lower === "alt") m |= 1;
    else if (lower === "ctrl" || lower === "control") m |= 2;
    else if (lower === "meta" || lower === "cmd" || lower === "command") m |= 4;
    else if (lower === "shift") m |= 8;
  }
  return m;
}

async function tabsPressKey({ tabId, key, modifiers }) {
  await ensureAttached(tabId);
  const mods = modifiersBitmask(modifiers);
  // Mask Meta(4), Ctrl(2), Alt(1) — Shift(8) doesn't suppress text. If any
  // command-style modifier is held, drop the `text` field; otherwise Chromium
  // also dispatches a textInput event ("a") on top of the shortcut, so
  // Cmd+A ends up typing "a" instead of select-all.
  const isShortcut = (mods & (1 | 2 | 4)) !== 0;
  let def = KEY_MAP[key]
    || (key.length === 1
      ? { code: `Key${key.toUpperCase()}`, key, text: key, windowsVirtualKeyCode: key.toUpperCase().charCodeAt(0) }
      : null);
  if (!def) throw new Error(`unsupported key: ${key}`);
  if (isShortcut && "text" in def) {
    def = { ...def };
    delete def.text;
  }
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchKeyEvent", {
    type: isShortcut ? "rawKeyDown" : "keyDown", modifiers: mods, ...def,
  });
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchKeyEvent", {
    type: "keyUp", modifiers: mods, ...def,
  });
  return { ok: true };
}

async function tabsTypeText({ tabId, text, delayMs }) {
  await ensureAttached(tabId);
  const delay = Math.max(0, Math.min(500, delayMs ?? 0));
  for (const ch of text) {
    await chrome.debugger.sendCommand({ tabId }, "Input.insertText", { text: ch });
    if (delay) await new Promise((r) => setTimeout(r, delay));
  }
  return { ok: true, chars: text.length };
}

// Trusted submit via OS-level Cmd+Enter (Win/Linux: Ctrl+Enter). Focuses the
// composer in ISOLATED world, then dispatches a real CDP key event so
// `event.isTrusted === true`. X / Bluesky / most rich-text editors honor
// Cmd+Enter as a "send" shortcut, so this replaces a synthetic click on the
// Post button that would have isTrusted=false (the main signal X uses to
// detect automated posting).
async function tabsSubmitPost({ tabId, selector }) {
  if (selector) {
    const [focusRes] = await chrome.scripting.executeScript({
      target: { tabId }, world: "ISOLATED",
      func: (sel) => {
        const el = document.querySelector(sel);
        if (!el) throw new Error(`selector not found: ${sel}`);
        el.focus({ preventScroll: false });
        const rect = el.getBoundingClientRect();
        for (const t of ["mousedown", "mouseup", "click"]) {
          el.dispatchEvent(new MouseEvent(t, {
            bubbles: true, cancelable: true, view: window,
            clientX: rect.left + 10, clientY: rect.top + 10, button: 0,
          }));
        }
        return { ok: true, focused: document.activeElement === el || el.contains(document.activeElement) };
      },
      args: [selector],
    });
    if (!focusRes?.result?.focused) {
      throw new Error("submit_post: failed to focus composer");
    }
  }
  await ensureAttached(tabId);
  // Meta(4) on macOS, Ctrl(2) elsewhere — Cmd+Enter / Ctrl+Enter both work in
  // X's compose modal. We send Meta unconditionally; on Win/Linux Chrome the
  // event still maps to "submit" because X's handler accepts either modifier.
  const mods = 4;
  const keyDef = {
    key: "Enter",
    code: "Enter",
    windowsVirtualKeyCode: 13,
  };
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchKeyEvent", {
    type: "rawKeyDown", modifiers: mods, ...keyDef,
  });
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchKeyEvent", {
    type: "keyUp", modifiers: mods, ...keyDef,
  });
  return { ok: true };
}

// Trusted click — v0.53: dispatches the click via CDP Input.dispatchMouseEvent
// so `event.isTrusted === true`. Synthetic dispatchEvent(MouseEvent) was being
// dropped by X for sensitive actions (e.g. SideNav Post button silently
// changing URL without opening the modal).
//
// Step 1: resolve element center coords + paint cursor (ISOLATED world).
// Step 2: small settle so the user sees the cursor first.
// Step 3: real OS-level mousePressed + mouseReleased via debugger.
async function tabsClickNative({ tabId, selector }) {
  const [res] = await chrome.scripting.executeScript({
    target: { tabId }, world: "ISOLATED",
    func: (sel) => {
      const el = document.querySelector(sel);
      if (!el) throw new Error(`selector not found: ${sel}`);
      el.scrollIntoView({ block: "center", inline: "center" });
      const r = el.getBoundingClientRect();
      const x = r.left + r.width / 2;
      const y = r.top + r.height / 2;
      glanceShowCursor(x, y);
      return { ok: true, tag: el.tagName, x, y };

      function glanceShowCursor(x, y) {
        const ID = "__glance_cursor";
        let c = document.getElementById(ID);
        if (!c) {
          c = document.createElement("div");
          c.id = ID;
          c.style.cssText = "position:fixed;z-index:2147483647;pointer-events:none;width:24px;height:24px;top:0;left:0;transition:transform 200ms cubic-bezier(.2,.7,.3,1), opacity 200ms;opacity:0";
          c.innerHTML = `<svg viewBox="0 0 24 24" width="24" height="24" xmlns="http://www.w3.org/2000/svg"><path d="M3 2.5L20 12L11.5 13.5L9 21L3 2.5Z" fill="#7c3aed" stroke="white" stroke-width="1.2" stroke-linejoin="round"/></svg>`;
          document.documentElement.appendChild(c);
        }
        c.style.transform = `translate(${x}px, ${y}px)`;
        c.style.opacity = "1";
        clearTimeout(c.__glanceFadeT);
        c.__glanceFadeT = setTimeout(() => { c.style.opacity = "0"; }, 700);
      }
    },
    args: [selector],
  });
  if (!res?.result) throw new Error("click_native returned nothing");
  const { x, y, tag } = res.result;

  await new Promise((r) => setTimeout(r, 250));
  await ensureAttached(tabId);
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchMouseEvent", {
    type: "mousePressed", x, y, button: "left", clickCount: 1,
  });
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchMouseEvent", {
    type: "mouseReleased", x, y, button: "left", clickCount: 1,
  });
  return { ok: true, tag, x, y };
}

// Native fill — input/textarea value set via React-aware setter so frameworks
// pick up the change. ISOLATED world, no eval.
async function tabsFillNative({ tabId, selector, value }) {
  const [res] = await chrome.scripting.executeScript({
    target: { tabId }, world: "ISOLATED",
    func: (sel, val) => {
      const el = document.querySelector(sel);
      if (!el) throw new Error(`selector not found: ${sel}`);
      const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
      const setter = Object.getOwnPropertyDescriptor(proto, "value")?.set;
      if (setter) setter.call(el, val);
      else el.value = val;
      el.dispatchEvent(new Event("input",  { bubbles: true }));
      el.dispatchEvent(new Event("change", { bubbles: true }));
      return { ok: true, tag: el.tagName };
    },
    args: [selector, value],
  });
  if (!res?.result) throw new Error("fill_native returned nothing");
  return res.result;
}

// Insert multi-paragraph text into the focused element without touching the
// system clipboard and without needing Chrome to be the foreground OS window.
// Splits on \n\n, sends each block as a single CDP Input.insertText (which
// Draft.js / Lexical / ProseMirror sees as one trusted beforeinput with the
// full block as `data`), then dispatches Enter keys between blocks for proper
// paragraph break. 600 ms settle between blocks lets React commit before the
// next insertion arrives — that's what fixed the rapid-fire reorder bug from
// earlier attempts.
//
// Trade-off vs. os_paste: ~1 s slower per tweet, but zero OS focus needed.
async function tabsTypeMultiline({ tabId, selector, value, settleMs }) {
  const settle = Math.max(200, Math.min(2000, settleMs || 600));

  // 1. Focus the target element in ISOLATED world. No CDP focus is needed —
  //    Input.insertText routes to whatever element document.activeElement is.
  if (selector) {
    const [focusRes] = await chrome.scripting.executeScript({
      target: { tabId }, world: "ISOLATED",
      func: (sel) => {
        const el = document.querySelector(sel);
        if (!el) throw new Error(`selector not found: ${sel}`);
        el.scrollIntoView({ block: "center", inline: "center" });
        el.focus({ preventScroll: false });
        // Mouse click sequence to plant cursor inside the element.
        const r = el.getBoundingClientRect();
        for (const t of ["mousedown", "mouseup", "click"]) {
          el.dispatchEvent(new MouseEvent(t, {
            bubbles: true, cancelable: true, view: window,
            clientX: r.left + 10, clientY: r.top + 10, button: 0,
          }));
        }
        return { ok: true, focused: document.activeElement === el || el.contains(document.activeElement) };
      },
      args: [selector],
    });
    if (!focusRes?.result?.focused) {
      throw new Error("type_multiline: failed to focus selector");
    }
  }

  await ensureAttached(tabId);

  // 2. Split into paragraphs and insert one at a time.
  const blocks = String(value || "").split(/\n\n+/);
  for (let i = 0; i < blocks.length; i++) {
    if (i > 0) {
      // Two Enter keys = one blank line between paragraphs in most editors,
      // and Draft.js treats consecutive newline insertions as a fresh block.
      for (let j = 0; j < 2; j++) {
        await chrome.debugger.sendCommand({ tabId }, "Input.dispatchKeyEvent", {
          type: "rawKeyDown", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13,
        });
        await chrome.debugger.sendCommand({ tabId }, "Input.dispatchKeyEvent", {
          type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13,
        });
        await new Promise((r) => setTimeout(r, 100));
      }
    }
    const block = blocks[i];
    if (block.length === 0) continue;
    // Single-line newlines within a block also need Enter keys, not literal \n
    // in insertText (Draft.js otherwise inserts them as text glyphs).
    const lines = block.split(/\n/);
    for (let k = 0; k < lines.length; k++) {
      if (k > 0) {
        // Shift+Enter for a soft line break within a paragraph.
        await chrome.debugger.sendCommand({ tabId }, "Input.dispatchKeyEvent", {
          type: "rawKeyDown", modifiers: 8, key: "Enter", code: "Enter", windowsVirtualKeyCode: 13,
        });
        await chrome.debugger.sendCommand({ tabId }, "Input.dispatchKeyEvent", {
          type: "keyUp", modifiers: 8, key: "Enter", code: "Enter", windowsVirtualKeyCode: 13,
        });
        await new Promise((r) => setTimeout(r, 80));
      }
      if (lines[k].length > 0) {
        await chrome.debugger.sendCommand({ tabId }, "Input.insertText", { text: lines[k] });
      }
    }
    // Let Draft.js commit before next block fires.
    await new Promise((r) => setTimeout(r, settle));
  }

  return { ok: true, blocks: blocks.length };
}

// OS-level paste. Caller must put text in system clipboard FIRST (glance-mcp
// does this via pbcopy/xclip). Focuses the target element via ISOLATED world,
// then dispatches a CDP key event with `commands:["paste"]` so the browser
// fires a trusted paste event populated from the real clipboard. This is
// the only path that survives Draft.js / Lexical isTrusted checks for
// multi-paragraph rich text.
async function tabsPasteKeyboard({ tabId, selector }) {
  // 1. Focus the target via ISOLATED world.
  if (selector) {
    const [focusRes] = await chrome.scripting.executeScript({
      target: { tabId }, world: "ISOLATED",
      func: (sel) => {
        const el = document.querySelector(sel);
        if (!el) throw new Error(`selector not found: ${sel}`);
        el.focus({ preventScroll: false });
        const rect = el.getBoundingClientRect();
        // Mouse-click sequence to claim focus / set selection cursor.
        for (const t of ["mousedown", "mouseup", "click"]) {
          el.dispatchEvent(new MouseEvent(t, {
            bubbles: true, cancelable: true, view: window,
            clientX: rect.left + 10, clientY: rect.top + 10, button: 0,
          }));
        }
        return { ok: true, focused: document.activeElement === el || el.contains(document.activeElement) };
      },
      args: [selector],
    });
    if (!focusRes?.result?.focused) {
      throw new Error("paste_keyboard: failed to focus selector");
    }
  }
  // 2. Trigger paste via debugger Input.dispatchKeyEvent commands:["paste"].
  //    Modifier 4 = Meta (Cmd on macOS, Win on Windows/Linux).
  await ensureAttached(tabId);
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchKeyEvent", {
    type: "keyDown",
    modifiers: 4,
    key: "v",
    code: "KeyV",
    windowsVirtualKeyCode: 86,
    commands: ["paste"],
  });
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchKeyEvent", {
    type: "keyUp",
    modifiers: 4,
    key: "v",
    code: "KeyV",
    windowsVirtualKeyCode: 86,
  });
  // Brief settle so caller sees the post-paste DOM state.
  await new Promise((r) => setTimeout(r, 200));
  return { ok: true };
}

async function tabsHover({ tabId, selector }) {
  const [res] = await chrome.scripting.executeScript({
    target: { tabId }, world: "ISOLATED",
    func: async (sel) => {
      const el = document.querySelector(sel);
      if (!el) throw new Error(`selector not found: ${sel}`);
      el.scrollIntoView({ block: "center", inline: "center" });
      const r = el.getBoundingClientRect();
      const x = r.left + r.width / 2;
      const y = r.top + r.height / 2;
      glanceShowCursor(x, y);
      await new Promise((r) => setTimeout(r, 200));
      for (const t of ["pointerover", "mouseover", "mouseenter", "pointermove", "mousemove"]) {
        el.dispatchEvent(new MouseEvent(t, { bubbles: true, cancelable: true, view: window, clientX: x, clientY: y }));
      }
      return { ok: true, tag: el.tagName, x, y };

      function glanceShowCursor(x, y) {
        const ID = "__glance_cursor";
        let c = document.getElementById(ID);
        if (!c) {
          c = document.createElement("div");
          c.id = ID;
          c.style.cssText = "position:fixed;z-index:2147483647;pointer-events:none;width:24px;height:24px;top:0;left:0;transition:transform 200ms cubic-bezier(.2,.7,.3,1), opacity 200ms;opacity:0";
          c.innerHTML = `<svg viewBox="0 0 24 24" width="24" height="24" xmlns="http://www.w3.org/2000/svg"><path d="M3 2.5L20 12L11.5 13.5L9 21L3 2.5Z" fill="#7c3aed" stroke="white" stroke-width="1.2" stroke-linejoin="round"/></svg>`;
          document.documentElement.appendChild(c);
        }
        c.style.transform = `translate(${x}px, ${y}px)`;
        c.style.opacity = "1";
        clearTimeout(c.__glanceFadeT);
        c.__glanceFadeT = setTimeout(() => { c.style.opacity = "0"; }, 600);
      }
    },
    args: [selector],
  });
  if (!res?.result) throw new Error("hover script returned nothing");
  return res.result;
}

async function tabsNavigateBack({ tabId }) {
  await chrome.tabs.goBack(tabId);
  return { ok: true };
}
async function tabsNavigateForward({ tabId }) {
  await chrome.tabs.goForward(tabId);
  return { ok: true };
}

async function tabsSelectOption({ tabId, selector, value, label }) {
  const [res] = await chrome.scripting.executeScript({
    target: { tabId }, world: "ISOLATED",
    func: (sel, val, lab) => {
      const el = document.querySelector(sel);
      if (!el || el.tagName !== "SELECT") throw new Error(`<select> not found: ${sel}`);
      let matched = null;
      for (const opt of el.options) {
        if (val != null && opt.value === val) { matched = opt; break; }
        if (lab != null && opt.label === lab) { matched = opt; break; }
        if (lab != null && opt.text  === lab) { matched = opt; break; }
      }
      if (!matched) throw new Error(`no option matched value=${val} label=${lab}`);
      el.value = matched.value;
      el.dispatchEvent(new Event("input", { bubbles: true }));
      el.dispatchEvent(new Event("change", { bubbles: true }));
      return { ok: true, value: matched.value, text: matched.text };
    },
    args: [selector, value ?? null, label ?? null],
  });
  if (!res?.result) throw new Error("select script returned nothing");
  return res.result;
}

async function tabsFillForm({ tabId, fields }) {
  // fields: [{selector, value}, ...]
  const [res] = await chrome.scripting.executeScript({
    target: { tabId }, world: "ISOLATED",
    func: (rows) => {
      const out = [];
      for (const { selector, value } of rows) {
        const el = document.querySelector(selector);
        if (!el) { out.push({ selector, ok: false, error: "not found" }); continue; }
        const proto =
          el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
        const setter = Object.getOwnPropertyDescriptor(proto, "value")?.set;
        if (setter) setter.call(el, value);
        else el.value = value;
        el.dispatchEvent(new Event("input",  { bubbles: true }));
        el.dispatchEvent(new Event("change", { bubbles: true }));
        out.push({ selector, ok: true, tag: el.tagName });
      }
      return { ok: true, results: out };
    },
    args: [fields || []],
  });
  if (!res?.result) throw new Error("fill_form script returned nothing");
  return res.result;
}

async function tabsDrag({ tabId, fromSelector, toSelector }) {
  await ensureAttached(tabId);
  const [pos] = await chrome.scripting.executeScript({
    target: { tabId }, world: "ISOLATED",
    func: (a, b) => {
      const A = document.querySelector(a), B = document.querySelector(b);
      if (!A) throw new Error(`from not found: ${a}`);
      if (!B) throw new Error(`to not found: ${b}`);
      A.scrollIntoView({ block: "center" });
      const ra = A.getBoundingClientRect(), rb = B.getBoundingClientRect();
      return {
        from: { x: ra.left + ra.width / 2, y: ra.top + ra.height / 2 },
        to:   { x: rb.left + rb.width / 2, y: rb.top + rb.height / 2 },
      };
    },
    args: [fromSelector, toSelector],
  });
  if (!pos?.result) throw new Error("drag positions not resolved");
  const { from, to } = pos.result;
  const seq = [
    ["mousePressed", from],
    ["mouseMoved", { x: (from.x + to.x) / 2, y: (from.y + to.y) / 2 }],
    ["mouseMoved", to],
    ["mouseReleased", to],
  ];
  for (const [type, p] of seq) {
    await chrome.debugger.sendCommand({ tabId }, "Input.dispatchMouseEvent", {
      type, x: p.x, y: p.y, button: "left", clickCount: 1,
    });
  }
  return { ok: true, from, to };
}

async function tabsUploadFile({ tabId, selector, files }) {
  // CDP DOM.setFileInputFiles needs the backend node id of the <input type=file>.
  await ensureAttached(tabId);
  const doc = await chrome.debugger.sendCommand({ tabId }, "DOM.getDocument", {});
  const root = doc.root.nodeId;
  const q = await chrome.debugger.sendCommand({ tabId }, "DOM.querySelector", {
    nodeId: root, selector,
  });
  if (!q.nodeId) throw new Error(`selector not found: ${selector}`);
  await chrome.debugger.sendCommand({ tabId }, "DOM.setFileInputFiles", {
    files: files || [], nodeId: q.nodeId,
  });
  return { ok: true, files: files?.length || 0 };
}

async function tabsResize({ tabId, width, height }) {
  await ensureAttached(tabId);
  await chrome.debugger.sendCommand({ tabId }, "Emulation.setDeviceMetricsOverride", {
    width: width || 1280,
    height: height || 800,
    deviceScaleFactor: 1,
    mobile: false,
  });
  return { ok: true, width, height };
}

async function tabsEmulate({ tabId, viewport, network, userAgent, geolocation, timezone, colorScheme, cpuThrottling, clear }) {
  await ensureAttached(tabId);
  if (clear) {
    await chrome.debugger.sendCommand({ tabId }, "Emulation.clearDeviceMetricsOverride", {}).catch(() => {});
    await chrome.debugger.sendCommand({ tabId }, "Network.emulateNetworkConditions", {
      offline: false, latency: 0, downloadThroughput: -1, uploadThroughput: -1,
    }).catch(() => {});
    await chrome.debugger.sendCommand({ tabId }, "Network.setUserAgentOverride", { userAgent: "" }).catch(() => {});
    await chrome.debugger.sendCommand({ tabId }, "Emulation.clearGeolocationOverride", {}).catch(() => {});
    await chrome.debugger.sendCommand({ tabId }, "Emulation.setEmulatedMedia", { media: "", features: [] }).catch(() => {});
    await chrome.debugger.sendCommand({ tabId }, "Emulation.setCPUThrottlingRate", { rate: 1 }).catch(() => {});
    return { ok: true, cleared: true };
  }
  const applied = [];
  if (viewport) {
    await chrome.debugger.sendCommand({ tabId }, "Emulation.setDeviceMetricsOverride", {
      width: viewport.width || 1280,
      height: viewport.height || 800,
      deviceScaleFactor: viewport.deviceScaleFactor || 1,
      mobile: !!viewport.mobile,
    });
    applied.push("viewport");
  }
  if (network) {
    const presets = {
      offline:  { offline: true, latency: 0, downloadThroughput: 0, uploadThroughput: 0 },
      "slow-3g":{ offline: false, latency: 400, downloadThroughput: 500*1024/8, uploadThroughput: 500*1024/8 },
      "fast-3g":{ offline: false, latency: 150, downloadThroughput: 1.6*1024*1024/8, uploadThroughput: 750*1024/8 },
      "slow-4g":{ offline: false, latency: 150, downloadThroughput: 2.5*1024*1024/8, uploadThroughput: 1.5*1024*1024/8 },
      "fast-4g":{ offline: false, latency: 50,  downloadThroughput: 9*1024*1024/8,   uploadThroughput: 9*1024*1024/8 },
    };
    const cfg = typeof network === "string" ? presets[network] : network;
    if (!cfg) throw new Error(`unknown network preset: ${network}`);
    await chrome.debugger.sendCommand({ tabId }, "Network.emulateNetworkConditions", cfg);
    applied.push("network");
  }
  if (userAgent) {
    await chrome.debugger.sendCommand({ tabId }, "Network.setUserAgentOverride", { userAgent });
    applied.push("userAgent");
  }
  if (geolocation) {
    await chrome.debugger.sendCommand({ tabId }, "Emulation.setGeolocationOverride", {
      latitude: geolocation.latitude,
      longitude: geolocation.longitude,
      accuracy: geolocation.accuracy ?? 1,
    });
    applied.push("geolocation");
  }
  if (timezone) {
    await chrome.debugger.sendCommand({ tabId }, "Emulation.setTimezoneOverride", { timezoneId: timezone });
    applied.push("timezone");
  }
  if (colorScheme) {
    await chrome.debugger.sendCommand({ tabId }, "Emulation.setEmulatedMedia", {
      features: [{ name: "prefers-color-scheme", value: colorScheme }],
    });
    applied.push("colorScheme");
  }
  if (cpuThrottling) {
    await chrome.debugger.sendCommand({ tabId }, "Emulation.setCPUThrottlingRate", { rate: cpuThrottling });
    applied.push("cpuThrottling");
  }
  return { ok: true, applied };
}

// ----- tier-2: console + dialog -----

const consoleBuffers = new Map();   // tabId -> array of entries
const consoleEnabled = new Set();
const dialogPending  = new Map();   // tabId -> array of pending dialogs
const dialogEnabled  = new Set();

const CONSOLE_BUF_MAX = 500;

async function ensureConsoleEnabled(tabId) {
  await ensureAttached(tabId);
  if (consoleEnabled.has(tabId)) return;
  await chrome.debugger.sendCommand({ tabId }, "Runtime.enable", {});
  consoleEnabled.add(tabId);
  if (!consoleBuffers.has(tabId)) consoleBuffers.set(tabId, []);
}

async function ensureDialogEnabled(tabId) {
  await ensureAttached(tabId);
  if (dialogEnabled.has(tabId)) return;
  await chrome.debugger.sendCommand({ tabId }, "Page.enable", {});
  dialogEnabled.add(tabId);
  if (!dialogPending.has(tabId)) dialogPending.set(tabId, []);
}

chrome.debugger.onEvent.addListener((source, method, params) => {
  const tabId = source.tabId;
  if (tabId == null) return;
  if (consoleEnabled.has(tabId) && (method === "Runtime.consoleAPICalled" || method === "Runtime.exceptionThrown")) {
    const buf = consoleBuffers.get(tabId) || [];
    let entry;
    if (method === "Runtime.consoleAPICalled") {
      entry = {
        kind: "console",
        level: params.type || "log",
        ts: params.timestamp,
        args: (params.args || []).map((a) => a.value !== undefined ? a.value : (a.description || a.unserializableValue || a.type)),
        url: params.stackTrace?.callFrames?.[0]?.url,
        line: params.stackTrace?.callFrames?.[0]?.lineNumber,
      };
    } else {
      const ex = params.exceptionDetails || {};
      entry = {
        kind: "exception",
        level: "error",
        ts: params.timestamp,
        text: ex.text,
        error: ex.exception?.description || ex.exception?.value || "",
        url: ex.url,
        line: ex.lineNumber,
      };
    }
    buf.push(entry);
    while (buf.length > CONSOLE_BUF_MAX) buf.shift();
    consoleBuffers.set(tabId, buf);
  }
  if (dialogEnabled.has(tabId) && method === "Page.javascriptDialogOpening") {
    const arr = dialogPending.get(tabId) || [];
    arr.push({
      type: params.type, message: params.message, defaultPrompt: params.defaultPrompt, url: params.url,
      ts: Date.now(),
    });
    dialogPending.set(tabId, arr);
  }
});

async function consoleList({ tabId, level, contains, limit }) {
  await ensureConsoleEnabled(tabId);
  const buf = consoleBuffers.get(tabId) || [];
  let out = buf.slice();
  if (level) out = out.filter((e) => e.level === level);
  if (contains) out = out.filter((e) =>
    JSON.stringify(e.args || e.text || e.error || "").includes(contains));
  const max = Math.max(1, Math.min(500, limit || 100));
  out = out.slice(-max);
  return { tabId, count: out.length, total: buf.length, entries: out };
}

async function consoleClear({ tabId }) {
  consoleBuffers.set(tabId, []);
  return { ok: true };
}

async function dialogHandle({ tabId, accept, promptText }) {
  await ensureDialogEnabled(tabId);
  await chrome.debugger.sendCommand({ tabId }, "Page.handleJavaScriptDialog", {
    accept: !!accept, promptText: promptText || "",
  });
  // Pop the most-recent pending entry.
  const arr = dialogPending.get(tabId) || [];
  arr.pop();
  dialogPending.set(tabId, arr);
  return { ok: true };
}

async function dialogListPending({ tabId }) {
  await ensureDialogEnabled(tabId);
  return { tabId, pending: (dialogPending.get(tabId) || []).slice() };
}

// ----- tier-4: trace + heap -----

async function perfStartTrace({ tabId, categories }) {
  await ensureAttached(tabId);
  await chrome.debugger.sendCommand({ tabId }, "Tracing.start", {
    categories: categories || "devtools.timeline,disabled-by-default-devtools.timeline",
    transferMode: "ReturnAsStream",
  });
  return { ok: true, started: true };
}

async function perfStopTrace({ tabId }) {
  await ensureAttached(tabId);
  return new Promise((resolve, reject) => {
    let streamHandle;
    function onEvent(source, method, params) {
      if (source.tabId !== tabId) return;
      if (method === "Tracing.tracingComplete") {
        chrome.debugger.onEvent.removeListener(onEvent);
        streamHandle = params.stream;
        readStream(streamHandle).then(resolve, reject);
      }
    }
    chrome.debugger.onEvent.addListener(onEvent);
    chrome.debugger.sendCommand({ tabId }, "Tracing.end", {}).catch((e) => {
      chrome.debugger.onEvent.removeListener(onEvent);
      reject(e);
    });

    async function readStream(handle) {
      let body = "";
      while (true) {
        const r = await chrome.debugger.sendCommand({ tabId }, "IO.read", { handle, size: 1024 * 1024 });
        body += r.base64Encoded ? atob(r.data) : r.data;
        if (r.eof) break;
      }
      await chrome.debugger.sendCommand({ tabId }, "IO.close", { handle }).catch(() => {});
      return { ok: true, traceJson: body, sizeBytes: body.length };
    }
  });
}

async function perfHeapSnapshot({ tabId }) {
  await ensureAttached(tabId);
  return new Promise((resolve, reject) => {
    let chunks = [];
    function onEvent(source, method, params) {
      if (source.tabId !== tabId) return;
      if (method === "HeapProfiler.addHeapSnapshotChunk") {
        chunks.push(params.chunk);
      } else if (method === "HeapProfiler.reportHeapSnapshotProgress" && params.finished) {
        chrome.debugger.onEvent.removeListener(onEvent);
        const body = chunks.join("");
        resolve({ ok: true, snapshot: body, sizeBytes: body.length });
      }
    }
    chrome.debugger.onEvent.addListener(onEvent);
    chrome.debugger.sendCommand({ tabId }, "HeapProfiler.takeHeapSnapshot", { reportProgress: true })
      .then(() => {
        // some Chrome builds emit lastSeenObjectId as the "done" signal — fall back: assume after a small delay.
        setTimeout(() => {
          if (chunks.length) {
            chrome.debugger.onEvent.removeListener(onEvent);
            const body = chunks.join("");
            resolve({ ok: true, snapshot: body, sizeBytes: body.length });
          }
        }, 1500);
      })
      .catch((e) => {
        chrome.debugger.onEvent.removeListener(onEvent);
        reject(e);
      });
  });
}

// ----- keepalive -----

chrome.alarms.create("glance-keepalive", { periodInMinutes: 0.4 });
chrome.alarms.onAlarm.addListener((a) => {
  if (a.name === "glance-keepalive") {
    if (!port) connect();
    else send({ kind: "ping", ts: Date.now() });
  }
});

chrome.runtime.onStartup.addListener(() => connect());
chrome.runtime.onInstalled.addListener(() => connect());
connect();

// ----- popup status -----

chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg?.kind === "status") {
    sendResponse({ connected: !!port, host: HOST_NAME, error: lastError });
    return true;
  }
});
