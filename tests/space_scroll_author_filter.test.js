const assert = require("node:assert/strict");
const fs = require("node:fs");
const test = require("node:test");
const vm = require("node:vm");

function loadAuthorFilter(confirmResult) {
    class Element {
        constructor() {
            this.attributes = new Map();
            this.hidden = false;
            this.previousElementSibling = null;
            this.classList = {
                contains: (name) => name === "post",
                add() {},
                remove() {},
            };
        }

        getAttribute(name) {
            return this.attributes.get(name) || null;
        }

        setAttribute(name, value) {
            this.attributes.set(name, value);
        }

        removeAttribute(name) {
            this.attributes.delete(name);
        }

        matches(selector) {
            return selector.includes("data-permanent-author-filtered")
                && this.getAttribute("data-permanent-author-filtered") === "true";
        }

        querySelectorAll() {
            return [];
        }

        dispatchEvent() {}
    }

    class HTMLElement extends Element {}
    class HTMLAnchorElement extends Element {
        constructor(pathname) {
            super();
            this.pathname = pathname;
        }
    }
    class HTMLButtonElement extends HTMLElement {}

    const authorLink = new HTMLAnchorElement("/u/Mixed_Case-User");
    const makePost = () => {
        const post = new HTMLElement();
        post.querySelector = (selector) => selector === ".post_author[href]" ? authorLink : null;
        return post;
    };
    const posts = [makePost(), makePost()];
    const calls = [];
    const document = {
        addEventListener() {},
        querySelectorAll(selector) {
            return selector === ".post" ? posts : [];
        },
    };
    const window = {
        addEventListener() {},
        confirm: () => confirmResult,
        filterPreferenceValue(value) {
            calls.push(value);
            return true;
        },
        location: { pathname: "/", href: "https://example.test/" },
    };
    const context = vm.createContext({
        BroadcastChannel: undefined,
        CustomEvent: class {},
        Element,
        HTMLAnchorElement,
        HTMLButtonElement,
        HTMLElement,
        document,
        window,
    });
    const source = fs.readFileSync("static/space_scroll.js", "utf8")
        + "\nglobalThis.authorFilterTestApi = { filterPostAuthor, getPostAuthorPreferenceValue };";
    vm.runInContext(source, context);

    return { api: context.authorFilterTestApi, calls, posts };
}

test("confirmed author filter uses normalized existing preference semantics and hides matching cards", () => {
    const { api, calls, posts } = loadAuthorFilter(true);

    assert.equal(api.getPostAuthorPreferenceValue(posts[0]), "u_Mixed_Case-User");
    assert.equal(api.filterPostAuthor(posts[0]), true);
    assert.deepEqual(calls, ["u_Mixed_Case-User"]);
    assert.equal(posts[0].hidden, true);
    assert.equal(posts[1].hidden, true);
    assert.equal(posts[0].getAttribute("data-permanent-author-filtered"), "true");
});

test("cancelled author filter neither persists nor changes the visible feed", () => {
    const { api, calls, posts } = loadAuthorFilter(false);

    assert.equal(api.filterPostAuthor(posts[0]), false);
    assert.deepEqual(calls, []);
    assert.equal(posts[0].hidden, false);
    assert.equal(posts[1].hidden, false);
});
