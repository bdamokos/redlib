const assert = require('node:assert/strict');
const fs = require('node:fs');
const test = require('node:test');
const vm = require('node:vm');

const script = fs.readFileSync('static/post_preview.js', 'utf8');

function loadPostPreview() {
    const context = {
        document: {
            addEventListener() {},
            querySelectorAll: () => [],
        },
        HTMLImageElement: class {},
        window: {
            addEventListener() {},
        },
    };

    vm.runInNewContext(script, context);
    return context;
}

function preview({ gallery = false, scrollHeight = 0, clientHeight = 0 } = {}) {
    return {
        classList: {
            contains: (name) => name === 'gallery' && gallery,
        },
        clientHeight,
        scrollHeight,
    };
}

test('gallery previews show all images regardless of their height or count', () => {
    const { shouldTruncatePreview } = loadPostPreview();

    assert.equal(shouldTruncatePreview(preview({ gallery: true, scrollHeight: 1200, clientHeight: 250 })), false);
});

test('long text previews remain truncated', () => {
    const { shouldTruncatePreview } = loadPostPreview();

    assert.equal(shouldTruncatePreview(preview({ scrollHeight: 500, clientHeight: 250 })), true);
    assert.equal(shouldTruncatePreview(preview({ scrollHeight: 250, clientHeight: 250 })), false);
});
