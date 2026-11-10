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

// The settings as the core reads them, from the configuration's values.
function settingsFrom(get) {
    const port = validatePort(get('port', DEFAULT_PORT));
    return { port: port.ok ? port.value : DEFAULT_PORT };
}

// The core's status as one line for a toast; t translates a key with its variables.
function summarize(status, t) {
    if (!status || typeof status !== 'object') return t('STATUS_DOWN');
    return t('STATUS_UP', { version: String(status.version || '?'), state: String(status.state || '?') });
}

module.exports = { DEFAULT_PORT, validatePort, settingsFrom, summarize };
