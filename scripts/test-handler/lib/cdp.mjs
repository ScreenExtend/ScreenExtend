// Chrome DevTools Protocol plumbing: launch a throwaway Chrome, open a page, evaluate.
// Nothing here dispatches input: there is deliberately no wrapper for Input.* methods, and
// `send` refuses them so a request snippet cannot reach them either.
import { spawn } from 'node:child_process';
import fs from 'node:fs';

export const CHROME = process.env.CHROME || 'C:/Program Files/Google/Chrome/Application/chrome.exe';
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const FORBIDDEN = /^Input\./;

export async function connect(wsUrl) {
  const ws = new WebSocket(wsUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = () => rej(new Error('CDP websocket failed: ' + wsUrl)); });
  let id = 0; const pending = new Map(); const events = []; const hooks = [];
  let closed = false;
  ws.onclose = () => { closed = true; for (const p of pending.values()) p.rej(new Error('CDP closed')); pending.clear(); };
  ws.onmessage = (m) => {
    const d = JSON.parse(m.data);
    if (d.id && pending.has(d.id)) { const p = pending.get(d.id); pending.delete(d.id); d.error ? p.rej(new Error(d.error.message || JSON.stringify(d.error))) : p.res(d.result); }
    else if (d.method) { events.push(d); if (events.length > 20000) events.splice(0, 5000); hooks.forEach((h) => { try { h(d); } catch { /* observer */ } }); }
  };
  const send = (method, params = {}, sessionId, timeoutMs = 20000) => {
    if (FORBIDDEN.test(method)) return Promise.reject(new Error('refused: ' + method + ' would inject input'));
    if (closed) return Promise.reject(new Error('CDP closed'));
    return new Promise((res, rej) => {
      const my = ++id; pending.set(my, { res, rej });
      ws.send(JSON.stringify(sessionId ? { id: my, method, params, sessionId } : { id: my, method, params }));
      setTimeout(() => { if (pending.delete(my)) rej(new Error('CDP timeout: ' + method)); }, timeoutMs);
    });
  };
  return { events, send, on: (fn) => hooks.push(fn), close: () => { try { ws.close(); } catch { /* gone */ } }, get closed() { return closed; } };
}

/** Launch a fresh Chrome on `port` with `profile` (deleted first). */
export async function launchChrome({ port, profile, flags = [], headed = false }) {
  fs.rmSync(profile, { recursive: true, force: true });
  const args = [
    ...(headed ? ['--window-position=80,80'] : ['--headless=new']),
    `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`,
    '--ignore-certificate-errors', '--autoplay-policy=no-user-gesture-required',
    '--use-fake-device-for-media-stream', '--mute-audio', '--no-first-run', '--no-default-browser-check',
    '--disable-background-timer-throttling', '--disable-renderer-backgrounding', '--disable-backgrounding-occluded-windows',
    ...flags, 'about:blank',
  ];
  const proc = spawn(CHROME, args, { stdio: 'ignore' });
  let exited = false; proc.on('exit', () => { exited = true; });
  let ver = null;
  for (let i = 0; i < 75 && !ver && !exited; i++) {
    try { ver = await (await fetch(`http://127.0.0.1:${port}/json/version`)).json(); } catch { await sleep(200); }
  }
  if (!ver) { try { proc.kill(); } catch { /* gone */ } throw new Error('chrome debugging port never answered on ' + port); }
  const browser = await connect(ver.webSocketDebuggerUrl);
  return { proc, browser, version: ver.Browser, args, get exited() { return exited; } };
}

export async function newPage(port) {
  const tgt = await (await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: 'PUT' })).json();
  const page = await connect(tgt.webSocketDebuggerUrl);
  page.targetId = tgt.id;
  const ev = async (expr, { gesture = false, timeout = 20000 } = {}) => {
    const r = await page.send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true, userGesture: gesture }, undefined, timeout);
    if (r.exceptionDetails) throw new Error('page threw: ' + ((r.exceptionDetails.exception && r.exceptionDetails.exception.description) || r.exceptionDetails.text));
    return r.result.value;
  };
  page.ev = ev;
  // An unanswered beforeunload dialog hangs the run (TEST-WORKFLOW §5.6).
  page.dialogs = [];
  page.on((d) => { if (d.method === 'Page.javascriptDialogOpening') { page.dialogs.push(d.params.type + ': ' + d.params.message); page.send('Page.handleJavaScriptDialog', { accept: true }).catch(() => {}); } });
  return page;
}

export async function closeChrome(c) {
  if (!c) return;
  await c.browser.send('Browser.close').catch(() => {});
  for (let i = 0; i < 100 && !c.exited; i++) await sleep(100);
  if (!c.exited) { try { c.proc.kill(); } catch { /* gone */ } for (let i = 0; i < 30 && !c.exited; i++) await sleep(100); }
}
