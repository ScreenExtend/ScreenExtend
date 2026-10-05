// Read-only (and, in exclusive mode, cleanup) access to the virtual display driver's pipe.
// Wire format (src-tauri/src/windows_utils/driver_ipc/core.rs): one JSON message per write,
// terminated by 0x04; `"State"` asks for the monitor list, the reply is `{"State":[...]}`, and
// the driver also pushes `{"Changed":[...]}` events to every connected client.
import net from 'node:net';

const PIPE = '\\\\.\\pipe\\virtualdisplaydriver';
const EOF = 0x04;

function exchange(message, wantKey, timeoutMs = 4000) {
  return new Promise((resolve, reject) => {
    const sock = net.connect(PIPE);
    let buf = Buffer.alloc(0);
    const done = (err, val) => { clearTimeout(t); sock.destroy(); err ? reject(err) : resolve(val); };
    const t = setTimeout(() => done(new Error('driver pipe timeout')), timeoutMs);
    sock.on('error', (e) => done(e));
    sock.on('connect', () => {
      sock.write(Buffer.concat([Buffer.from(JSON.stringify(message)), Buffer.from([EOF])]));
      if (!wantKey) setTimeout(() => done(null, true), 300);
    });
    sock.on('data', (d) => {
      buf = Buffer.concat([buf, d]);
      let i;
      while ((i = buf.indexOf(EOF)) >= 0) {
        const msg = buf.subarray(0, i).toString('utf8'); buf = buf.subarray(i + 1);
        try { const o = JSON.parse(msg); if (wantKey && o[wantKey] !== undefined) return done(null, o[wantKey]); } catch { /* not ours */ }
      }
    });
  });
}

/** The driver's monitor list, or { error } when the pipe does not answer (driver busy creating). */
export async function driverState() {
  for (let attempt = 0; attempt < 3; attempt++) {
    try { return await exchange('State', 'State'); } catch (e) { if (attempt === 2) return { error: e.message }; await new Promise((r) => setTimeout(r, 1500)); }
  }
}

/** Remove specific monitor ids. Used only in exclusive mode, after a PID-scoped stop of a build
 *  that could not remove its own display. */
export async function driverRemove(ids) {
  return exchange({ Remove: ids }, null);
}
