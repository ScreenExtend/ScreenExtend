#!/usr/bin/env node
// The startup gate's decision table: `pnpm test:gate`.
//
// No runner and no devDependency, the same rule the client-page harnesses follow. Node strips
// the types out of `src/lib/startup-gate.ts` on import (it is erasable-syntax-only and its one
// import is `import type`), so the function under test is the one the app ships rather than a
// copy that can drift from it.
//
// What it pins is the owner's rule: a host that can create a display and send video starts,
// whatever else is missing, and only a host that cannot — after the app has tried to install
// what it can install — loses "Continue anyway".
import assert from 'node:assert/strict';
import { classifyCompat, driverRemediation, offersDriverInstall } from '../src/lib/startup-gate.ts';

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

const row = (key, severity) => ({
  key,
  severity,
  name: key,
  description: key,
  required_version: 'test',
});

const report = (rows, os_supported = true) => ({
  os_name: 'Windows',
  os_version: 'test',
  min_os_version: 'test',
  os_supported,
  unsupported_apis: rows,
  audio_backend: 'wasapi',
  display_backend: 'IddDriver',
  capture_backend: 'WindowsGraphicsCapture',
  encoder_backend: 'nvenc',
  hosted_network_backend: 'WiFiDirect',
  input_backend: 'SendInput',
});

const fresh = { remediated: false, dontShowAgain: false };

console.log('=== startup gate ===');

test('a healthy host starts with no dialog', () => {
  assert.equal(classifyCompat(report([]), fresh).kind, 'ok');
});

test('every optional fault gets "Continue anyway", never a block', () => {
  const optional = [
    'capture-preferred',
    'audio',
    'hosted-network',
    'wifi-radios',
    'wmi',
    'gpu-adapter',
    'input-accessibility',
    'display-capped',
  ].map(k => row(k, 'optional'));
  for (const one of optional) {
    const gate = classifyCompat(report([one]), fresh);
    assert.equal(gate.kind, 'warn', one.key + ' produced ' + gate.kind);
  }
  // And all of them at once is still a host that streams.
  assert.equal(classifyCompat(report(optional), fresh).kind, 'warn');
});

test('"do not show again" starts silently on an optional fault', () => {
  const gate = classifyCompat(report([row('audio', 'optional')]), {
    remediated: false,
    dontShowAgain: true,
  });
  assert.equal(gate.kind, 'ok');
});

test('"do not show again" never suppresses a blocking one', () => {
  const gate = classifyCompat(report([row('capture', 'blocking')]), {
    remediated: false,
    dontShowAgain: true,
  });
  assert.equal(gate.kind, 'blocked');
});

test('an installable row is remediated before anything is decided', () => {
  const gate = classifyCompat(report([row('display-driver', 'installable')]), fresh);
  assert.equal(gate.kind, 'remediate');
  assert.deepEqual(gate.rows.map(r => r.key), ['display-driver']);
});

test('an installable row is remediated even alongside optional faults', () => {
  const gate = classifyCompat(
    report([row('audio', 'optional'), row('display-driver-unresponsive', 'installable')]),
    fresh
  );
  assert.equal(gate.kind, 'remediate');
});

test('an installable row that survives remediation is the one dead end', () => {
  const gate = classifyCompat(report([row('display-driver', 'installable')]), {
    remediated: true,
    dontShowAgain: false,
  });
  assert.equal(gate.kind, 'blocked');
  // The user is shown the row this time; while it was still ours to fix, it was hidden.
  assert.deepEqual(gate.rows.map(r => r.key), ['display-driver']);
});

test('a driver installed successfully leaves an optional fault as a warning', () => {
  const gate = classifyCompat(report([row('audio', 'optional')]), {
    remediated: true,
    dontShowAgain: false,
  });
  assert.equal(gate.kind, 'warn');
});

test('no capture backend at all blocks', () => {
  assert.equal(classifyCompat(report([row('capture', 'blocking')]), fresh).kind, 'blocked');
});

test('an unsupported OS blocks even with an empty list', () => {
  assert.equal(classifyCompat(report([], false), fresh).kind, 'blocked');
});

console.log('=== driver remediation ===');

test('a silent driver is reinstalled, a missing one installed', () => {
  assert.equal(driverRemediation([row('display-driver-unresponsive', 'installable')]), 'reinstall');
  assert.equal(driverRemediation([row('display-driver', 'installable')]), 'install');
  // Both at once cannot happen today, but reinstall is the superset if it ever does.
  assert.equal(
    driverRemediation([row('display-driver', 'installable'), row('display-driver-unresponsive', 'installable')]),
    'reinstall'
  );
  assert.equal(driverRemediation([row('audio', 'optional')]), null);
  assert.equal(driverRemediation([]), null);
});

test('"Install drivers" is offered only where there is a driver to install', () => {
  assert.equal(offersDriverInstall(null), false);
  assert.equal(offersDriverInstall(report([])), false);
  // A macOS report: no driver rows at all, so the control is not shown -- the bug item 7 named.
  assert.equal(offersDriverInstall(report([row('input-accessibility', 'optional')])), false);
  assert.equal(offersDriverInstall(report([row('display-driver', 'installable')])), true);
  assert.equal(offersDriverInstall(report([row('display-driver-unresponsive', 'installable')])), true);
});

console.log(failed === 0 ? '\nall startup-gate assertions passed' : `\n${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
