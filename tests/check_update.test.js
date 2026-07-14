const assert = require('node:assert/strict');
const fs = require('node:fs');
const test = require('node:test');
const vm = require('node:vm');

const script = fs.readFileSync('static/check_update.js', 'utf8');

function loadCheckUpdate(fetch) {
    const elements = {
        'error-446': { remove() {} },
        git_commit: { dataset: { value: 'local-commit' } },
        'random-instance': { href: '', innerText: '' },
        'random-instance-status': { innerText: '' },
        'update-status': { innerText: 'Primary error context' },
    };
    const errors = [];
    const context = {
        console: { error: (...args) => errors.push(args) },
        document: {
            getElementById: (id) => elements[id],
        },
        DOMParser: class {
            parseFromString() {
                return { getElementsByTagName: () => [] };
            }
        },
        fetch,
        window: {
            addEventListener() {},
            location: { host: 'redlib.example', pathname: '/error-path' },
        },
    };

    vm.runInNewContext(script, context);
    return { context, elements, errors };
}

test('instance-list network failures do not replace primary error context', async () => {
    const { context, elements, errors } = loadCheckUpdate(async (path) => {
        if (path === '/commits.atom') {
            return { text: async () => '' };
        }
        throw new TypeError('Load failed');
    });

    await context.checkOtherInstances();

    assert.equal(elements['update-status'].innerText, 'Primary error context');
    assert.equal(elements['random-instance'].innerText, '');
    assert.equal(errors.length, 1);
});

test('instance-list HTTP failures do not replace primary error context', async () => {
    const { context, elements, errors } = loadCheckUpdate(async (path) => {
        if (path === '/commits.atom') {
            return { text: async () => '' };
        }
        return { ok: false, status: 502 };
    });

    await context.checkOtherInstances();

    assert.equal(elements['update-status'].innerText, 'Primary error context');
    assert.equal(errors.length, 1);
});

test('a successful instance list still populates the fallback link', async () => {
    const { context, elements } = loadCheckUpdate(async (path) => {
        if (path === '/commits.atom') {
            return { text: async () => '' };
        }
        return {
            ok: true,
            json: async () => ({ instances: [{ url: 'https://fallback.example' }] }),
        };
    });

    await context.checkOtherInstances();

    assert.equal(elements['random-instance'].href, 'https://fallback.example/error-path');
    assert.equal(elements['random-instance'].innerText, 'Visit Random Instance');
});
