document.addEventListener("DOMContentLoaded", () => {
    const forms = Array.from(document.querySelectorAll("form[data-subscription-form]"));
    if (forms.length === 0) {
        return;
    }

    const nativeSubmit = HTMLFormElement.prototype.submit;
    const oneYearFromNow = () => new Date(Date.now() + 365 * 24 * 60 * 60 * 1000).toUTCString();

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

    const readCookie = (name) => {
        const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
        const match = document.cookie.match(new RegExp(`(?:^|; )${escaped}=([^;]*)`));
        return match ? decodeURIComponent(match[1]) : "";
    };

    const readShardedCookie = (baseName) => {
        let index = 0;
        let value = "";
        while (true) {
            const cookieName = index === 0 ? baseName : `${baseName}${index}`;
            const chunk = readCookie(cookieName);
            if (!chunk) {
                break;
            }
            value += chunk;
            index += 1;
        }
        return value;
    };

    const removeCookie = (name) => {
        document.cookie = `${name}=; expires=Thu, 01 Jan 1970 00:00:00 GMT; path=/; SameSite=Lax`;
    };

    const writeShardedCookie = (baseName, value) => {
        const chunkSize = 3800;
        const chunks = value ? value.match(new RegExp(`.{1,${chunkSize}}`, "g")) || [] : [];
        if (chunks.length === 0) {
            let staleIndex = 0;
            while (true) {
                const staleName = staleIndex === 0 ? baseName : `${baseName}${staleIndex}`;
                if (!readCookie(staleName)) {
                    break;
                }
                removeCookie(staleName);
                staleIndex += 1;
            }
            return;
        }

        chunks.forEach((chunk, index) => {
            const cookieName = index === 0 ? baseName : `${baseName}${index}`;
            document.cookie = `${cookieName}=${encodeURIComponent(chunk)}; expires=${oneYearFromNow()}; path=/; SameSite=Lax`;
        });

        let staleIndex = chunks.length;
        while (true) {
            const staleName = staleIndex === 0 ? baseName : `${baseName}${staleIndex}`;
            if (!readCookie(staleName)) {
                break;
            }
            removeCookie(staleName);
            staleIndex += 1;
        }
    };

    const readListCookie = (baseName) =>
        readShardedCookie(baseName)
            .split("+")
            .map((item) => item.trim())
            .filter(Boolean);

    const writeListCookie = (baseName, values) => {
        const sorted = Array.from(new Set(values)).sort((left, right) => left.localeCompare(right, undefined, { sensitivity: "base" }));
        writeShardedCookie(baseName, sorted.join("+"));
    };

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

    const writeFlairCookie = (subscriptions) => {
        if (!subscriptions.length) {
            writeShardedCookie("flair_subscriptions", "");
            return;
        }
        const normalized = subscriptions
            .map((entry) => ({ subreddit: entry.subreddit, flair: entry.flair }))
            .sort((left, right) => {
                const subredditOrder = left.subreddit.localeCompare(right.subreddit, undefined, { sensitivity: "base" });
                if (subredditOrder !== 0) {
                    return subredditOrder;
                }
                return left.flair.localeCompare(right.flair, undefined, { sensitivity: "base" });
            });
        writeShardedCookie("flair_subscriptions", base64UrlEncode(JSON.stringify(normalized)));
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
        for (const form of forms) {
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

    const toggleTarget = (target) => {
        for (const form of forms) {
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
        for (const form of forms) {
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

            let entries = readFlairCookie();
            entries = entries.filter((entry) => !(entry.subreddit === subreddit && entry.flair === flair));
            if (isEnabling) {
                entries.push({ subreddit, flair });
            }
            writeFlairCookie(entries);
            return;
        }

        const cookieValues = (form.dataset.cookieValues || "")
            .split(/[|+]/)
            .map((item) => item.trim())
            .filter(Boolean);
        if (cookieValues.length === 0) {
            throw new Error("Missing cookie values");
        }

        let values = readListCookie(cookieName);
        const normalized = new Map(values.map((value) => [value.toLowerCase(), value]));

        for (const value of cookieValues) {
            const key = value.toLowerCase();
            if (isEnabling) {
                normalized.set(key, value);
            } else {
                normalized.delete(key);
            }
        }

        values = Array.from(normalized.values());
        writeListCookie(cookieName, values);
    };

    for (const form of forms) {
        updateFormState(form, currentCookieState(form));
        form.addEventListener("submit", async (event) => {
            event.preventDefault();

            const target = form.dataset.subscriptionTarget || "";
            if (!target || form.dataset.subscriptionPending === "true") {
                return;
            }

            setPending(target, true);

            try {
                syncTargetStateFromCookies(target);
                mutateCookies(form);
                toggleTarget(target);
            } catch (_error) {
                setPending(target, false);
                nativeSubmit.call(form);
                return;
            }

            setPending(target, false);
        });
    }

    const syncAllForms = () => {
        for (const form of forms) {
            updateFormState(form, currentCookieState(form));
        }
    };

    window.addEventListener("focus", syncAllForms);
    document.addEventListener("visibilitychange", () => {
        if (!document.hidden) {
            syncAllForms();
        }
    });
});
