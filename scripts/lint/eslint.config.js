// The lint scripts/check.sh runs over the plugin's scripts: one rule, no name used
// that nothing defines. A script with such a name parses and passes its tests, and
// throws when the line runs. The globals are listed here so the lint needs ESLint and
// nothing else.
const names = (list) => Object.fromEntries(list.map((name) => [name, 'readonly']));

const node = names([
  'require', 'module', 'exports', 'process', 'Buffer', '__dirname', '__filename',
  'console', 'setTimeout', 'clearTimeout', 'setInterval', 'clearInterval',
  'setImmediate', 'clearImmediate', 'URL', 'TextDecoder', 'TextEncoder'
]);

module.exports = [
  {
    files: ['plugin/**/*.js'],
    linterOptions: { reportUnusedDisableDirectives: 'off' },
    languageOptions: { ecmaVersion: 2022, sourceType: 'commonjs', globals: node },
    rules: { 'no-undef': 'error' }
  }
];
