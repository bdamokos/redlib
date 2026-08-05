const assert = require('node:assert/strict');
const fs = require('node:fs');
const test = require('node:test');

const searchTemplate = fs.readFileSync('templates/search.html', 'utf8');
const utilitiesTemplate = fs.readFileSync('templates/utils.html', 'utf8');
const spaceScroll = fs.readFileSync('static/space_scroll.js', 'utf8');
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

test('compact author actions stay popup-free and visible across viewport classes', () => {
    assert.match(utilitiesTemplate, /data-post-author-action="temporary-filter"[\s\S]*?>\s*Y\s*<\/button>/);
    assert.match(utilitiesTemplate, /data-post-author-action="permanent-filter"[\s\S]*?>\s*X\s*<\/button>/);
    assert.match(utilitiesTemplate, /data-subscribe-label="F"/);
    assert.doesNotMatch(utilitiesTemplate, /Filter u\/\{\{/);
    assert.doesNotMatch(spaceScroll, /(?:window\.)?confirm\s*\(/);
    assert.match(stylesheet, /\.mobile_post_author_actions\s*{[^}]*display:\s*inline-flex;/);
    assert.doesNotMatch(stylesheet, /\.mobile_post_author_actions\s*>\s*:not\([^)]*\)\s*{[^}]*display:\s*none;/);
});

test('Home targets discovery and discovery accepts temporary author filtering', () => {
    assert.match(utilitiesTemplate, /<a href="\/discovered">Home<\/a>/);
    assert.ok(spaceScroll.includes('return /^\\/(?:best|hot|new|discovered|random|top|rising|controversial|video-length)?\\/?$/.test(pathname);'));
});
