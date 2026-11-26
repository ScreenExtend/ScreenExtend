// Chrome short-circuits a receiver's encoded transform when nothing is installed within one task
// of the transceiver being created. A transform assigned later still fires `rtctransform`, but
// every frame goes to the built-in decoder, the WebCodecs path paints nothing and the page's 5 s
// watchdog demotes the session to <video>.
//
// So in index.html nothing may yield between `addTransceiver('video'` and the
// `RTCRtpScriptTransform` assignment. Checked on the source with comments stripped.
import fs from 'node:fs';

const STATIC = process.argv.slice(2).filter((a) => a.charAt(0) !== '-')[0] || 'src-tauri/src/streamer/static';
const page = fs.readFileSync(STATIC + '/index.html', 'utf8');

let failures = 0;
const ok = (m) => console.log('ok  - ' + m);
const fail = (m) => { failures++; console.log('FAIL- ' + m); };

const from = page.indexOf("pc.addTransceiver('video'");
const to = page.indexOf('receiver.transform = new RTCRtpScriptTransform(', from);
if (from < 0 || to < 0) {
  fail('could not find the video transceiver and the script-transform assignment in index.html');
} else {
  ok('found the transceiver and the transform assignment');
  const span = page.slice(from, to)
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .split('\n').map((l) => l.replace(/\/\/.*$/, '')).join('\n');
  const yields = span.match(/\bawait\b|\.then\s*\(|\byield\b/g);
  if (yields) fail('the transform is installed after a yield (' + yields.join(', ') + '); Chrome will have short-circuited it');
  else ok('nothing yields between creating the video transceiver and installing its transform');

  // building the worker awaits, so it has to happen before the transceiver exists
  const before = page.slice(page.lastIndexOf('async function start(', from), from);
  if (/await makeTransformWorker\(\)/.test(before)) ok('the transform worker is built before the transceiver');
  else fail('the transform worker is not built before the video transceiver');
}

if (failures) { console.log(failures + ' failure(s)'); process.exit(1); }
console.log('all passed');
