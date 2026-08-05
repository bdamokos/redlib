const assert = require('node:assert/strict');
const fs = require('node:fs');
const test = require('node:test');

const searchTemplate = fs.readFileSync('templates/search.html', 'utf8');
const stylesheet = fs.readFileSync('static/style.css', 'utf8');

test('shell search status and results stack in the main content column', () => {
    assert.match(
        searchTemplate,
        /class="feed_status_banner warming"[\s\S]*id="page_fragment"/,
        'search shell must retain the status banner before the fragment container',
    );
    assert.match(
        stylesheet,
        /main:has\(> \.feed_status_banner\)\s*{[^}]*align-items:\s*center;[^}]*flex-direction:\s*column;[^}]*}/,
        'shell-loaded pages must stack and center status and result content',
    );
});
