'use strict';
// The plugin's pure parts, with no dependency on the player: what the tests exercise
// and index.js uses. Nothing here reads a file or talks to the player.

const DEFAULT_PORT = 8030;

// A port as typed: an integer from 1024 to 65535.
function validatePort(value) {
    const text = String(value === undefined || value === null ? '' : value).trim();
    if (!/^\d{1,5}$/.test(text)) return { ok: false };
    const n = parseInt(text, 10);
    if (n < 1024 || n > 65535) return { ok: false };
    return { ok: true, value: n };
}

// An address as typed: empty (the one found on the player is used), or a host name or
// an IP address, nothing with a space, a quote or a slash in it.
function validateAddress(value) {
    const text = String(value === undefined || value === null ? '' : value).trim();
    if (text === '') return { ok: true, value: '' };
    if (!/^[A-Za-z0-9.\-:[\]]{1,253}$/.test(text)) return { ok: false };
    return { ok: true, value: text };
}

// The settings as the core reads them, from the configuration's values.
function settingsFrom(get) {
    const port = validatePort(get('port', DEFAULT_PORT));
    return { port: port.ok ? port.value : DEFAULT_PORT };
}

// The IPv4 address browsers reach the player by: a wired interface first, else
// wireless, else any that is not loopback.
function lanAddress(interfaces) {
    const all = [];
    Object.keys(interfaces || {}).forEach((name) => {
        (interfaces[name] || []).forEach((a) => {
            if (!a || a.internal) return;
            const v4 = a.family === 'IPv4' || a.family === 4;
            if (v4 && a.address) all.push({ name, address: a.address });
        });
    });
    const by = (re) => all.find((a) => re.test(a.name));
    const pick = by(/^(eth|en|eno|ens|enp)/) || by(/^(wlan|wl)/) || all[0];
    return pick ? pick.address : null;
}

// The two ways onto the core's pages from the Volumio page. The UI opens an openUrl
// button in a new tab, always, and on the player's own display or in the app that tab
// has no way back; the one button type the UI navigates in place is the oauth one,
// which sets the window's location to its performer URL (two query parameters the
// page ignores). So "here" goes to Volumio's own iframe page holding the core's page,
// with the UI's arrow back; the page's address is the ui-router parameter, its slashes
// as ~2F. The address is the operator's when set, else the one found on the player.
function pageLinks(address, port, page) {
    const host = /:/.test(address) && !/^\[/.test(address) ? '[' + address + ']' : address;
    const url = 'http://' + host + ':' + String(port) + '/' + (page ? String(page).replace(/^\//, '') : '');
    return { url, here: 'http://' + host + '/iframe-page/' + url.replace(/\//g, '~2F') };
}

// The Avahi service group the plugin writes to /etc/avahi/services, so a browser or a
// phone that lists the network's services finds the pages: the player's host name in
// the name (Avahi fills %h), HTTP on the port, the path in the record.
function avahiService(port) {
    return [
        '<?xml version="1.0" standalone="no"?>',
        '<!DOCTYPE service-group SYSTEM "avahi-service.dtd">',
        '<service-group>',
        '  <name replace-wildcards="yes">MPDantic on %h</name>',
        '  <service>',
        '    <type>_http._tcp</type>',
        '    <port>' + String(port) + '</port>',
        '    <txt-record>path=/</txt-record>',
        '  </service>',
        '</service-group>',
        ''
    ].join('\n');
}

// The core's status as one line for a toast; t translates a key with its variables.
function summarize(status, t) {
    if (!status || typeof status !== 'object') return t('STATUS_DOWN');
    const version = String(status.version || '?');
    if (status.state === 'error') return t('STATUS_ERROR', { version, error: String(status.error || '?') });
    return t('STATUS_UP', { version, port: String(status.port || '?') });
}

module.exports = { DEFAULT_PORT, validatePort, validateAddress, settingsFrom, lanAddress, pageLinks, avahiService, summarize };
