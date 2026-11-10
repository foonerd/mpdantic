'use strict';
// MPDantic: the plugin shell. It owns the plugin's lifecycle, the settings page, the
// core's service and the pages' announcement on the network; the core (bin/mpdantic)
// does the work on the library and serves the management pages. Nothing heavy runs in
// this process.

const libQ = require('kew');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { execFile } = require('child_process');
const VConf = require('v-conf');
const h = require('./helpers');

const LOG = 'mpdantic: ';
const SERVICE = 'mpdantic.service';
const RUN_DIR = '/run/mpdantic';
const STATUS_FILE = RUN_DIR + '/status.json';
// The pages announced on the network, as Volumio announces its own page: a service
// file Avahi watches for.
const AVAHI_FILE = '/etc/avahi/services/mpdantic.service';

const msg = (e) => (e && e.message ? e.message : String(e));

function Mpdantic(context) {
    this.context = context;
    this.commandRouter = context.coreCommand;
    this.logger = this.commandRouter.logger;
    this.config = new VConf();
    this.active = false;
    this.settingsFile = null;
}

// ----------------------------------------------------------------- lifecycle

Mpdantic.prototype.onVolumioStart = function () {
    this.configFile = this.commandRouter.pluginManager.getConfigurationFile(this.context, 'config.json');
    this.config.loadFile(this.configFile);
    // The settings as the core reads them, beside the configuration.
    this.settingsFile = path.join(path.dirname(this.configFile), 'settings.json');
    return libQ.resolve();
};

Mpdantic.prototype.onStart = function () {
    const self = this;
    self.active = true;
    self.commandRouter.loadI18nStrings();
    // The core reads the settings at its start: the file is written first.
    try { self.writeSettings(); } catch (e) { self.logger.error(LOG + 'settings: ' + msg(e)); }
    return self.announce()
        .fail((e) => { self.logger.warn(LOG + 'announce: ' + msg(e)); })
        .then(() => self.startCore())
        .fail((e) => {
            self.logger.error(LOG + 'start: ' + msg(e));
            self.toast('error', 'ERR_CORE', { error: msg(e) });
        })
        .then(() => 'OK');
};

Mpdantic.prototype.onStop = function () {
    const self = this;
    self.active = false;
    return self.unannounce()
        .fail((e) => { self.logger.warn(LOG + 'unannounce: ' + msg(e)); })
        .then(() => self.stopCore())
        .fail((e) => { self.logger.warn(LOG + 'stop: ' + msg(e)); })
        .then(() => 'OK');
};

Mpdantic.prototype.onRestart = function () {
    return libQ.resolve();
};

Mpdantic.prototype.getConfigurationFiles = function () {
    return ['config.json'];
};

Mpdantic.prototype.getI18nFile = function (langCode) {
    const file = path.join(__dirname, 'i18n', 'strings_' + langCode + '.json');
    return fs.existsSync(file) ? file : path.join(__dirname, 'i18n', 'strings_en.json');
};

// ---------------------------------------------------------------------- core

Mpdantic.prototype.systemctl = function (verb) {
    const self = this;
    const defer = libQ.defer();
    execFile('/usr/bin/sudo', ['/bin/systemctl', verb, SERVICE], { timeout: 20000 }, (error, stdout, stderr) => {
        if (error) {
            self.logger.error(LOG + 'systemctl ' + verb + ': ' + String(stderr || error.message).trim());
            defer.reject(new Error(String(stderr || error.message).trim()));
        } else {
            defer.resolve();
        }
    });
    return defer.promise;
};

Mpdantic.prototype.startCore = function () {
    return this.systemctl('restart');
};

Mpdantic.prototype.stopCore = function () {
    return this.systemctl('stop');
};

Mpdantic.prototype.readStatus = function () {
    try { return JSON.parse(fs.readFileSync(STATUS_FILE, 'utf8')); } catch (e) { return null; }
};

// The settings as the core reads them, written whole and moved into place.
Mpdantic.prototype.writeSettings = function () {
    const self = this;
    const text = JSON.stringify(h.settingsFrom((k, d) => self.config.get(k, d)), null, 2) + '\n';
    const part = self.settingsFile + '.part';
    fs.writeFileSync(part, text);
    fs.renameSync(part, self.settingsFile);
};

// ------------------------------------------------------------ the announcement

// Written directly when the folder allows it (Volumio's own system plugin opens it
// up), else through sudo tee, which the volumio user has.
Mpdantic.prototype.announce = function () {
    const self = this;
    const text = h.avahiService(h.settingsFrom((k, d) => self.config.get(k, d)).port);
    try {
        fs.writeFileSync(AVAHI_FILE, text);
        return libQ.resolve();
    } catch (e) {
        if (e.code !== 'EACCES' && e.code !== 'EPERM') return libQ.reject(e);
    }
    const defer = libQ.defer();
    const child = execFile('/usr/bin/sudo', ['/usr/bin/tee', AVAHI_FILE], { timeout: 10000 }, (error, stdout, stderr) => {
        if (error) defer.reject(new Error(String(stderr || error.message).trim()));
        else defer.resolve();
    });
    child.stdin.on('error', () => { /* the exit says it */ });
    child.stdin.end(text);
    return defer.promise;
};

Mpdantic.prototype.unannounce = function () {
    try {
        fs.unlinkSync(AVAHI_FILE);
        return libQ.resolve();
    } catch (e) {
        if (e.code === 'ENOENT') return libQ.resolve();
        if (e.code !== 'EACCES' && e.code !== 'EPERM') return libQ.reject(e);
    }
    const defer = libQ.defer();
    execFile('/usr/bin/sudo', ['/bin/rm', '-f', AVAHI_FILE], { timeout: 10000 }, (error, stdout, stderr) => {
        if (error) defer.reject(new Error(String(stderr || error.message).trim()));
        else defer.resolve();
    });
    return defer.promise;
};

// ------------------------------------------------------------- the settings

Mpdantic.prototype.getUIConfig = function () {
    const self = this;
    const defer = libQ.defer();
    const lang = self.commandRouter.sharedVars.get('language_code');
    const c = self.config;
    self.commandRouter.i18nJson(path.join(__dirname, 'i18n', 'strings_' + lang + '.json'),
        path.join(__dirname, 'i18n', 'strings_en.json'),
        path.join(__dirname, 'UIConfig.json'))
        .then((uiconf) => {
            const section = (id) => (uiconf.sections.find((s) => s.id === id) || { content: [] }).content;
            const mgmt = section('management_section');
            const port = h.settingsFrom((k, d) => c.get(k, d)).port;
            setValue(mgmt, 'port', String(port));
            // The address browsers reach the player by: the operator's when set, else the
            // one found on the player's own interfaces (an IP address, so a network whose
            // .local names do not resolve still gets there); the one found is the field's
            // placeholder.
            const found = h.lanAddress(os.networkInterfaces());
            const override = String(c.get('address', '')).trim();
            setValue(mgmt, 'address', override);
            const addressField = mgmt.find((x) => x.id === 'address');
            if (addressField && addressField.attributes && addressField.attributes[0]) addressField.attributes[0].placeholder = found || '';
            // The buttons open the core's pages at that address, here inside Volumio's own
            // page or in a tab of their own; without an address there are no buttons.
            const here = mgmt.find((x) => x.id === 'open_here');
            const tab = mgmt.find((x) => x.id === 'open_tab');
            const address = override || found;
            if (address) {
                const links = h.pageLinks(address, port);
                if (here) here.onClick.performerUrl = links.here;
                if (tab) {
                    tab.onClick.url = links.url;
                    tab.doc = self.i18n('OPEN_TAB_DESC') + ' ' + links.url;
                }
            } else {
                if (here) here.hidden = true;
                if (tab) tab.hidden = true;
            }
            defer.resolve(uiconf);
        })
        .fail((e) => {
            self.logger.error(LOG + 'settings page: ' + msg(e));
            defer.reject(new Error());
        });
    return defer.promise;
};

// The management section saved: the port and the address checked and stored, the
// announcement rewritten, the core restarted on the port.
Mpdantic.prototype.updateManagement = function (data) {
    const self = this;
    const port = h.validatePort(data && data.port);
    if (!port.ok) {
        self.toast('error', 'ERR_PORT', { value: String(data && data.port) });
        return libQ.resolve();
    }
    const address = h.validateAddress(data && data.address);
    if (!address.ok) {
        self.toast('error', 'ERR_ADDRESS', { value: String(data && data.address) });
        return libQ.resolve();
    }
    self.config.set('port', String(port.value));
    self.config.set('address', address.value);
    try { self.writeSettings(); } catch (e) { self.logger.error(LOG + 'settings: ' + msg(e)); }
    self.toast('success', 'SAVED');
    return self.announce()
        .fail((e) => { self.logger.warn(LOG + 'announce: ' + msg(e)); })
        .then(() => self.startCore())
        .fail((e) => {
            self.logger.error(LOG + 'restart: ' + msg(e));
            self.toast('error', 'ERR_CORE', { error: msg(e) });
        })
        .then(() => libQ.resolve());
};

// The status button: what the core says, as one toast.
Mpdantic.prototype.showStatus = function () {
    const self = this;
    const line = h.summarize(self.readStatus(), (key, vars) => self.i18n(key, vars));
    self.commandRouter.pushToastMessage('info', self.i18n('STATUS_TITLE'), line);
    return libQ.resolve();
};

// ------------------------------------------------------------------ strings

// A string of the plugin's, with its {{variables}} filled in.
Mpdantic.prototype.i18n = function (key, vars) {
    let text = this.commandRouter.getI18nString('MPDANTIC.' + key);
    if (typeof text !== 'string') text = key;
    Object.keys(vars || {}).forEach((k) => { text = text.split('{{' + k + '}}').join(String(vars[k])); });
    return text;
};

Mpdantic.prototype.toast = function (type, key, vars) {
    this.commandRouter.pushToastMessage(type, this.i18n('PLUGIN_NAME'), this.i18n(key, vars));
};

function setValue(content, id, value) {
    const item = content.find((c) => c.id === id);
    if (item) item.value = value;
}

module.exports = Mpdantic;
