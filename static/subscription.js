(() => {
    const nativeSubmit = HTMLFormElement.prototype.submit;
    const maxCookieShards = 16;
    const maxBookmarks = 200;
    const oneYearFromNow = () => new Date(Date.now() + 365 * 24 * 60 * 60 * 1000).toUTCString();
    const getForms = () => Array.from(document.querySelectorAll("form[data-subscription-form]"));

    const base64UrlEncode = (value) =>
        btoa(unescape(encodeURIComponent(value)))
            .replace(/\+/g, "-")
            .replace(/\//g, "_")
            .replace(/=+$/g, "");

    const base64UrlDecode = (value) => {
        const normalized = value.replace(/-/g, "+").replace(/_/g, "/");
        const padded = normalized + "=".repeat((4 - (normalized.length % 4 || 4)) % 4);
        return decodeURIComponent(escape(atob(padded)));
    };

    /** One parse of `document.cookie` so shard reads cannot mix revisions from concurrent writers. */
    const parseCookieSnapshot = (header) => {
        const jar = new Map();
        if (!header) {
            return jar;
        }
        for (const part of header.split(/;\s*/)) {
            if (!part) {
                continue;
            }
            const eq = part.indexOf("=");
            if (eq < 1) {
                continue;
            }
            const name = part.slice(0, eq);
            const raw = part.slice(eq + 1);
            try {
                jar.set(name, decodeURIComponent(raw));
            } catch (_error) {
                jar.set(name, raw);
            }
        }
        return jar;
    };

    const readShardedCookie = (baseName) => {
        const jar = parseCookieSnapshot(document.cookie);
        let index = 0;
        let value = "";
        while (true) {
            const cookieName = index === 0 ? baseName : `${baseName}${index}`;
            const chunk = jar.get(cookieName) ?? "";
            if (!chunk) {
                break;
            }
            value += chunk;
            index += 1;
        }
        return value;
    };

    /** Max shard index present in `jar` (0 for `name`, 1 for `name1`, …). */
    const highestPresentShardIndex = (jar, baseName) => {
        let highest = -1;
        for (let i = 0; i < maxCookieShards; i += 1) {
            const cookieName = i === 0 ? baseName : `${baseName}${i}`;
            if (jar.has(cookieName) && (jar.get(cookieName) ?? "") !== "") {
                highest = i;
            }
        }
        return highest;
    };

    const listTokensFromRaw = (raw) =>
        raw
            .split("+")
            .map((item) => item.trim())
            .filter(Boolean);

    const normalizeListCookieString = (raw) => {
        const tokens = listTokensFromRaw(raw);
        const map = new Map(tokens.map((t) => [t.toLowerCase(), t]));
        return Array.from(map.values())
            .sort((left, right) => left.localeCompare(right, undefined, { sensitivity: "base" }))
            .join("+");
    };

    const listCookieStringsEqual = (a, b) => normalizeListCookieString(a) === normalizeListCookieString(b);

    const sortFlairEntries = (entries) =>
        entries
            .map((entry) => ({ subreddit: entry.subreddit, flair: entry.flair }))
            .sort((left, right) => {
                const subredditOrder = left.subreddit.localeCompare(right.subreddit, undefined, { sensitivity: "base" });
                if (subredditOrder !== 0) {
                    return subredditOrder;
                }
                return left.flair.localeCompare(right.flair, undefined, { sensitivity: "base" });
            });

    const canonicalFlairCookieString = (raw) => {
        if (!raw) {
            return "";
        }
        try {
            const entries = JSON.parse(base64UrlDecode(raw));
            if (!Array.isArray(entries)) {
                return raw;
            }
            return base64UrlEncode(JSON.stringify(sortFlairEntries(entries)));
        } catch (_error) {
            return raw;
        }
    };

    const flairCookieStringsEqual = (a, b) => canonicalFlairCookieString(a) === canonicalFlairCookieString(b);

    const normalizeBookmarkEntries = (entries) =>
        entries
            .filter((entry) => entry && entry.i && entry.p)
            .sort((left, right) => (right.b || 0) - (left.b || 0) || right.i.localeCompare(left.i))
            .slice(0, maxBookmarks);

    const canonicalBookmarkCookieString = (raw) => {
        if (!raw) {
            return "";
        }
        try {
            const arr = JSON.parse(base64UrlDecode(raw));
            if (!Array.isArray(arr)) {
                return raw;
            }
            return JSON.stringify(normalizeBookmarkEntries(arr));
        } catch (_error) {
            return raw;
        }
    };

    const bookmarkCookieStringsEqual = (a, b) => canonicalBookmarkCookieString(a) === canonicalBookmarkCookieString(b);

    /** Optimistic compare-and-retry so concurrent tabs are less likely to drop writes. */
    const readModifyWriteSharded = (baseName, applyIntent, areEquivalent, maxRetries = 8) => {
        for (let attempt = 0; attempt < maxRetries; attempt += 1) {
            const before = readShardedCookie(baseName);
            const next = applyIntent(before);
            const mid = readShardedCookie(baseName);
            if (mid !== before) {
                continue;
            }
            writeShardedCookie(baseName, next);
            const after = readShardedCookie(baseName);
            if (areEquivalent(after, next)) {
                return;
            }
        }
        const before = readShardedCookie(baseName);
        const next = applyIntent(before);
        writeShardedCookie(baseName, next);
    };

    const removeCookie = (name) => {
        document.cookie = `${name}=; expires=Thu, 01 Jan 1970 00:00:00 GMT; path=/; SameSite=Lax`;
    };

    const writeShardedCookie = (baseName, value) => {
        const chunkSize = 3800;
        const chunks = value ? value.match(new RegExp(`.{1,${chunkSize}}`, "g")) || [] : [];
        const jar = parseCookieSnapshot(document.cookie);
        const oldUpper = highestPresentShardIndex(jar, baseName);

        if (chunks.length === 0) {
            const end = oldUpper >= 0 ? oldUpper : maxCookieShards - 1;
            for (let staleIndex = 0; staleIndex <= end; staleIndex += 1) {
                const staleName = staleIndex === 0 ? baseName : `${baseName}${staleIndex}`;
                removeCookie(staleName);
            }
            return;
        }

        chunks.forEach((chunk, index) => {
            const cookieName = index === 0 ? baseName : `${baseName}${index}`;
            document.cookie = `${cookieName}=${encodeURIComponent(chunk)}; expires=${oneYearFromNow()}; path=/; SameSite=Lax`;
        });

        const lastNew = chunks.length - 1;
        const capStale = oldUpper >= 0 ? oldUpper : lastNew;
        if (lastNew < capStale) {
            for (let staleIndex = lastNew + 1; staleIndex <= capStale; staleIndex += 1) {
                const staleName = staleIndex === 0 ? baseName : `${baseName}${staleIndex}`;
                removeCookie(staleName);
            }
        }
    };

    const readListCookie = (baseName) => listTokensFromRaw(readShardedCookie(baseName));

    const readFlairCookie = () => {
        const encoded = readShardedCookie("flair_subscriptions");
        if (!encoded) {
            return [];
        }
        try {
            return JSON.parse(base64UrlDecode(encoded));
        } catch (_error) {
            return [];
        }
    };

    const readBookmarksCookie = () => {
        const encoded = readShardedCookie("bookmarks");
        if (!encoded) {
            return [];
        }
        try {
            return JSON.parse(base64UrlDecode(encoded));
        } catch (_error) {
            return [];
        }
    };

    const writeBookmarksCookie = (entries) => {
        readModifyWriteSharded(
            "bookmarks",
            () => {
                const normalized = normalizeBookmarkEntries(entries);
                return normalized.length === 0 ? "" : base64UrlEncode(JSON.stringify(normalized));
            },
            bookmarkCookieStringsEqual,
        );
    };

    const updateFormState = (form, nextState) => {
        const button = form.querySelector("button");
        if (!button) {
            return;
        }

        const stateOn = form.dataset.stateOn || "subscribed";
        const isOn = nextState === stateOn;
        const buttonClass = isOn ? form.dataset.unsubscribeClass : form.dataset.subscribeClass;
        const buttonLabel = isOn ? form.dataset.unsubscribeLabel : form.dataset.subscribeLabel;
        const action = isOn ? form.dataset.unsubscribeAction : form.dataset.subscribeAction;

        form.dataset.subscriptionState = nextState;

        if (action) {
            form.action = action;
        }

        if (buttonClass) {
            button.className = buttonClass;
        }

        if (buttonLabel) {
            button.textContent = buttonLabel;
        }
    };

    const setPending = (target, pending) => {
        for (const form of getForms()) {
            if (form.dataset.subscriptionTarget !== target) {
                continue;
            }
            form.dataset.subscriptionPending = pending ? "true" : "false";
            const button = form.querySelector("button");
            if (button) {
                button.disabled = pending;
            }
        }
    };

    const removeSettingsFeedItem = (form) => {
        const removeWhenState = form.dataset.removeWhenState;
        if (!removeWhenState || form.dataset.subscriptionState !== removeWhenState) {
            return;
        }

        const item = form.closest("[data-settings-feed-item]");
        if (!item) {
            return;
        }

        const group = item.closest("[data-settings-feed-group]");
        item.remove();

        if (group && !group.querySelector("[data-settings-feed-item]")) {
            group.remove();
        }
    };

    const toggleTarget = (target) => {
        for (const form of getForms()) {
            if (form.dataset.subscriptionTarget !== target) {
                continue;
            }
            const stateOn = form.dataset.stateOn || "subscribed";
            const stateOff = form.dataset.stateOff || "unsubscribed";
            const nextState = form.dataset.subscriptionState === stateOn ? stateOff : stateOn;
            updateFormState(form, nextState);
        }
    };

    const currentCookieState = (form) => {
        const cookieMode = form.dataset.cookieMode || "list";
        const cookieName = form.dataset.cookieName || "subscriptions";
        const stateOn = form.dataset.stateOn || "subscribed";
        const stateOff = form.dataset.stateOff || "unsubscribed";

        if (cookieMode === "flair") {
            const subreddit = form.dataset.cookieSubreddit;
            const flair = form.dataset.cookieFlair;
            const enabled = readFlairCookie().some((entry) => entry.subreddit === subreddit && entry.flair === flair);
            return enabled ? stateOn : stateOff;
        }

        if (cookieMode === "bookmarks") {
            const bookmarkId = form.dataset.bookmarkId;
            const enabled = readBookmarksCookie().some((entry) => entry.i === bookmarkId);
            return enabled ? stateOn : stateOff;
        }

        const cookieValues = (form.dataset.cookieValues || "")
            .split(/[|+]/)
            .map((item) => item.trim())
            .filter(Boolean);
        if (cookieValues.length === 0) {
            return form.dataset.subscriptionState || stateOff;
        }

        const values = readListCookie(cookieName).map((value) => value.toLowerCase());
        const enabled = cookieValues.every((value) => values.includes(value.toLowerCase()));
        return enabled ? stateOn : stateOff;
    };

    const syncTargetStateFromCookies = (target) => {
        for (const form of getForms()) {
            if (form.dataset.subscriptionTarget !== target) {
                continue;
            }
            updateFormState(form, currentCookieState(form));
        }
    };

    const mutateCookies = (form) => {
        const cookieMode = form.dataset.cookieMode || "list";
        const cookieName = form.dataset.cookieName || "subscriptions";
        const stateOn = form.dataset.stateOn || "subscribed";
        const isEnabling = form.dataset.subscriptionState !== stateOn;

        if (cookieMode === "flair") {
            const subreddit = form.dataset.cookieSubreddit;
            const flair = form.dataset.cookieFlair;
            if (!subreddit || !flair) {
                throw new Error("Missing flair subscription metadata");
            }

            readModifyWriteSharded(
                "flair_subscriptions",
                (beforeRaw) => {
                    let entries = [];
                    if (beforeRaw) {
                        try {
                            entries = JSON.parse(base64UrlDecode(beforeRaw));
                        } catch (_error) {
                            entries = [];
                        }
                    }
                    if (!Array.isArray(entries)) {
                        entries = [];
                    }
                    entries = entries.filter((entry) => !(entry.subreddit === subreddit && entry.flair === flair));
                    if (isEnabling) {
                        entries.push({ subreddit, flair });
                    }
                    if (entries.length === 0) {
                        return "";
                    }
                    return base64UrlEncode(JSON.stringify(sortFlairEntries(entries)));
                },
                flairCookieStringsEqual,
            );
            return;
        }

        if (cookieMode === "bookmarks") {
            if (!form.dataset.bookmarkId) {
                throw new Error("Missing bookmark metadata");
            }
            return;
        }

        const cookieValues = (form.dataset.cookieValues || "")
            .split(/[|+]/)
            .map((item) => item.trim())
            .filter(Boolean);
        if (cookieValues.length === 0) {
            throw new Error("Missing cookie values");
        }

        readModifyWriteSharded(
            cookieName,
            (beforeRaw) => {
                const values = listTokensFromRaw(beforeRaw);
                const normalized = new Map(values.map((value) => [value.toLowerCase(), value]));

                for (const value of cookieValues) {
                    const key = value.toLowerCase();
                    if (isEnabling) {
                        normalized.set(key, value);
                    } else {
                        normalized.delete(key);
                    }
                }

                const sorted = Array.from(normalized.values()).sort((left, right) =>
                    left.localeCompare(right, undefined, { sensitivity: "base" }),
                );
                return sorted.join("+");
            },
            listCookieStringsEqual,
        );
    };

    const bindForm = (form) => {
        if (form.dataset.subscriptionBound === "true") {
            return;
        }

        form.dataset.subscriptionBound = "true";
        updateFormState(form, currentCookieState(form));
        form.addEventListener("submit", async (event) => {
            event.preventDefault();

            const target = form.dataset.subscriptionTarget || "";
            if (!target || form.dataset.subscriptionPending === "true") {
                return;
            }

            setPending(target, true);
            const requestAction = form.action;

            try {
                if (form.dataset.cookieMode !== "bookmarks") {
                    syncTargetStateFromCookies(target);
                    mutateCookies(form);
                    toggleTarget(target);
                }

                const response = await fetch(requestAction, {
                    method: form.method || "POST",
                    body:
                        (form.method || "POST").toUpperCase() === "GET"
                            ? undefined
                            : new URLSearchParams(new FormData(form)).toString(),
                    credentials: "same-origin",
                    headers: {
                        "X-Redlib-Ajax": "1",
                        "Content-Type": "application/x-www-form-urlencoded; charset=UTF-8",
                    },
                });

                if (!response.ok) {
                    throw new Error(`Subscription request failed: ${response.status}`);
                }

                if (form.dataset.cookieMode === "bookmarks") {
                    syncTargetStateFromCookies(target);
                }
            } catch (_error) {
                form.action = requestAction;
                setPending(target, false);
                if (form.dataset.cookieMode === "bookmarks") {
                    return;
                }
                nativeSubmit.call(form);
                return;
            }

            setPending(target, false);
            removeSettingsFeedItem(form);
        });
    };

    const syncAllForms = () => {
        for (const form of getForms()) {
            updateFormState(form, currentCookieState(form));
        }
    };

    const initializeSubscriptionForms = (root = document) => {
        const scopedForms = Array.from(root.querySelectorAll?.("form[data-subscription-form]") || []);
        for (const form of scopedForms) {
            bindForm(form);
        }
        syncAllForms();
    };

    const togglePrimarySubscription = () => {
        const form = document.querySelector(
            "#user_subscription form[data-subscription-form], #sub_subscription form[data-subscription-form]",
        );
        if (!form || form.dataset.subscriptionPending === "true") {
            return false;
        }

        bindForm(form);
        if (typeof form.requestSubmit === "function") {
            form.requestSubmit();
        } else {
            form.querySelector("button")?.click();
        }
        return true;
    };

    window.initializeSubscriptionForms = initializeSubscriptionForms;
    window.togglePrimarySubscription = togglePrimarySubscription;

    window.addEventListener("focus", syncAllForms);
    document.addEventListener("visibilitychange", () => {
        if (!document.hidden) {
            syncAllForms();
        }
    });
    document.addEventListener("DOMContentLoaded", () => initializeSubscriptionForms(document));
})();
