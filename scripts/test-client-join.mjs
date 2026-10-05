// The join path of index.html: the load-time warm-up that keeps two first-use costs off the path
// between Connect and the offer. On a browser's first join the offer left 1.9-2.3 s after the
// click, nearly all of it the first `enumerateDevices()` and the first `new RTCPeerConnection()`
// (docs/overhaul-new/G07.md).
//
// Functions are lifted out of the page by name, as test-client-reconfig.mjs does.
//
//   node scripts/test-client-join.mjs [path/to/static]
import fs from 'node:fs';
import vm from 'node:vm';

const STATIC = process.argv[2] || 'src-tauri/src/streamer/static';
const page = fs.readFileSync(STATIC + '/index.html', 'utf8');

let failures = 0;
const ok = (m) => console.log('ok  - ' + m);
const fail = (m) => { failures++; console.log('FAIL- ' + m); };
const eq = (name, got, want) => (got === want ? ok(name + ' = ' + got) : fail(name + ' = ' + got + ', expected ' + want));

function takeConst(name) {
  const m = new RegExp('\\b(?:const|let)\\s+' + name + '\\s*=\\s*[^;]+;').exec(page);
  if (!m) throw new Error(name + ' not found in index.html');
  return m[0];
}
function takeFunction(name) {
  const at = page.search(new RegExp('(?:async\\s+)?function\\s+' + name + '\\s*\\('));
  if (at < 0) throw new Error('function ' + name + ' not found in index.html');
  const open = page.indexOf('{', at);
  let depth = 0;
  for (let i = open; i < page.length; i++) {
    if (page[i] === '{') depth++;
    else if (page[i] === '}' && --depth === 0) return page.slice(at, i + 1);
  }
  throw new Error('function ' + name + ' is unbalanced');
}

// ---- warmJoinPath
function runWarm(env) {
  const calls = [];
  const ctx = vm.createContext({ calls, noteSuppressed: () => calls.push('suppressed'), navigator: env.navigator, RTCPeerConnection: env.RTCPeerConnection });
  vm.runInContext(takeFunction('warmJoinPath') + '\nwarmJoinPath();', ctx);
  return calls;
}
{
  const calls = [];
  const navigator = {
    mediaDevices: {
      enumerateDevices() { calls.push('enumerate'); return Promise.resolve([]); },
      getUserMedia() { calls.push('getUserMedia'); return Promise.resolve(null); },
    },
    permissions: { query() { calls.push('permissions'); return Promise.resolve({ state: 'prompt' }); } },
  };
  function RTCPeerConnection() { calls.push('pc'); }
  RTCPeerConnection.prototype.close = function () { calls.push('close'); };
  runWarm({ navigator, RTCPeerConnection });
  eq('warm-up enumerates once', calls.filter((c) => c === 'enumerate').length, 1);
  eq('warm-up builds and closes one peer connection', calls.filter((c) => c === 'pc').length + '/' + calls.filter((c) => c === 'close').length, '1/1');
  eq('warm-up asks for nothing that can prompt', calls.filter((c) => c === 'getUserMedia' || c === 'permissions').length, 0);

  const r1 = runWarm({ navigator: {}, RTCPeerConnection: undefined });
  eq('warm-up on an engine with neither API', r1.length, 0);
  const r2 = runWarm({
    navigator: { mediaDevices: { enumerateDevices() { throw new Error('nope'); } } },
    RTCPeerConnection: function () { throw new Error('nope'); },
  });
  eq('warm-up swallows and reports throwing APIs', r2.filter((c) => c === 'suppressed').length, 2);
  let rejected = null;
  const r3calls = runWarm({
    navigator: { mediaDevices: { enumerateDevices() { rejected = Promise.reject(new Error('denied')); return rejected; } } },
    RTCPeerConnection: undefined,
  });
  await Promise.resolve(rejected).catch(() => {});
  await new Promise((r) => setTimeout(r, 0));
  eq('a rejected enumeration is reported, not thrown', r3calls.filter((c) => c === 'suppressed').length, 1);

  // Scheduled at load, after the task that parses the page, and never awaited by the join.
  if (/setTimeout\(warmJoinPath,\s*0\)/.test(page)) ok('warmJoinPath is scheduled at load');
  else fail('warmJoinPath is not scheduled at load');
  const submit = page.slice(page.indexOf("addEventListener('submit'"));
  if (!/warmJoinPath/.test(submit)) ok('the join does not wait for the warm-up');
  else fail('the submit handler references warmJoinPath');
}

if (failures) { console.log(failures + ' failure(s)'); process.exit(1); }
console.log('all client join assertions passed');
