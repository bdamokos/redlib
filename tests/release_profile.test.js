const assert = require('node:assert/strict');
const fs = require('node:fs');
const test = require('node:test');

test('release profile keeps the bounded-memory ThinLTO configuration', () => {
    const manifest = fs.readFileSync('Cargo.toml', 'utf8');
    const releaseProfileStart = manifest.indexOf('[profile.release]');
    assert.notEqual(releaseProfileStart, -1);
    const releaseProfileTail = manifest.slice(releaseProfileStart + '[profile.release]'.length);
    const nextSectionOffset = releaseProfileTail.search(/^\[/m);
    const releaseProfile = nextSectionOffset === -1 ? releaseProfileTail : releaseProfileTail.slice(0, nextSectionOffset);

    assert.match(releaseProfile, /^codegen-units\s*=\s*1$/m);
    assert.match(releaseProfile, /^lto\s*=\s*"thin"$/m);
    assert.match(releaseProfile, /^strip\s*=\s*"symbols"$/m);
});
