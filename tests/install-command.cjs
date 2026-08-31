// `node tests/install-command.cjs`: verify install command selection without
// installing a binary or changing the caller's toolchain/environment.
const assert = require('node:assert/strict');
const path = require('node:path');
const {execFileSync} = require('node:child_process');
const repo = path.join(__dirname, '..');
function command(...overrides) {
  const env = {...process.env};
  delete env.INSTALL_TOOLCHAIN;
  const output = execFileSync('make', ['--no-print-directory', '-n', 'install', ...overrides], {cwd:repo, encoding:'utf8', env});
  return output.trim().split(/\s+/);
}
const args = ['install', '--path', '.', '--bin', 'revebot', '--locked', '--force', '--offline'];
assert.deepEqual(command('RUSTUP_TOOLCHAIN=', 'INSTALL_TOOLCHAIN='), ['cargo', ...args]);
assert.deepEqual(command('RUSTUP_TOOLCHAIN=1.97.1'), ['cargo', '+1.97.1', ...args]);
assert.deepEqual(command('RUSTUP_TOOLCHAIN=1.97.1', 'INSTALL_TOOLCHAIN=stable'), ['cargo', '+stable', ...args]);
console.log('install command selection: passed');
