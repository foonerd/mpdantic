'use strict';
// The catalogues held to the pages and the core, run from the repository's root by
// scripts/check.sh: every key a page uses is in the English catalogue and every page
// key of the catalogue is used; the English the markup carries is the catalogue's;
// every finding code's text is the core's; every msg! of the core is in the catalogue
// with the same template, its arguments the template's placeholders; the markup a
// text may carry is a link, a code word, a bold or a span and nothing else; and every
// language's plural forms are as many as that language takes.
const fs = require('fs');

const LANG = 'bins/mpdantic/assets/lang/';
const PAGES = ['bins/mpdantic/assets/index.html', 'bins/mpdantic/assets/about.html'];
const SRC = 'bins/mpdantic/src/';
// The plural forms a language's texts list after the bar, by Volumio's code.
const FORMS = { en: 2, de: 2, es: 2, fr: 2, it: 2, ja: 1, nl: 2, pl: 3, pt: 2, ru: 3, zh: 1 };
// The prefixes the pages complete at runtime from a state or a kind.
const DYNAMIC = ['page.list_state.', 'page.kind.', 'page.tag.', 'page.field.', 'page.class.', 'page.file_state.', 'page.entry_state.', 'page.import_state.', 'page.action.', 'page.outcome.', 'code.'];

const problems = [];
const say = (p) => problems.push(p);
const read = (f) => fs.readFileSync(f, 'utf8');
const squash = (s) => s.replace(/\s+/g, ' ').trim();
const decode = (s) => s.replace(/&quot;/g, '"').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&amp;/g, '&');
const placeholders = (s) => Array.from(String(s).matchAll(/\{([a-z_]+)(\|[^}]*)?\}/g)).map((m) => m[1]);

const en = JSON.parse(read(LANG + 'en.json'));
const keys = new Set(Object.keys(en));

// The pages: the keys in the markup, their English, the keys in the scripts.
const used = new Set();
for (const page of PAGES) {
  const html = read(page);
  for (const m of html.matchAll(/<(\w+)([^>]*?)\sdata-i18n="([^"]+)"([^>]*)>([^<]*)</g)) {
    const key = m[3];
    used.add(key);
    if (!keys.has(key)) { say(page + ': data-i18n ' + key + ' is not in the catalogue'); continue; }
    if (squash(decode(m[5])) !== squash(en[key])) say(page + ': the markup under ' + key + ' differs from the catalogue: ' + JSON.stringify(squash(decode(m[5]))));
  }
  for (const m of html.matchAll(/<(\w+)([^>]*?)\sdata-i18n-html="([^"]+)"([^>]*)>([\s\S]*?)<\/\1>/g)) {
    const key = m[3];
    used.add(key);
    if (!keys.has(key)) { say(page + ': data-i18n-html ' + key + ' is not in the catalogue'); continue; }
    if (squash(m[5]) !== squash(en[key])) say(page + ': the markup under ' + key + ' differs from the catalogue');
  }
  for (const m of html.matchAll(/\sdata-i18n-(?:title|placeholder)="([^"]+)"/g)) {
    used.add(m[1]);
    if (!keys.has(m[1])) say(page + ': ' + m[1] + ' is not in the catalogue');
  }
  const script = html.split('<script>')[1] || '';
  for (const m of script.matchAll(/'((?:page|about|list)\.[a-z0-9_.]+)'/g)) {
    used.add(m[1]);
    if (!keys.has(m[1]) && !DYNAMIC.some((p) => m[1] === p)) say(page + ': ' + m[1] + ' is not in the catalogue');
  }
  for (const m of script.matchAll(/'((?:page|about|code)\.[a-z0-9_.]+\.)'\s*\+/g)) {
    if (!DYNAMIC.includes(m[1])) say(page + ': the prefix ' + m[1] + ' is completed at runtime and not listed as dynamic');
  }
}
for (const key of keys) {
  if (!/^(page|about|list)\./.test(key)) continue;
  if (!used.has(key) && !DYNAMIC.some((p) => key.startsWith(p))) say('en.json: ' + key + ' is used by no page');
}

// The finding codes: the English of the catalogue is the core's.
const codes = {};
for (const f of ['cuecheck.rs', 'plcheck.rs', 'tagcheck.rs']) {
  for (const m of read(SRC + f).matchAll(/^\s+"([CPT]\d\d)" => "((?:[^"\\]|\\.)*)",$/gm)) codes[m[1]] = JSON.parse('"' + m[2] + '"');
}
for (const [code, text] of Object.entries(codes)) {
  if (en['code.' + code] !== text) say('en.json: code.' + code + ' is not the core\'s text: ' + JSON.stringify(text));
}
for (const key of keys) {
  if (key.startsWith('code.') && !(key.slice(5) in codes)) say('en.json: ' + key + ' names no code of the core');
}

// The core: every msg! in the catalogue with the same template, its arguments the
// placeholders, and no core key without a msg!.
const core = {};
for (const f of fs.readdirSync(SRC).filter((x) => x.endsWith('.rs'))) {
  const text = read(SRC + f);
  const re = /msg!\(\s*"([a-z0-9_.]+)"\s*,\s*"((?:[^"\\]|\\.)*)"/g;
  let m;
  while ((m = re.exec(text))) {
    const key = m[1];
    const template = JSON.parse('"' + m[2] + '"');
    if (key.startsWith('core.test.')) continue;
    if (key in core && core[key] !== template) say(f + ': ' + key + ' has two templates');
    core[key] = template;
    // The call's arguments: the names before `=` up to the closing parenthesis.
    let depth = 1;
    let i = re.lastIndex;
    let quoted = false;
    while (i < text.length && depth > 0) {
      const c = text[i];
      if (quoted) { if (c === '\\') i++; else if (c === '"') quoted = false; }
      else if (c === '"') quoted = true;
      else if (c === '(' || c === '[' || c === '{') depth++;
      else if (c === ')' || c === ']' || c === '}') depth--;
      i++;
    }
    const span = text.slice(re.lastIndex, i - 1);
    const args = Array.from(span.matchAll(/(?:^|,)\s*([a-z_]+)\s*=(?!=)/g)).map((x) => x[1]);
    const named = placeholders(template);
    const missing = named.filter((n) => !args.includes(n));
    const extra = args.filter((n) => !named.includes(n));
    if (missing.length || extra.length) say(f + ': ' + key + (missing.length ? ' lacks ' + missing.join(', ') : '') + (extra.length ? ' passes ' + extra.join(', ') + ' the template does not name' : ''));
  }
}
for (const [key, template] of Object.entries(core)) {
  if (!(key in en)) say('en.json: ' + key + ' of the core is missing');
  else if (en[key] !== template) say('en.json: ' + key + ' is not the core\'s template: ' + JSON.stringify(template));
}
for (const key of keys) {
  if (key.startsWith('core.') && !(key in core)) say('en.json: ' + key + ' has no msg! in the core');
}

// Every language: the markup a text may carry, and the plural forms it takes.
const files = fs.readdirSync(LANG).filter((f) => /^[a-z]+\.json$/.test(f));
for (const file of files) {
  const code = file.replace('.json', '');
  const forms = FORMS[code];
  if (!forms) { say(LANG + file + ': not a language the core carries'); continue; }
  const cat = JSON.parse(read(LANG + file));
  for (const [key, value] of Object.entries(cat)) {
    if (typeof value !== 'string') { say(file + ': ' + key + ' is not a text'); continue; }
    for (const m of value.matchAll(/\{([a-z_]+)\|([^}]*)\}/g)) {
      const n = m[2].split('|').length;
      if (n !== forms) say(file + ': ' + key + ' lists ' + n + ' plural forms for ' + m[1] + '; ' + code + ' takes ' + forms);
    }
    if (/<|&/.test(value)) {
      const html = key === 'about.intro' || key === 'about.evo.text';
      if (!html) say(file + ': ' + key + ' carries markup or an entity, which only the two html texts may');
      const tags = Array.from(value.matchAll(/<\/?([a-z]+)([^>]*)>/g));
      for (const tag of tags) {
        if (!['a', 'code', 'b', 'span'].includes(tag[1])) say(file + ': ' + key + ' carries a <' + tag[1] + '>');
        for (const attr of tag[2].matchAll(/([a-z-]+)=/g)) {
          if (!['href', 'target', 'rel', 'class'].includes(attr[1])) say(file + ': ' + key + ' carries the attribute ' + attr[1]);
        }
      }
      if (/javascript:|&(?!amp;|lt;|gt;|quot;)/i.test(value)) say(file + ': ' + key + ' carries a script link or a bare ampersand');
    }
  }
}

if (problems.length) {
  console.error(problems.join('\n'));
  process.exit(1);
}
console.log(files.length + ' catalogues, ' + keys.size + ' keys; the pages, the codes and the core agree with the English');
