(() => {
    const nativeSubmit = HTMLFormElement.prototype.submit;
    const maxCookieShards = 16;
    const maxBookmarks = 200;
    const preferenceChannelName = "redlib-preferences";
    const preferenceChannel = "BroadcastChannel" in window ? new BroadcastChannel(preferenceChannelName) : null;
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

    let toastTimeout = null;
    const showPreferenceToast = (message) => {
        if (!message) {
            return;
        }

        let toast = document.getElementById("redlib_toast");
        if (!toast) {
            toast = document.createElement("div");
            toast.id = "redlib_toast";
            toast.className = "redlib_toast";
            toast.setAttribute("role", "status");
            toast.setAttribute("aria-live", "polite");
            document.body.append(toast);
        }

        toast.textContent = message;
        toast.classList.add("visible");

        if (toastTimeout !== null) {
            window.clearTimeout(toastTimeout);
        }
        toastTimeout = window.setTimeout(() => {
            toast.classList.remove("visible");
            toastTimeout = null;
        }, 2200);
    };

    const cookieValuesForForm = (form) =>
        (form.dataset.cookieValues || "")
            .split(/[|+]/)
            .map((item) => item.trim())
            .filter(Boolean);

    const sameValues = (left, right) => {
        if (left.length !== right.length) {
            return false;
        }
        const normalizedLeft = left.map((value) => value.toLowerCase()).sort();
        const normalizedRight = right.map((value) => value.toLowerCase()).sort();
        return normalizedLeft.every((value, index) => value === normalizedRight[index]);
    };

    const displayPreferenceValue = (value, cookieName) => {
        if (value.startsWith("u_")) {
            return `u/${value.slice(2)}`;
        }
        if (cookieName === "subscriptions" || cookieName === "filters") {
            return `r/${value}`;
        }
        return value;
    };

    const preferenceSuccessToast = (form, intent, isEnabling) => {
        if (form.dataset.successToast) {
            return form.dataset.successToast;
        }
        if (intent.mode !== "list" || intent.values.length !== 1) {
            return "";
        }

        const value = displayPreferenceValue(intent.values[0], intent.cookieName);
        if (intent.cookieName === "subscriptions") {
            return `${isEnabling ? "Followed" : "Unfollowed"} ${value}`;
        }
        if (intent.cookieName === "filters") {
            return `${isEnabling ? "Filtered" : "Unfiltered"} ${value}`;
        }
        return "";
    };

    const preferenceIntent = (form) => ({
        mode: form.dataset.cookieMode || "list",
        cookieName: form.dataset.cookieName || "subscriptions",
        values: cookieValuesForForm(form),
        subreddit: form.dataset.cookieSubreddit || "",
        flair: form.dataset.cookieFlair || "",
    });

    const formMatchesIntent = (form, intent) => {
        const mode = form.dataset.cookieMode || "list";
        if (mode !== intent.mode) {
            return false;
        }
        if (mode === "flair") {
            return form.dataset.cookieSubreddit === intent.subreddit && form.dataset.cookieFlair === intent.flair;
        }
        if (mode === "bookmarks") {
            return false;
        }
        return (form.dataset.cookieName || "subscriptions") === intent.cookieName && sameValues(cookieValuesForForm(form), intent.values || []);
    };

    const updateFormForIntent = (form, isEnabling) => {
        const stateOn = form.dataset.stateOn || "subscribed";
        const stateOff = form.dataset.stateOff || "unsubscribed";
        updateFormState(form, isEnabling ? stateOn : stateOff);
        removeSettingsFeedItem(form);
    };

    const applyCounterpartListIntent = (intent, isEnabling) => {
        if (!isEnabling || intent.mode !== "list") {
            return;
        }
        const counterpart =
            intent.cookieName === "subscriptions" ? "filters" : intent.cookieName === "filters" ? "subscriptions" : "";
        if (!counterpart) {
            return;
        }

        for (const form of getForms()) {
            const mode = form.dataset.cookieMode || "list";
            if (mode !== "list" || (form.dataset.cookieName || "subscriptions") !== counterpart) {
                continue;
            }
            if (!sameValues(cookieValuesForForm(form), intent.values || [])) {
                continue;
            }
            updateFormForIntent(form, false);
        }
    };

    const applyPreferenceIntent = (intent, isEnabling) => {
        for (const form of getForms()) {
            if (formMatchesIntent(form, intent)) {
                updateFormForIntent(form, isEnabling);
            }
        }
        applyCounterpartListIntent(intent, isEnabling);
    };

    const broadcastPreferenceIntent = (intent, isEnabling) => {
        const message = { type: "preference-intent", intent, isEnabling, time: Date.now() };
        preferenceChannel?.postMessage(message);
        try {
            localStorage.setItem(`${preferenceChannelName}-updated`, JSON.stringify(message));
        } catch (_error) {
            // Ignore storage failures; BroadcastChannel handles modern browsers.
        }
    };

    const currentCookieState = (form) => {
        const cookieMode = form.dataset.cookieMode || "list";
        const stateOn = form.dataset.stateOn || "subscribed";
        const stateOff = form.dataset.stateOff || "unsubscribed";

        if (cookieMode !== "bookmarks") {
            return form.dataset.subscriptionState || stateOff;
        }

        const bookmarkId = form.dataset.bookmarkId;
        const enabled = readBookmarksCookie().some((entry) => entry.i === bookmarkId);
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
            let successToast = "";

            try {
                const stateOn = form.dataset.stateOn || "subscribed";
                const isEnabling = form.dataset.subscriptionState !== stateOn;
                const intent = preferenceIntent(form);
                successToast = preferenceSuccessToast(form, intent, isEnabling);

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
                } else {
                    applyPreferenceIntent(intent, isEnabling);
                    broadcastPreferenceIntent(intent, isEnabling);
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
            showPreferenceToast(successToast);
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

    const hiddenListPreferenceForm = (value, cookieName, actionName, labels) => {
        const existing = getForms().find((form) => {
            const intent = preferenceIntent(form);
            return intent.mode === "list" && intent.cookieName === cookieName && sameValues(intent.values, [value]);
        });
        if (existing) {
            return existing;
        }

        const encodedValue = encodeURIComponent(value);
        const encodedRedirect = encodeURIComponent(`${window.location.pathname}${window.location.search}`);
        const form = document.createElement("form");
        form.hidden = true;
        form.method = "POST";
        form.action = `/r/${encodedValue}/${actionName}?redirect=${encodedRedirect}&canonical=${encodedValue}`;
        form.dataset.subscriptionForm = "";
        form.dataset.subscriptionTarget = `${actionName}:${value}`;
        form.dataset.subscriptionState = labels.stateOff;
        form.dataset.stateOn = labels.stateOn;
        form.dataset.stateOff = labels.stateOff;
        form.dataset.subscribeAction = `/r/${encodedValue}/${actionName}?redirect=${encodedRedirect}&canonical=${encodedValue}`;
        form.dataset.unsubscribeAction = `/r/${encodedValue}/un${actionName}?redirect=${encodedRedirect}&canonical=${encodedValue}`;
        form.dataset.subscribeLabel = labels.subscribeLabel;
        form.dataset.unsubscribeLabel = labels.unsubscribeLabel;
        form.dataset.subscribeClass = labels.subscribeClass;
        form.dataset.unsubscribeClass = labels.unsubscribeClass;
        form.dataset.cookieMode = "list";
        form.dataset.cookieName = cookieName;
        form.dataset.cookieValues = value;
        form.dataset.successToast = labels.successToast || "";

        const button = document.createElement("button");
        button.type = "submit";
        button.className = labels.subscribeClass;
        button.textContent = labels.subscribeLabel;
        form.append(button);
        document.body.append(form);

        return form;
    };

    const filterPreferenceValue = (value) => {
        if (!/^[A-Za-z0-9_-]+$/.test(value)) {
            return false;
        }

        const form = hiddenListPreferenceForm(value, "filters", "filter", {
            stateOn: "filtered",
            stateOff: "unfiltered",
            subscribeLabel: "Filter",
            unsubscribeLabel: "Unfilter",
            subscribeClass: "filter",
            unsubscribeClass: "unfilter",
            successToast: `Filtered ${value.startsWith("u_") ? `u/${value.slice(2)}` : value}`,
        });
        bindForm(form);

        if (currentCookieState(form) === (form.dataset.stateOn || "filtered")) {
            return false;
        }

        if (typeof form.requestSubmit === "function") {
            form.requestSubmit();
        } else {
            form.querySelector("button")?.click();
        }
        return true;
    };

    window.initializeSubscriptionForms = initializeSubscriptionForms;
    window.togglePrimarySubscription = togglePrimarySubscription;
    window.filterPreferenceValue = filterPreferenceValue;

    window.addEventListener("focus", syncAllForms);
    document.addEventListener("visibilitychange", () => {
        if (!document.hidden) {
            syncAllForms();
        }
    });
    preferenceChannel?.addEventListener("message", (event) => {
        if (event.data?.type === "preference-intent") {
            applyPreferenceIntent(event.data.intent, event.data.isEnabling);
        }
    });
    window.addEventListener("storage", (event) => {
        if (event.key !== `${preferenceChannelName}-updated` || !event.newValue) {
            return;
        }
        try {
            const message = JSON.parse(event.newValue);
            if (message.type === "preference-intent") {
                applyPreferenceIntent(message.intent, message.isEnabling);
            }
        } catch (_error) {
            // Ignore malformed storage events from older tabs.
        }
    });
    document.addEventListener("DOMContentLoaded", () => initializeSubscriptionForms(document));
})();
