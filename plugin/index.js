'use strict';
// MPDantic: the plugin shell. It owns the plugin's lifecycle, the settings page and the
// core's service; the core (bin/mpdantic) does the work on the library and will serve
// the management interface. Nothing heavy runs in this process.

const libQ = require('kew');
const fs = require('fs');
const path = require('path');
const { execFile } = require('child_process');
const VConf = require('v-conf');
const h = require('./helpers');

const LOG = 'mpdantic: ';
const SERVICE = 'mpdantic.service';
const RUN_DIR = '/run/mpdantic';
const STATUS_FILE = RUN_DIR + '/status.json';

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
    return self.startCore()
        .fail((e) => {
            self.logger.error(LOG + 'start: ' + msg(e));
            self.toast('error', 'ERR_CORE', { error: msg(e) });
        })
        .then(() => 'OK');
};

Mpdantic.prototype.onStop = function () {
    const self = this;
    self.active = false;
    return self.stopCore()
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

// ------------------------------------------------------------- the settings

Mpdantic.prototype.getUIConfig = function () {
    const self = this;
    const defer = libQ.defer();
    const lang = self.commandRouter.sharedVars.get('language_code');
    self.commandRouter.i18nJson(path.join(__dirname, 'i18n', 'strings_' + lang + '.json'),
        path.join(__dirname, 'i18n', 'strings_en.json'),
        path.join(__dirname, 'UIConfig.json'))
        .then((uiconf) => {
            const section = (id) => (uiconf.sections.find((s) => s.id === id) || { content: [] }).content;
            setValue(section('management_section'), 'port', String(self.config.get('port', String(h.DEFAULT_PORT))));
            defer.resolve(uiconf);
        })
        .fail((e) => {
            self.logger.error(LOG + 'settings page: ' + msg(e));
            defer.reject(new Error());
        });
    return defer.promise;
};

// The management section saved: the port checked, stored, and the core restarted on it.
Mpdantic.prototype.updateManagement = function (data) {
    const self = this;
    const port = h.validatePort(data && data.port);
    if (!port.ok) {
        self.toast('error', 'ERR_PORT', { value: String(data && data.port) });
        return libQ.resolve();
    }
    self.config.set('port', String(port.value));
    try { self.writeSettings(); } catch (e) { self.logger.error(LOG + 'settings: ' + msg(e)); }
    self.toast('success', 'SAVED');
    return self.startCore()
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
