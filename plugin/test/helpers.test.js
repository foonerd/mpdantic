'use strict';
// The plugin's pure parts: the port and the address as typed, the settings the core
// reads, the player's address, the links onto the pages, the Avahi service, the
// status summary.
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

test('an address is empty, a host name or an IP address', () => {
    assert.deepEqual(h.validateAddress(''), { ok: true, value: '' });
    assert.deepEqual(h.validateAddress(undefined), { ok: true, value: '' });
    assert.deepEqual(h.validateAddress(' volumio.local '), { ok: true, value: 'volumio.local' });
    assert.deepEqual(h.validateAddress('192.168.1.20'), { ok: true, value: '192.168.1.20' });
    assert.deepEqual(h.validateAddress('[fe80::1]'), { ok: true, value: '[fe80::1]' });
    assert.deepEqual(h.validateAddress('my player'), { ok: false });
    assert.deepEqual(h.validateAddress('http://x'), { ok: false });
    assert.deepEqual(h.validateAddress('a"b'), { ok: false });
});

test('the settings carry the port, the default when the value is not one', () => {
    assert.deepEqual(h.settingsFrom(() => '8031'), { port: 8031 });
    assert.deepEqual(h.settingsFrom(() => 'nonsense'), { port: h.DEFAULT_PORT });
    assert.deepEqual(h.settingsFrom((k, d) => d), { port: h.DEFAULT_PORT });
});

test('the player address is the wired interface first, then wireless, then any', () => {
    const lo = [{ address: '127.0.0.1', family: 'IPv4', internal: true }];
    const eth = [{ address: '192.168.1.20', family: 'IPv4', internal: false }, { address: 'fe80::1', family: 'IPv6', internal: false }];
    const wlan = [{ address: '192.168.1.21', family: 4, internal: false }];
    const other = [{ address: '10.0.0.5', family: 'IPv4', internal: false }];
    assert.equal(h.lanAddress({ lo, wlan0: wlan, eth0: eth }), '192.168.1.20');
    assert.equal(h.lanAddress({ lo, wlan0: wlan, tun0: other }), '192.168.1.21');
    assert.equal(h.lanAddress({ lo, tun0: other }), '10.0.0.5');
    assert.equal(h.lanAddress({ lo }), null);
    assert.equal(h.lanAddress({}), null);
    assert.equal(h.lanAddress(undefined), null);
});

test('the links onto the pages: the tab URL, and here through the iframe page', () => {
    assert.deepEqual(h.pageLinks('192.168.1.20', 8030), {
        url: 'http://192.168.1.20:8030/',
        here: 'http://192.168.1.20/iframe-page/http:~2F~2F192.168.1.20:8030~2F'
    });
    assert.equal(h.pageLinks('volumio.local', '8031', '/about').url, 'http://volumio.local:8031/about');
    assert.equal(h.pageLinks('fe80::1', 8030).url, 'http://[fe80::1]:8030/');
    assert.equal(h.pageLinks('[fe80::1]', 8030).url, 'http://[fe80::1]:8030/');
});

test('the Avahi service names the player, HTTP on the port, the path', () => {
    const text = h.avahiService(8030);
    assert.ok(text.startsWith('<?xml version="1.0" standalone="no"?>\n<!DOCTYPE service-group SYSTEM "avahi-service.dtd">\n<service-group>\n'));
    assert.match(text, /<name replace-wildcards="yes">MPDantic on %h<\/name>/);
    assert.match(text, /<type>_http\._tcp<\/type>\n\s*<port>8030<\/port>\n\s*<txt-record>path=\/<\/txt-record>/);
    assert.ok(text.endsWith('</service-group>\n'));
});

test('the status summary says the port when up, the error when not, or that the core is down', () => {
    assert.equal(h.summarize({ version: '0.1.0', state: 'idle', port: 8030 }, t), 'STATUS_UP {"version":"0.1.0","port":"8030"}');
    assert.equal(h.summarize({ version: '0.1.0', state: 'error', error: 'port 8030: in use' }, t), 'STATUS_ERROR {"version":"0.1.0","error":"port 8030: in use"}');
    assert.equal(h.summarize({}, t), 'STATUS_UP {"version":"?","port":"?"}');
    assert.equal(h.summarize(null, t), 'STATUS_DOWN');
    assert.equal(h.summarize('text', t), 'STATUS_DOWN');
});
