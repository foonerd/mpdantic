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
    assert.equal(h.summarize({ version: '0.1.0', state: 'idle', port: 8030 }, t, 'the buffer'), 'STATUS_UP {"version":"0.1.0","port":"8030","buffer":"the buffer"}');
    assert.equal(h.summarize({ version: '0.1.0', state: 'error', error: 'port 8030: in use' }, t), 'STATUS_ERROR {"version":"0.1.0","error":"port 8030: in use"}');
    assert.equal(h.summarize({}, t), 'STATUS_UP {"version":"?","port":"?","buffer":""}');
    assert.equal(h.summarize(null, t), 'STATUS_DOWN');
    assert.equal(h.summarize('text', t), 'STATUS_DOWN');
});

const MIB = 1048576;
const GIB = 1024 * MIB;

test('the MPD version is read from its banner, and a second value counts from 0.21.19', () => {
    assert.deepEqual(h.parseMpdVersion('Music Player Daemon 0.24.6 (b6f106b+)\n'), [0, 24, 6]);
    assert.deepEqual(h.parseMpdVersion('Music Player Daemon 0.21'), [0, 21, 0]);
    assert.equal(h.parseMpdVersion('nothing'), null);
    assert.equal(h.parseMpdVersion(undefined), null);
    assert.equal(h.mpdOverrides([0, 24, 6]), true);
    assert.equal(h.mpdOverrides([0, 21, 19]), true);
    assert.equal(h.mpdOverrides([0, 21, 18]), false);
    assert.equal(h.mpdOverrides([0, 20, 23]), false);
    assert.equal(h.mpdOverrides([1, 0, 0]), true);
    assert.equal(h.mpdOverrides(null), false);
});

test('the buffer is the smallest bound, never below the floor; the Pi family stays at the floor', () => {
    const mpd = [0, 24, 6];
    // The VM: x86, 3914 MiB, a 2005 MiB heap. The time bound holds at 128 MiB.
    const vm = h.bufferPlan({ memTotal: 3914 * MIB, heapLimit: 2005.3 * MIB, arch: 'x64', mpdVersion: mpd });
    assert.deepEqual(vm.bounds, { heap: 156, memory: 243, time: 128 });
    assert.equal(vm.limiting, 'time');
    assert.deepEqual([vm.reason, vm.raised, vm.mib, vm.kib], ['raised', true, 128, 131072]);
    assert.deepEqual(vm.inputs, { memTotalGb: 3.8, heapLimitMib: 2005, arch: 'x64', mpdVersion: '0.24.6' });
    // The hanger: a Pi 5 with 16 GB on a 32-bit userland, whose heap is 1 GiB whatever
    // the memory. The heap bound holds below the floor.
    const pi5 = h.bufferPlan({ memTotal: 16003 * MIB, heapLimit: 1048 * MIB, arch: 'arm', mpdVersion: mpd });
    assert.deepEqual(pi5.bounds, { heap: 76, memory: 1106, time: 86 });
    assert.deepEqual([pi5.limiting, pi5.reason, pi5.raised, pi5.mib, pi5.kib], ['heap', 'floor', false, 80, 81920]);
    // A 512 MB board: nothing beyond the reserves.
    const zero2 = h.bufferPlan({ memTotal: 512 * MIB, heapLimit: 129.5 * MIB, arch: 'arm', mpdVersion: mpd });
    assert.deepEqual(zero2.bounds, { heap: 0, memory: 0, time: 86 });
    assert.deepEqual([zero2.reason, zero2.mib], ['floor', 80]);
    // x86 with plenty: the time bound, well under the ceiling.
    const big = h.bufferPlan({ memTotal: 64 * GIB, heapLimit: 4144 * MIB, arch: 'x64', mpdVersion: mpd });
    assert.deepEqual([big.limiting, big.mib], ['time', 128]);
    // The ceiling holds when every bound is past it.
    const huge = h.bufferPlan({ memTotal: 64 * GIB, heapLimit: 40 * GIB, arch: 'x64', mpdVersion: mpd });
    assert.ok(huge.bounds.heap > 1024 && huge.bounds.memory > 1024);
    assert.ok(huge.mib <= 1024);
    // An architecture not measured takes the Pi's constant.
    assert.equal(h.bufferPlan({ memTotal: 4 * GIB, heapLimit: 2 * GIB, arch: 'riscv64', mpdVersion: mpd }).bounds.time, 86);
    // Off, or MPD unknown or too old: said, and nothing appended.
    assert.equal(h.bufferPlan({ memTotal: 3914 * MIB, heapLimit: 2005 * MIB, arch: 'x64', mpdVersion: mpd, enabled: false }).reason, 'off');
    assert.equal(h.bufferPlan({ memTotal: 3914 * MIB, heapLimit: 2005 * MIB, arch: 'x64', mpdVersion: null }).reason, 'mpd_unknown');
    assert.equal(h.bufferPlan({ memTotal: 3914 * MIB, heapLimit: 2005 * MIB, arch: 'x64', mpdVersion: [0, 21, 5] }).reason, 'mpd_old');
    assert.equal(h.bufferPlan({}).reason, 'mpd_unknown');
});

test('the hook appends the line under its marker only when the buffer is raised', () => {
    const mpd = [0, 24, 6];
    const raised = h.bufferPlan({ memTotal: 3914 * MIB, heapLimit: 2005.3 * MIB, arch: 'x64', mpdVersion: mpd });
    const text = h.mpdConfigText(raised);
    assert.equal(text, '\n# MPDantic: the output buffer sized to this player: 128 MiB\nmax_output_buffer_size "131072"\n');
    assert.ok(text.includes(h.MPD_MARKER));
    assert.equal(h.mpdConfigText(h.bufferPlan({ memTotal: 16003 * MIB, heapLimit: 1048 * MIB, arch: 'arm', mpdVersion: mpd })), '');
    assert.equal(h.mpdConfigText(Object.assign({}, raised, { enabled: false })), '');
    assert.equal(h.mpdConfigText(Object.assign({}, raised, { supported: false })), '');
    assert.equal(h.mpdConfigText(null), '');
    // mpd.conf is current when it carries the text, or no marker when there is none.
    const template = 'max_output_buffer_size\t\t"81920"\n';
    assert.equal(h.mpdConfCurrent(template + text, text), true);
    assert.equal(h.mpdConfCurrent(template, text), false);
    assert.equal(h.mpdConfCurrent(template + text.replace('131072', '98304'), text), false);
    assert.equal(h.mpdConfCurrent(template, ''), true);
    assert.equal(h.mpdConfCurrent(template + text, ''), false);
});

test('the buffer summary names the reason, the bound and the inputs', () => {
    const mpd = [0, 24, 6];
    const vm = h.bufferPlan({ memTotal: 3914 * MIB, heapLimit: 2005.3 * MIB, arch: 'x64', mpdVersion: mpd });
    assert.equal(h.bufferSummary(vm, t), 'BUFFER_RAISED {"mib":"128","floor":"80","mem":"3.8","heap":"2005","version":"0.24.6","limit":"LIMIT_TIME"}');
    const pi5 = h.bufferPlan({ memTotal: 16003 * MIB, heapLimit: 1048 * MIB, arch: 'arm', mpdVersion: mpd });
    assert.match(h.bufferSummary(pi5, t), /^BUFFER_FLOOR .*"limit":"LIMIT_HEAP"/);
    assert.match(h.bufferSummary(h.bufferPlan({ memTotal: 1, heapLimit: 1, arch: 'arm', mpdVersion: [0, 20, 0] }), t), /^BUFFER_MPD_OLD .*"version":"0.20.0"/);
    assert.match(h.bufferSummary(h.bufferPlan({ enabled: false }), t), /^BUFFER_OFF /);
});
