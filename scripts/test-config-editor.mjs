#!/usr/bin/env node
// The Settings config.json editor: `pnpm test:config`.
//
// No runner and no devDependency, the same rule the client-page harnesses follow. Node
// strips the types out of the three modules under test on import -- `config-defaults.ts`,
// `config-schema.ts` and `json-structure.ts` are erasable-syntax-only and import nothing
// outside that set -- so what is checked is what the app ships, not a copy of it.
//
// It exists because of two defects that both reached the owner's machine at once, and it
// pins the three properties that would have caught them:
//
//   1. `stunUrls` was added to the config type and not to the schema, so the editor
//      reported the user's own config as invalid and refused to save it. Nothing failed:
//      not tsc, not eslint, not a build. => the schema and the type are cross-checked.
//   2. index.html gained an inline <style>, so Tauri added a nonce to `style-src`, which
//      per CSP voids the `'unsafe-inline'` monaco's runtime-injected theme stylesheet
//      needs. The editor lost every syntax colour, its gutter and its scrollbars, and
//      logged nothing. => index.html is asserted to carry no <style>, and the policy is
//      asserted to still say 'unsafe-inline'.
//   3. The masking that hides tokens is a column range over the scanned document; one
//      column too many and the key name goes under the mask too. => the spans are checked
//      against the exact characters of each secret.
import assert from 'node:assert/strict';
import { readFileSync, existsSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';

import { defaultConfig } from '../src/lib/config-defaults.ts';
import {
  configJsonSchema,
  validateConfig,
  blockingErrors,
  MASKED_CONFIG_PATHS,
} from '../src/lib/config-schema.ts';
import { normalizeConfig } from '../src/lib/config-normalize.ts';
import { scanJson, maskedSpans, isMaskedPath } from '../src/lib/json-structure.ts';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const read = name => readFileSync(resolve(root, name), 'utf8');

let failed = 0;
function test(what, run) {
  try {
    run();
    console.log('  ok   ' + what);
  } catch (e) {
    failed += 1;
    console.log('  FAIL ' + what + '\n       ' + (e && e.message));
  }
}

// The owner's own config.json, merged with the defaults exactly as `readStore` does, with
// every secret replaced by a dummy of the same shape. It is the document that could not be
// saved.
const fixture = read('scripts/fixtures/config-owner.json');
const fixtureConfig = JSON.parse(fixture);

const properties = configJsonSchema.properties;
const required = configJsonSchema.required;

console.log('=== config schema <-> config type ===');

test('every key the app persists is described by the schema', () => {
  const missing = Object.keys(defaultConfig).filter(key => !(key in properties));
  assert.deepEqual(missing, [], 'in defaultConfig but not in configJsonSchema.properties');
});

test('the schema describes no key the app does not persist', () => {
  const extra = Object.keys(properties).filter(key => !(key in defaultConfig));
  assert.deepEqual(extra, [], 'in configJsonSchema.properties but not in defaultConfig');
});

test('every persisted key is required, so a missing one is reported', () => {
  const optional = Object.keys(defaultConfig).filter(key => !required.includes(key));
  assert.deepEqual(optional, []);
});

// The nested shapes have no runtime value to enumerate -- `devices` defaults to [] -- so
// their members are read out of the type declarations themselves. A member line is
// `  name: type;` or `  name?: type;` at one level of indentation inside the block.
const membersOf = typeName => {
  const source = read('src/lib/config-defaults.ts');
  const start = source.indexOf('export type ' + typeName + ' = {');
  assert.notEqual(start, -1, 'type ' + typeName + ' not found');
  const end = source.indexOf('\n};', start);
  const body = source.slice(start, end);
  const names = [];
  for (const line of body.split(/\r?\n/).slice(1)) {
    const hit = /^ {2}([A-Za-z_][A-Za-z0-9_]*)\??:/.exec(line);
    if (hit) names.push(hit[1]);
  }
  assert.ok(names.length > 0, 'no members parsed out of ' + typeName);
  return names;
};

for (const [typeName, key] of [['Device', 'devices'], ['KnownDevice', 'knownDevices']]) {
  test(typeName + "'s members all appear in the schema", () => {
    const described = Object.keys(properties[key].items.properties);
    const missing = membersOf(typeName).filter(name => !described.includes(name));
    assert.deepEqual(missing, []);
  });
}

console.log('=== validation ===');

test('the defaults validate with nothing reported', () => {
  assert.deepEqual(validateConfig(defaultConfig), []);
});

test("the owner's config validates with nothing reported", () => {
  assert.deepEqual(validateConfig(fixtureConfig), []);
});

test('stunUrls specifically is accepted', () => {
  // The exact regression: a config carrying the default STUN list was rejected as
  // "Property stunUrls is not allowed".
  assert.equal('stunUrls' in fixtureConfig, true, 'fixture lost its stunUrls key');
  const errors = validateConfig({ ...fixtureConfig, stunUrls: 'stun:stun.l.google.com:19302' });
  assert.deepEqual(errors, []);
});

test('an empty stunUrls is accepted, a nonsense one is not', () => {
  assert.deepEqual(validateConfig({ ...fixtureConfig, stunUrls: '' }), []);
  assert.equal(blockingErrors(validateConfig({ ...fixtureConfig, stunUrls: 'http://x' })).length, 1);
});

test('an unknown key is a warning, and the document still saves', () => {
  const found = validateConfig({ ...fixtureConfig, somethingNew: 1 });
  assert.equal(found.length, 1);
  assert.equal(found[0].severity, 'warning');
  assert.deepEqual(blockingErrors(found), []);
});

test('an unknown dontShowAgain flag is a warning too', () => {
  const dontShowAgain = { ...fixtureConfig.dontShowAgain, someFutureDialog: true };
  const found = validateConfig({ ...fixtureConfig, dontShowAgain });
  assert.deepEqual(blockingErrors(found), []);
});

test('a warning does not swallow the cross-field checks after it', () => {
  const found = validateConfig({
    ...fixtureConfig,
    somethingNew: 1,
    serverPorts: { http: 8080, https: 8080 },
  });
  const blocking = blockingErrors(found);
  assert.equal(blocking.length, 1);
  assert.match(blocking[0].message, /ports must differ/);
});

test('real errors still block', () => {
  const cases = [
    [{ theme: 'chartreuse' }, /is not one of/],
    [{ zoomFactor: 9 }, /above the maximum/],
    [{ serverPorts: { http: 8080, https: 8080 } }, /ports must differ/],
    [{ disconnectGraceSecs: 10000 }, /above the maximum/],
    [{ turnConfig: { urls: 'nope', username: '', credential: '' } }, /turn:/],
  ];
  for (const [patch, expected] of cases) {
    const blocking = blockingErrors(validateConfig({ ...fixtureConfig, ...patch }));
    assert.ok(blocking.length > 0, JSON.stringify(patch) + ' should block');
    assert.ok(
      blocking.some(e => expected.test(e.message)),
      JSON.stringify(patch) + ' -> ' + blocking.map(e => e.message).join(' | ')
    );
  }
});

test('a missing required key is reported rather than silently defaulted', () => {
  const withoutStun = { ...fixtureConfig };
  delete withoutStun.stunUrls;
  const blocking = blockingErrors(validateConfig(withoutStun));
  assert.equal(blocking.length, 1);
  assert.match(blocking[0].message, /missing required key "stunUrls"/);
});

console.log('=== secret masking ===');

const lines = fixture.split('\n');
const spans = maskedSpans(scanJson(fixture), MASKED_CONFIG_PATHS);
// A span is (line, startColumn, endColumn), 1-based and half-open, over the characters
// between the quotes.
const covered = span => lines[span.line - 1].slice(span.startColumn - 1, span.endColumn - 1);

test('every secret in the document is masked, and nothing else is', () => {
  const paths = spans.map(s => s.path).sort();
  assert.deepEqual(paths, [
    'devices[0].token',
    'devices[1].token',
    'hostedNetworkCredentials.password',
    'knownDevices[0].token',
    'knownDevices[1].token',
    'knownDevices[2].token',
    'knownDevices[3].token',
    'knownDevices[4].token',
    'knownDevices[5].token',
    'knownDevices[6].token',
    'knownDevices[7].token',
    'knownDevices[8].token',
    'turnConfig.credential',
  ]);
});

test('a mask covers the value exactly -- no quote, no comma, no key name', () => {
  for (const span of spans) {
    const text = covered(span);
    const line = lines[span.line - 1];
    assert.ok(text.length > 0, span.path + ': masked nothing');
    assert.ok(!/["',]/.test(text), span.path + ': mask reaches past the value -> ' + text);
    assert.ok(line.includes('"' + text + '"'), span.path + ': ' + text + ' not in ' + line);
    // The key name must stay legible: a mask that swallowed it is the blank line the
    // owner saw.
    const key = /^\s*"([^"]+)":/.exec(line)[1];
    assert.ok(
      span.startColumn > line.indexOf('"' + key + '"') + key.length + 2,
      span.path + ': the mask starts inside the key name'
    );
  }
});

test('each token is masked at its own line, and the tokens differ', () => {
  const tokens = spans.filter(s => s.path.endsWith('.token')).map(covered);
  assert.equal(tokens.length, 11);
  for (const token of tokens) assert.match(token, /^[0-9a-f]{32}$/);
});

test('the hosted-network password and the TURN credential are masked', () => {
  const byPath = Object.fromEntries(spans.map(s => [s.path, covered(s)]));
  assert.equal(byPath['hostedNetworkCredentials.password'], 'Xk8pQm2ZaB7c');
  assert.equal(byPath['turnConfig.credential'], 's3cr3t-cred');
});

test('an empty secret produces no span rather than a zero-width one', () => {
  const empty = '{\n  "turnConfig": {\n    "credential": ""\n  }\n}\n';
  assert.deepEqual(maskedSpans(scanJson(empty), MASKED_CONFIG_PATHS), []);
});

test('the array wildcard matches any index and nothing else', () => {
  assert.equal(isMaskedPath('devices[0].token', MASKED_CONFIG_PATHS), true);
  assert.equal(isMaskedPath('devices[12].token', MASKED_CONFIG_PATHS), true);
  assert.equal(isMaskedPath('devices[0].ip', MASKED_CONFIG_PATHS), false);
  assert.equal(isMaskedPath('turnConfig.username', MASKED_CONFIG_PATHS), false);
});

test('a value carrying every base64url and URL character is scanned whole', () => {
  // The scanner walks the raw text, so a value that looks like structure is the thing
  // that would break it and leave a mask over the wrong columns.
  const secret = 'a+/=-_.~:@!$&()*,;[]{}"\\\\x';
  const doc = '{\n  "turnConfig": {\n    "credential": ' + JSON.stringify(secret) + '\n  }\n}\n';
  const [span] = maskedSpans(scanJson(doc), MASKED_CONFIG_PATHS);
  assert.ok(span, 'no span for an awkward credential');
  const line = doc.split('\n')[2];
  assert.equal(line.slice(span.startColumn - 1, span.endColumn - 1), JSON.stringify(secret).slice(1, -1));
});

console.log('=== the editor\'s stylesheets survive the page CSP ===');

// The comment in index.html says the word, at length, so that the next person does not
// re-add the element; strip comments before looking for the element itself.
const hasStyleElement = html => /<style[\s>]/i.test(html.replace(/<!--[\s\S]*?-->/g, ''));

test('index.html carries no <style> element', () => {
  // Tauri nonces `style-src` as soon as it finds one, and a nonce makes CSP ignore
  // 'unsafe-inline' -- which silently drops monaco's runtime-injected theme sheet.
  assert.equal(hasStyleElement(read('index.html')), false);
});

test('the built page carries no <style> element either', () => {
  if (!existsSync(resolve(root, 'dist/index.html'))) {
    console.log('       (dist/index.html absent -- run pnpm build for the stronger check)');
    return;
  }
  assert.equal(hasStyleElement(read('dist/index.html')), false);
});

test("the policy still says style-src 'unsafe-inline'", () => {
  const csp = JSON.parse(read('src-tauri/tauri.conf.json')).app.security.csp;
  const styleSrc = csp.split(';').map(p => p.trim()).find(p => p.startsWith('style-src'));
  assert.ok(styleSrc, 'no style-src in the CSP');
  assert.ok(styleSrc.includes("'unsafe-inline'"), styleSrc);
  assert.equal(styleSrc.includes('nonce-'), false, 'a nonce here would void unsafe-inline');
});

test("monaco's stylesheet ships as its own asset, not as injected CSS", () => {
  // Monaco's ~60 KB of layout CSS reaches the page as a <link> emitted beside the lazy
  // editor chunk. A `cssCodeSplit: false` or a raised `assetsInlineLimit` would turn it
  // into an injected <style> instead, which walks into the same CSP trap from the other
  // side -- and, unlike the theme sheet, that one takes the editor's whole layout with it.
  const assets = resolve(root, 'dist/assets');
  if (!existsSync(assets)) {
    console.log('       (no dist/assets -- run pnpm build for this check)');
    return;
  }
  const css = readdirSync(assets).filter(name => name.endsWith('.css'));
  assert.ok(
    css.some(name => name.startsWith('monaco-setup-')),
    'no monaco stylesheet in dist/assets: ' + css.join(', ')
  );
});

console.log('=== a malformed config.json is survivable ===');

// The defect, exactly as it reached the owner. `config.json` held
//
//     "devices": {"0": {"refreshRate": 30}}
//
// -- an object where the type says array, which is what a dotted CLI write produced when the
// intermediate array did not exist. The bootstrap's saved-settings replay does
// `for (const d of config.devices ?? [])`, which throws `TypeError: object is not iterable`;
// the throw was caught as a setup failure and the user was told his drivers were missing.
//
// Two properties are pinned here: the raw value is genuinely un-iterable (so this is the real
// defect and not a paraphrase of it), and the normalised value is iterable and says so in the
// log.
const BROKEN_DEVICES = { '0': { refreshRate: 30 } };

// What `bootstrap.tsx` does with the value, reduced to the one line that threw.
const replayDevices = config => {
  const seen = [];
  for (const device of config.devices ?? []) seen.push(device);
  return seen;
};

test('the raw value really is the one that threw', () => {
  assert.throws(
    () => replayDevices({ devices: BROKEN_DEVICES }),
    /is not iterable/,
    'the fixture no longer reproduces the defect'
  );
});

test('after normalisation the replay runs and the dashboard is reachable', () => {
  const { config, notes, repaired } = normalizeConfig({ ...fixtureConfig, devices: BROKEN_DEVICES });
  assert.deepEqual(replayDevices(config), []);
  assert.deepEqual(config.devices, []);
  assert.deepEqual(notes, ['[migrate] devices is not an array (an object with 1 key); ignoring it']);
  assert.deepEqual(repaired, ['devices']);
  // Nothing else was touched: one malformed key costs only itself.
  assert.equal(config.stunUrls, fixtureConfig.stunUrls);
  assert.deepEqual(config.serverPorts, fixtureConfig.serverPorts);
  assert.equal(config.knownDevices.length, fixtureConfig.knownDevices.length);
});

test('every top-level key malformed at once still yields the declared shape', () => {
  // The bootstrap has to reach the dashboard with a store in which nothing is the right type.
  const wrecked = {};
  for (const key of Object.keys(defaultConfig)) {
    // Deliberately the wrong container for each: an object where an array belongs, an array
    // where an object belongs, a string where a number belongs, and so on.
    const declared = properties[key].type;
    wrecked[key] =
      declared === 'array' ? { '0': {} }
      : declared === 'object' ? ['nope']
      : declared === 'string' ? 42
      : declared === 'boolean' ? 'yes'
      : 'not a number';
  }
  const { config, notes, repaired } = normalizeConfig(wrecked);
  assert.equal(notes.length, Object.keys(defaultConfig).length, notes.join('\n'));
  assert.equal(repaired.length, Object.keys(defaultConfig).length);
  assert.deepEqual(config, defaultConfig);
  // And the document that comes out is one the editor accepts, which is the definition of "the
  // declared shape" that matters to the user.
  assert.deepEqual(validateConfig(config), []);
});

test('null, a missing key and a wrong scalar all read as absent', () => {
  for (const stored of [null, undefined, 'nope', [], 7]) {
    const { config } = normalizeConfig({ ...fixtureConfig, serverPorts: stored });
    assert.deepEqual(config.serverPorts, defaultConfig.serverPorts, JSON.stringify(stored));
  }
  // A *missing* key is not a repair: filling it in memory must not rewrite everyone's file.
  const absent = { ...fixtureConfig };
  delete absent.stunUrls;
  const { config, notes, repaired } = normalizeConfig(absent);
  assert.equal(config.stunUrls, defaultConfig.stunUrls);
  assert.deepEqual(notes, []);
  assert.deepEqual(repaired, []);
});

test('one bad member of an object does not cost the others', () => {
  const { config, notes } = normalizeConfig({
    ...fixtureConfig,
    serverPorts: { http: 9000, https: 'nope' },
  });
  assert.equal(config.serverPorts.http, 9000);
  assert.equal(config.serverPorts.https, defaultConfig.serverPorts.https);
  assert.deepEqual(notes, [
    '[migrate] serverPorts.https is not a whole number (a string); ignoring it',
  ]);
});

test('one bad row does not cost the rest of the list', () => {
  const rows = [
    { ...fixtureConfig.devices[0], refreshRate: 'fast' },
    'not a device',
    fixtureConfig.devices[1],
  ];
  const { config, notes } = normalizeConfig({ ...fixtureConfig, devices: rows });
  assert.equal(config.devices.length, 2, 'the unusable row goes, the others stay');
  assert.equal(config.devices[0].refreshRate, 60, 'the bad field falls back to its default');
  assert.equal(config.devices[0].token, fixtureConfig.devices[0].token);
  assert.equal(config.devices[1].token, fixtureConfig.devices[1].token);
  assert.deepEqual(notes, [
    '[migrate] devices[0].refreshRate is not a whole number (a string); ignoring it',
    '[migrate] devices[1] is not an object (a string); dropping it',
  ]);
});

test('a value outside its enum is replaced rather than passed on', () => {
  const { config, notes } = normalizeConfig({ ...fixtureConfig, theme: 'chartreuse' });
  assert.equal(config.theme, defaultConfig.theme);
  assert.match(notes[0], /^\[migrate\] theme is not one of /);
});

test('a device row missing fields comes out complete, so the replay can hand it to the host', () => {
  // `bootstrap.tsx` passes `device.scale` and friends straight to `setDeviceOverride`, where an
  // `undefined` is rejected at the IPC boundary -- a saved row written by an older version is
  // the same class of startup failure as the wrong container type.
  const { config, notes, repaired } = normalizeConfig({
    ...fixtureConfig,
    devices: [{ ip: '10.0.0.5', token: 'a'.repeat(32) }],
  });
  const device = config.devices[0];
  for (const key of Object.keys(properties.devices.items.properties)) {
    assert.notEqual(device[key], undefined, key + ' was left undefined');
  }
  assert.equal(device.scale, 100);
  assert.equal(device.refreshRate, 60);
  assert.equal(device.pointerScope, 'all');
  assert.equal(device.remoteControl, false);
  // Filling a gap is not a repair, so the file is left alone.
  assert.deepEqual(notes, []);
  assert.deepEqual(repaired, []);
});

test('keys the schema does not name are kept, not deleted', () => {
  // `config.json` is written by three parties; a key one of them does not know about is inert,
  // and quietly deleting it would lose whichever of them does know.
  const { config } = normalizeConfig({
    ...fixtureConfig,
    devices: [{ ...fixtureConfig.devices[0], someFutureField: 'keep me' }],
  });
  assert.equal(config.devices[0].someFutureField, 'keep me');
});

test("the owner's own config survives normalisation unchanged", () => {
  // The normaliser runs on every read, so a well-formed document may not be edited by it. The
  // one thing it adds is a value for a declared field the document does not carry at all --
  // `pointerScope` and `autoFrameRate` post-date the rows in this fixture -- and that is what
  // makes the replay able to hand every row to the host without an `undefined` in it.
  const { config, notes, repaired } = normalizeConfig(fixtureConfig);
  assert.deepEqual(notes, []);
  assert.deepEqual(repaired, [], 'a valid document must never be rewritten');
  assert.deepEqual(validateConfig(config), []);
  const unchanged = (was, now, path) => {
    if (was === null || typeof was !== 'object') {
      assert.deepEqual(now, was, path + ' changed under normalisation');
      return;
    }
    if (Array.isArray(was)) {
      assert.equal(now.length, was.length, path + ' lost or gained a row');
      was.forEach((item, i) => unchanged(item, now[i], path + '[' + i + ']'));
      return;
    }
    for (const [key, value] of Object.entries(was)) unchanged(value, now[key], path + '.' + key);
  };
  for (const key of Object.keys(defaultConfig)) unchanged(fixtureConfig[key], config[key], key);
});

console.log('=== the Rust writer agrees about what is an array ===');

// `cli.rs` carries the container half of this schema, because that is the half a *writer* has to
// enforce: `config set devices.0.refreshRate 30` used to create an object for every missing
// segment, numeric or not, which is where `{"0": {...}}` came from. Two tables mean two things
// to drift, so the Rust one is parsed out of the source here and checked against this schema --
// the same trick `membersOf` above plays on `config-defaults.ts`.
const rustSource = read('src-tauri/src/cli.rs');

const rustShapes = (() => {
  const fields = new Map();
  for (const m of rustSource.matchAll(/const ([A-Z0-9_]+): &\[\(&str, &Shape\)\] =\s*&\[([\s\S]*?)\];/g)) {
    fields.set(m[1], [...m[2].matchAll(/\("([A-Za-z0-9_]+)",\s*([A-Z0-9_]+)\)/g)].map(e => [e[1], e[2]]));
  }
  const shapes = new Map();
  for (const m of rustSource.matchAll(/const ([A-Z0-9_]+): &Shape = &Shape::(Str|Int|Num|Bool);/g)) {
    shapes.set(m[1], { kind: { Str: 'string', Int: 'integer', Num: 'number', Bool: 'boolean' }[m[2]] });
  }
  for (const m of rustSource.matchAll(/const ([A-Z0-9_]+): &Shape = &Shape::Object\(([A-Z0-9_]+)\);/g)) {
    shapes.set(m[1], { kind: 'object', fields: m[2] });
  }
  for (const m of rustSource.matchAll(/const ([A-Z0-9_]+): &Shape = &Shape::Array\(([A-Z0-9_]+)\);/g)) {
    shapes.set(m[1], { kind: 'array', item: m[2] });
  }
  return { fields, shapes };
})();

test("cli.rs's shape table parses", () => {
  assert.ok(rustShapes.shapes.has('CONFIG'), 'no CONFIG shape found in cli.rs');
  assert.ok(rustShapes.fields.has('CONFIG_FIELDS'), 'no CONFIG_FIELDS table found in cli.rs');
  assert.ok(rustShapes.fields.get('CONFIG_FIELDS').length > 10, 'CONFIG_FIELDS came out nearly empty');
});

test('the Rust table and the editor schema declare the same container for every key', () => {
  const problems = [];
  const walk = (shapeName, schema, path) => {
    const shape = rustShapes.shapes.get(shapeName);
    if (!shape) {
      problems.push(path + ': cli.rs has no shape named ' + shapeName);
      return;
    }
    const declared = schema.type === 'integer' ? 'integer' : schema.type;
    if (shape.kind !== declared) {
      problems.push(`${path}: cli.rs says ${shape.kind}, the schema says ${declared}`);
      return;
    }
    if (shape.kind === 'array') {
      walk(shape.item, schema.items, path + '[]');
      return;
    }
    if (shape.kind !== 'object') return;
    const rust = new Map(rustShapes.fields.get(shape.fields) ?? []);
    const described = schema.properties ?? {};
    for (const key of Object.keys(described)) {
      if (!rust.has(key)) problems.push(`${path}.${key}: in the schema, missing from cli.rs`);
    }
    for (const key of rust.keys()) {
      if (!(key in described)) problems.push(`${path}.${key}: in cli.rs, missing from the schema`);
    }
    for (const [key, child] of rust) {
      if (described[key]) walk(child, described[key], path + '.' + key);
    }
  };
  walk('CONFIG', configJsonSchema, 'config');
  assert.deepEqual(problems, []);
});

console.log(failed === 0 ? '\nall config editor checks passed' : '\n' + failed + ' failed');
process.exit(failed === 0 ? 0 : 1);
