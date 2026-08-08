const assert = require('node:assert/strict');
const fs = require('node:fs');
const test = require('node:test');
const vm = require('node:vm');

const script = fs.readFileSync('static/discovery_navigation.js', 'utf8');

class ClassList {
    constructor(...names) {
        this.names = new Set(names);
    }

    add(name) { this.names.add(name); }
    remove(name) { this.names.delete(name); }
    contains(name) { return this.names.has(name); }
}

function navigationFixture({ markerCount = 30, targetIndexes = [18, 24], currentPage = '1', hash = '' } = {}) {
    const listeners = new Map();
    const queuedFrames = [];
    const mutationObservers = [];
    const posts = {};
    const items = {
        clientHeight: 240,
        scrollHeight: markerCount * 24,
        scrollTop: 0,
        getBoundingClientRect: () => ({ top: 100, bottom: 100 + items.clientHeight, height: items.clientHeight }),
    };
    const targets = new Map();
    const markers = Array.from({ length: markerCount }, (_, index) => {
        const id = `post-${index}`;
        const page = index < 15 ? '0' : '1';
        const marker = {
            dataset: { discoveryTarget: id, discoveryPage: page },
            classList: new ClassList('discovery_navigation_marker'),
            attrs: new Map([['href', page === currentPage ? `#${id}` : `/discovered?cache_page=${page}#${id}`]]),
            hidden: false,
            isConnected: true,
            addEventListener() {},
            getAttribute(name) { return this.attrs.get(name); },
            setAttribute(name, value) { this.attrs.set(name, value); },
            removeAttribute(name) { this.attrs.delete(name); },
            remove() { this.isConnected = false; },
            getBoundingClientRect() {
                const top = 100 + (index * 24) - items.scrollTop;
                return { top, bottom: top + 24, height: 24 };
            },
        };
        if (targetIndexes.includes(index)) {
            targets.set(id, {
                id,
                hidden: false,
                isConnected: true,
                top: index === 12 ? -200 : index === 18 ? 80 : 500,
                getBoundingClientRect() { return { top: this.top }; },
                scrollIntoView() {},
                closest(selector) { return selector === '[hidden]' && this.hidden ? this : null; },
                querySelectorAll() { return []; },
            });
        }
        return marker;
    });
    const navigation = {
        dataset: { discoveryCurrentPage: currentPage },
        isConnected: true,
        querySelectorAll: () => markers,
        querySelector: (selector) => selector === '.discovery_navigation_items' ? items : null,
        addEventListener() {},
        contains: () => true,
    };
    const document = {
        documentElement: { classList: new ClassList('fixed_navbar') },
        querySelectorAll: () => [navigation],
        querySelector: (selector) => selector === '#posts' ? posts : null,
        getElementById: (id) => targets.get(id) || null,
        addEventListener(name, listener) {
            if (name === 'DOMContentLoaded') {
                listeners.set(name, listener);
            }
        },
        elementFromPoint: () => null,
    };
    const window = {
        location: { hash, assign() {} },
        history: { replaceState() {} },
        matchMedia: () => ({ matches: false }),
        requestAnimationFrame(callback) {
            queuedFrames.push(callback);
            return queuedFrames.length;
        },
        addEventListener(name, listener) { listeners.set(name, listener); },
        removeEventListener(name, listener) {
            if (listeners.get(name) === listener) {
                listeners.delete(name);
            }
        },
        clearTimeout() {},
        setTimeout() { return 1; },
        MutationObserver: class {
            constructor(callback) {
                this.callback = callback;
                this.disconnected = false;
                mutationObservers.push(this);
            }
            observe() {}
            disconnect() { this.disconnected = true; }
        },
    };
    const context = {
        document,
        window,
        getComputedStyle: () => ({ scrollPaddingTop: '100px' }),
        ResizeObserver: class { observe() {} disconnect() {} },
    };
    vm.runInNewContext(script, context);
    window.initializeDiscoveryNavigation(document);
    return { items, listeners, markers, mutationObservers, navigation, queuedFrames, targets };
}

test('initial activation uses the first rendered target and only scrolls the rail viewport', () => {
    const { items, markers } = navigationFixture({ currentPage: '0', targetIndexes: [12] });

    assert.equal(markers[12].attrs.get('aria-current'), 'location');
    assert.equal(items.scrollTop, 180);
    assert.equal(markers[29].attrs.has('aria-current'), false);
});

test('scroll tracking keeps the last profile boundary that crossed the header', () => {
    const { items, listeners, markers, queuedFrames, targets } = navigationFixture();

    queuedFrames.shift()();
    assert.equal(markers[18].attrs.get('aria-current'), 'location');
    assert.equal(markers[24].attrs.has('aria-current'), false);
    assert.equal(items.scrollTop, 324);

    targets.get('post-24').top = 99;
    listeners.get('scroll')();
    queuedFrames.shift()();
    assert.equal(markers[18].attrs.has('aria-current'), false);
    assert.equal(markers[24].attrs.get('aria-current'), 'location');
    assert.equal(items.scrollTop, 468);
});

test('cross-page hash activation starts at the landed target and centers its marker', () => {
    const { items, markers } = navigationFixture({ hash: '#post-24' });

    assert.equal(markers[24].attrs.get('aria-current'), 'location');
    assert.equal(markers[12].attrs.has('aria-current'), false);
    assert.equal(items.scrollTop, 468);
});

test('a projected marker from another page cannot hijack a matching rendered target', () => {
    const { markers } = navigationFixture({ targetIndexes: [12, 18] });

    assert.equal(markers[12].attrs.has('aria-current'), false);
    assert.equal(markers[18].attrs.get('aria-current'), 'location');
});

test('viewport resize recenters the active marker even when its target is unchanged', () => {
    const { items, listeners, queuedFrames } = navigationFixture();

    queuedFrames.shift()();
    assert.equal(items.scrollTop, 324);

    items.clientHeight = 120;
    listeners.get('resize')();
    queuedFrames.shift()();
    assert.equal(items.scrollTop, 384);
});

test('a replaced discovery fragment releases its stale scroll listeners', () => {
    const { listeners, mutationObservers, navigation, queuedFrames } = navigationFixture();

    navigation.isConnected = false;
    listeners.get('scroll')();

    assert.equal(listeners.has('scroll'), false);
    assert.equal(listeners.has('resize'), false);
    assert.equal(mutationObservers[0].disconnected, true);
    assert.equal(queuedFrames.length, 1);
});

test('a removed current-page target drops its marker and activates the first remaining boundary', () => {
    const { markers, mutationObservers, queuedFrames, targets } = navigationFixture();

    queuedFrames.shift()();
    assert.equal(markers[18].attrs.get('aria-current'), 'location');

    const removed = targets.get('post-18');
    targets.delete('post-18');
    removed.isConnected = false;
    mutationObservers[0].callback([{ removedNodes: [removed] }]);
    queuedFrames.shift()();

    assert.equal(markers[18].isConnected, false);
    assert.equal(markers[18].attrs.has('aria-current'), false);
    assert.equal(markers[24].attrs.get('aria-current'), 'location');
});

test('a hidden current-page target is skipped and restored without losing its marker', () => {
    const { markers, mutationObservers, queuedFrames, targets } = navigationFixture();

    queuedFrames.shift()();
    const target = targets.get('post-18');
    target.hidden = true;
    mutationObservers[0].callback([{ removedNodes: [] }]);
    queuedFrames.shift()();

    assert.equal(markers[18].hidden, true);
    assert.equal(markers[18].attrs.has('aria-current'), false);
    assert.equal(markers[24].attrs.get('aria-current'), 'location');

    target.hidden = false;
    mutationObservers[0].callback([{ removedNodes: [] }]);
    queuedFrames.shift()();

    assert.equal(markers[18].hidden, false);
    assert.equal(markers[18].attrs.get('aria-current'), 'location');
    assert.equal(markers[24].attrs.has('aria-current'), false);
});
