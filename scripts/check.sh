#!/bin/bash
# The workshop pass CI runs: formatting, lints with warnings denied, tests, the
# documentation with warnings denied, the shell scripts parsed and linted, the plugin's
# files parsed and tested, no name used that nothing defines, no python, no trace
# print, no key-like literal, and the workflows through actionlint. Run it before a
# commit; nothing ships without it.
set -euo pipefail
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
cd "$ROOT"

echo "check: format"
cargo fmt --all -- --check

echo "check: clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings

echo "check: tests"
cargo test --workspace --locked

echo "check: documentation"
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

echo "check: shell scripts"
for script in scripts/*.sh plugin/*.sh; do
  sh -n "$script" || { echo "check: $script does not parse" >&2; exit 1; }
done
# The scripts the player runs go through its sh: POSIX sh, no bash. The builder image
# has no shellcheck; CI's shell job runs the same lint on the bare runner, which has.
if command -v shellcheck >/dev/null 2>&1; then
  shellcheck -S warning -s sh plugin/install.sh plugin/uninstall.sh plugin/bin/mpdantic-mount
  # The helper refuses a target outside /run/mpdantic/mounts without calling mount.
  out=$(plugin/bin/mpdantic-mount umount /etc 2>&1) && {
    echo "check: mpdantic-mount accepted umount /etc" >&2
    exit 1
  }
  echo "$out" | grep -q 'outside /run/mpdantic/mounts' || {
    echo "check: mpdantic-mount refusal was not the confinement message: $out" >&2
    exit 1
  }
  shellcheck -S warning scripts/*.sh
else
  echo "check: shellcheck is not installed here; CI's shell job runs it"
fi

# Node runs the checks of the plugin's files: here where it is installed, else in the
# node image.
if command -v node >/dev/null 2>&1; then
  with_node() { node "$@"; }
else
  with_node() { docker run -i --rm -v "$ROOT:/mpdantic" -w /mpdantic node:20-slim node "$@"; }
fi

echo "check: plugin files"
# Volumio's core reads every plugin's strings at its start: one bad file takes the
# whole backend down, so every JSON the plugin ships must parse. A key written twice in
# one object parses, and the later one silently wins: that is refused too. The pages'
# catalogues, built into the core, go through the same.
with_node - plugin/package.json plugin/config.json plugin/UIConfig.json plugin/i18n/*.json bins/mpdantic/assets/lang/*.json <<'JS'
const fs = require('fs');
function keysWrittenTwice(text) {
  const twice = new Set();
  const stack = [];
  let i = 0;
  let start = 0;
  const skipString = () => { i++; while (i < text.length && text[i] !== '"') { if (text[i] === '\\') i++; i++; } return text.slice(start + 1, i++); };
  while (i < text.length) {
    const c = text[i];
    if (c === '{') { stack.push({ keys: new Set(), expectKey: true }); i++; }
    else if (c === '}') { stack.pop(); i++; }
    else if (c === '[') { stack.push(null); i++; }
    else if (c === ']') { stack.pop(); i++; }
    else if (c === '"') {
      start = i;
      const s = skipString();
      const top = stack[stack.length - 1];
      if (top && top.expectKey) { if (top.keys.has(s)) twice.add(s); top.keys.add(s); top.expectKey = false; }
    }
    else if (c === ':') { i++; }
    else if (c === ',') { const top = stack[stack.length - 1]; if (top) top.expectKey = true; i++; }
    else i++;
  }
  return [...twice].sort();
}
const files = process.argv.slice(2);
for (const file of files) {
  const text = fs.readFileSync(file, 'utf8');
  try { JSON.parse(text); } catch (e) { console.error(file + ': ' + e.message); process.exit(1); }
  const twice = keysWrittenTwice(text);
  if (twice.length) { console.error(file + ': written twice in one object: ' + twice.join(', ')); process.exit(1); }
  if (!text.endsWith('\n') || text.replace(/\n+$/, '') !== text.trimEnd()) { console.error(file + ': must end with one newline and nothing after the JSON'); process.exit(1); }
}
console.log(files.length + ' plugin files parse');
// Every language carries exactly the English keys, in the English order, with the
// same variables ({{name}} on the settings page, {name} in the pages' catalogues): a
// string added in English alone would show as its key on a player in another
// language, and a variable renamed in one would show as text. The settings page's
// strings and the pages' catalogues are two sets, each held to its own English.
const sets = [
  { name: 'plugin/i18n', english: 'strings_en.json', is: (f) => /\/i18n\/strings_[a-z_A-Z]+\.json$/.test(f) },
  { name: 'assets/lang', english: 'en.json', is: (f) => /\/assets\/lang\/[a-z_A-Z]+\.json$/.test(f) }
];
const flat = (o, p = '') => Object.entries(o).flatMap(([k, v]) => (v && typeof v === 'object' ? flat(v, p + k + '.') : [[p + k, v]]));
// The names of a text's variables, each once: {{name}} on the settings page, {name}
// and the plural {name|...} in the catalogues. A translation may use a count more or
// fewer times than the English for its agreement; it is the set of names that must match.
const vars = (s) => Array.from(new Set(Array.from(String(s).matchAll(/\{\{?([a-z_]+)(\|[^}]*)?\}?\}/g)).map((m) => m[1]))).sort().join(' ');
for (const set of sets) {
  const strings = files.filter(set.is);
  const english = strings.find((f) => f.endsWith('/' + set.english));
  if (!english) { console.error(set.name + '/' + set.english + ' is missing'); process.exit(1); }
  const en = flat(JSON.parse(fs.readFileSync(english, 'utf8')));
  for (const file of strings) {
    if (file === english) continue;
    const other = flat(JSON.parse(fs.readFileSync(file, 'utf8')));
    const missing = en.filter(([k]) => !other.some(([o]) => o === k)).map(([k]) => k);
    const extra = other.filter(([k]) => !en.some(([e]) => e === k)).map(([k]) => k);
    if (missing.length || extra.length) { console.error(file + ': not the English keys' + (missing.length ? '; missing ' + missing.join(', ') : '') + (extra.length ? '; extra ' + extra.join(', ') : '')); process.exit(1); }
    const order = other.map(([k]) => k).join('\n') === en.map(([k]) => k).join('\n');
    if (!order) { console.error(file + ': the keys are not in the English order'); process.exit(1); }
    const varsDiffer = en.filter(([k, v]) => vars(v) !== vars(other.find(([o]) => o === k)[1])).map(([k]) => k);
    if (varsDiffer.length) { console.error(file + ': the variables differ from English in ' + varsDiffer.join(', ')); process.exit(1); }
    // Empty only where the English is empty (a text filled in at runtime).
    const empty = other.filter(([k, v]) => String(v).trim() === '' && String(en.find(([e]) => e === k)[1]).trim() !== '').map(([k]) => k);
    if (empty.length) { console.error(file + ': empty in ' + empty.join(', ')); process.exit(1); }
  }
  console.log(set.name + ': ' + (strings.length - 1) + ' translations carry the ' + en.length + ' English keys');
}
JS

echo "check: plugin"
with_node --check plugin/index.js plugin/helpers.js
# Node 24 treats a bare directory as one failed module; name the files.
with_node --test plugin/test/*.js

echo "check: pages"
# The inline script of every page the core serves, parsed by Node: a page that does
# not parse shows nothing, and no test of the core would say so.
mkdir -p target/check
for page in bins/*/assets/*.html; do
  script="target/check/$(basename "$page").js"
  awk '/<script>/{f=1;next}/<\/script>/{f=0}f' "$page" > "$script"
  [ -s "$script" ] || { echo "check: $page has no script" >&2; exit 1; }
  with_node --check "$script" || { echo "check: the script of $page does not parse" >&2; exit 1; }
done

echo "check: catalogue"
# The catalogues against the pages and the core: every key used is there, every key
# there is used, the English the markup carries is the catalogue's, every msg! of the
# core is in it with its template and its arguments, each language's plural forms.
with_node scripts/lint/catalogue.js

echo "check: undefined names"
# A name nothing defines parses and passes the tests, and throws when its line runs:
# the plugin's scripts are linted for it, with one pinned ESLint fetched for the run.
ESLINT="eslint@10.11.0"
LINT="npx --yes $ESLINT --config scripts/lint/eslint.config.js --no-warn-ignored plugin/*.js plugin/test/*.js"
if command -v npx >/dev/null 2>&1; then
  $LINT || { echo "check: a script uses a name nothing defines (or $ESLINT could not be fetched)" >&2; exit 1; }
else
  docker run --rm -v "$ROOT:/mpdantic:ro" -v mpdantic-check-npm:/root/.npm -w /mpdantic node:20-slim sh -c "$LINT" \
    || { echo "check: a script uses a name nothing defines (or $ESLINT could not be fetched)" >&2; exit 1; }
fi

echo "check: traces"
# A line printed to find a fault once ships to every player's journal at every event it
# marks. A print that names itself a trace is refused, outside comments.
trace_prints() {
  grep -rnE "(eprintln!|println!|console\.(log|error|warn))\(\s*['\"]TRACE" --include=*.rs --include=*.js bins plugin 2>/dev/null \
    | grep -v node_modules | grep -vE ":[0-9]+:[[:space:]]*(#|//)"
}
if trace_prints >/dev/null; then
  echo "check: a trace print is left in the source:" >&2
  trace_prints >&2
  exit 1
fi

echo "check: no python"
# The product is Rust and Node, and a player without python3 must install and run it.
python_runs() {
  grep -rnE "(^|[^A-Za-z_\"'/.-])python3?( |$|-)" --include=*.sh --include=*.js --include=*.rs --include=*.yml --include=*.service bins plugin scripts .github 2>/dev/null \
    | grep -v node_modules | grep -v "scripts/check.sh" | grep -vE ":[0-9]+:[[:space:]]*(#|//)"
}
if python_runs >/dev/null; then
  echo "check: python is run in the source:" >&2
  python_runs >&2
  exit 1
fi

echo "check: keys"
# No API key or password in the source: a 32-hex literal in the plugin is one.
if grep -nE "['\"][0-9a-f]{32}['\"]" plugin/*.js >/dev/null; then
  echo "check: a key-like literal is in the plugin's source:" >&2
  grep -nE "['\"][0-9a-f]{32}['\"]" plugin/*.js >&2
  exit 1
fi

echo "check: actionlint"
if command -v actionlint > /dev/null; then
  actionlint -no-color .github/workflows/*.yml
elif command -v docker > /dev/null; then
  docker run --rm -v "$PWD:/repo" -w /repo rhysd/actionlint:latest -no-color .github/workflows/ci.yml .github/workflows/release.yml
else
  echo "check: neither actionlint nor docker is here; the workflows were not linted"
fi

echo "check: clean"
