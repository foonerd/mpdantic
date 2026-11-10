'use strict';
// The plugin's pure parts: the port as typed, the settings the core reads, the status
// summary.
const test = require('node:test');
const assert = require('node:assert/strict');
const h = require('../helpers');

const t = (k, v) => k + (v ? ' ' + JSON.stringify(v) : '');

test('a port is an integer from 1024 to 65535, as typed', () => {
    assert.deepEqual(h.validatePort('8030'), { ok: true, value: 8030 });
    assert.deepEqual(h.validatePort(' 1024 '), { ok: true, value: 1024 });
    assert.deepEqual(h.validatePort(65535), { ok: true, value: 65535 });
    assert.deepEqual(h.validatePort('1023'), { ok: false });
    assert.deepEqual(h.validatePort('65536'), { ok: false });
    assert.deepEqual(h.validatePort('80a'), { ok: false });
    assert.deepEqual(h.validatePort(''), { ok: false });
    assert.deepEqual(h.validatePort(undefined), { ok: false });
    assert.deepEqual(h.validatePort(null), { ok: false });
});

test('the settings carry the port, the default when the value is not one', () => {
    assert.deepEqual(h.settingsFrom(() => '8031'), { port: 8031 });
    assert.deepEqual(h.settingsFrom(() => 'nonsense'), { port: h.DEFAULT_PORT });
    assert.deepEqual(h.settingsFrom((k, d) => d), { port: h.DEFAULT_PORT });
});

test('the status summary says the version and the state, or that the core is down', () => {
    assert.equal(h.summarize({ version: '0.1.0', state: 'idle' }, t), 'STATUS_UP {"version":"0.1.0","state":"idle"}');
    assert.equal(h.summarize({}, t), 'STATUS_UP {"version":"?","state":"?"}');
    assert.equal(h.summarize(null, t), 'STATUS_DOWN');
    assert.equal(h.summarize('text', t), 'STATUS_DOWN');
});
