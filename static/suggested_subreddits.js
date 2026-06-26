(() => {
    const gridSelector = "[data-suggested-subreddit-group]";
    const moreSelector = "[data-suggested-subreddits-more]";
    const cardSelector = "[data-suggested-subreddit]";
    const ignoreSelector = "[data-suggested-subreddit-ignore]";
    const statusSelector = "[data-suggested-subreddits-status]";
    const activeCardClass = "is-keyboard-active";
    const ignoreCookieName = "suggested_subreddit_ignores";
    const maxCookieShards = 16;
    const ignoreTtlMs = 30 * 24 * 60 * 60 * 1000;
    let keyboardNavigationBound = false;

    const cardKey = (card) => (card.dataset.suggestedSubreddit || "").toLowerCase();
    const cards = () => Array.from(document.querySelectorAll(cardSelector));
    const oneYearFromNow = () => new Date(Date.now() + 365 * 24 * 60 * 60 * 1000).toUTCString();

    const parseCookieSnapshot = (header) => {
        const jar = new Map();
        if (!header) {
            return jar;
        }

        for (const part of header.split(/;\s*/)) {
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
        let value = "";
        for (let index = 0; index < maxCookieShards; index += 1) {
            const cookieName = index === 0 ? baseName : `${baseName}${index}`;
            const chunk = jar.get(cookieName) || "";
            if (!chunk) {
                break;
            }
            value += chunk;
        }
        return value;
    };

    const highestPresentShardIndex = (baseName) => {
        const jar = parseCookieSnapshot(document.cookie);
        let highest = -1;
        for (let index = 0; index < maxCookieShards; index += 1) {
            const cookieName = index === 0 ? baseName : `${baseName}${index}`;
            if (jar.has(cookieName) && (jar.get(cookieName) || "") !== "") {
                highest = index;
            }
        }
        return highest;
    };

    const removeCookie = (name) => {
        document.cookie = `${name}=; expires=Thu, 01 Jan 1970 00:00:00 GMT; path=/; SameSite=Lax`;
    };

    const writeShardedCookie = (baseName, value) => {
        const chunkSize = 3800;
        const chunks = value ? value.match(new RegExp(`.{1,${chunkSize}}`, "g")) || [] : [];
        const oldUpper = highestPresentShardIndex(baseName);

        if (chunks.length === 0) {
            const end = oldUpper >= 0 ? oldUpper : maxCookieShards - 1;
            for (let staleIndex = 0; staleIndex <= end; staleIndex += 1) {
                removeCookie(staleIndex === 0 ? baseName : `${baseName}${staleIndex}`);
            }
            return;
        }

        chunks.forEach((chunk, index) => {
            const cookieName = index === 0 ? baseName : `${baseName}${index}`;
            document.cookie = `${cookieName}=${encodeURIComponent(chunk)}; expires=${oneYearFromNow()}; path=/; SameSite=Lax`;
        });

        for (let staleIndex = chunks.length; staleIndex <= oldUpper; staleIndex += 1) {
            removeCookie(staleIndex === 0 ? baseName : `${baseName}${staleIndex}`);
        }
    };

    const ignoredSubreddits = () => {
        const now = Date.now();
        const entries = new Map();
        for (const entry of readShardedCookie(ignoreCookieName).split("+")) {
            const [name, ignoredAtSeconds] = entry.split(":");
            const ignoredAt = Number(ignoredAtSeconds) * 1000;
            const normalized = (name || "").trim().toLowerCase();
            if (!normalized || !Number.isFinite(ignoredAt) || now - ignoredAt > ignoreTtlMs) {
                continue;
            }
            entries.set(normalized, Math.floor(ignoredAt / 1000));
        }
        return entries;
    };

    const writeIgnoredSubreddits = (entries) => {
        const value = Array.from(entries.entries())
            .sort(([left], [right]) => left.localeCompare(right))
            .map(([name, ignoredAt]) => `${name}:${ignoredAt}`)
            .join("+");
        writeShardedCookie(ignoreCookieName, value);
    };

    const rememberIgnoredSubreddit = (name) => {
        const normalized = (name || "").trim().toLowerCase();
        if (!normalized) {
            return;
        }
        const entries = ignoredSubreddits();
        entries.set(normalized, Math.floor(Date.now() / 1000));
        writeIgnoredSubreddits(entries);
    };

    const isTextInputTarget = (target) =>
        target instanceof HTMLElement &&
        Boolean(target.closest("input, textarea, select, [contenteditable='true'], [contenteditable='']"));

    const clearActiveCards = () => {
        for (const card of cards()) {
            card.classList.remove(activeCardClass);
        }
    };

    const focusCard = (card) => {
        if (!card) {
            return;
        }

        clearActiveCards();
        card.classList.add(activeCardClass);
        card.focus({ preventScroll: true });
        card.scrollIntoView({ block: "center", inline: "nearest", behavior: "smooth" });
    };

    const activeCardIndex = (subredditCards) => {
        const focusedCard = document.activeElement?.closest?.(cardSelector);
        if (focusedCard) {
            return subredditCards.indexOf(focusedCard);
        }

        const activeCard = document.querySelector(`${cardSelector}.${activeCardClass}`);
        if (activeCard) {
            return subredditCards.indexOf(activeCard);
        }

        return -1;
    };

    const visibleCardIndex = (subredditCards) => {
        const viewportAnchor = Math.max(0, Math.round(window.innerHeight * 0.18));
        const firstVisibleIndex = subredditCards.findIndex((card) => card.getBoundingClientRect().bottom > viewportAnchor);
        return firstVisibleIndex >= 0 ? firstVisibleIndex : subredditCards.length - 1;
    };

    const navigateCards = (direction) => {
        const subredditCards = cards();
        if (subredditCards.length === 0) {
            return;
        }

        const activeIndex = activeCardIndex(subredditCards);
        const currentIndex = activeIndex >= 0 ? activeIndex : visibleCardIndex(subredditCards);
        const targetIndex = activeIndex >= 0 ? currentIndex + direction : currentIndex + Math.min(direction, 0);
        const nextIndex = Math.max(0, Math.min(subredditCards.length - 1, targetIndex));
        focusCard(subredditCards[nextIndex]);
    };

    const handleKeydown = (event) => {
        if (event.defaultPrevented || event.metaKey || event.ctrlKey || event.altKey || event.shiftKey || isTextInputTarget(event.target)) {
            return;
        }

        const key = event.key.toLowerCase();
        if (key === "j") {
            event.preventDefault();
            navigateCards(1);
        } else if (key === "k") {
            event.preventDefault();
            navigateCards(-1);
        }
    };

    const initializeCardNavigation = (root = document) => {
        for (const card of Array.from(root.querySelectorAll(cardSelector))) {
            if (!card.hasAttribute("tabindex")) {
                card.setAttribute("tabindex", "-1");
            }
        }

        if (!keyboardNavigationBound) {
            keyboardNavigationBound = true;
            document.addEventListener("keydown", handleKeydown);
        }
    };

    const setStatus = (nav, message) => {
        const status = nav.querySelector(statusSelector);
        if (status) {
            status.textContent = message;
        }
    };

    const setLoading = (nav, loading) => {
        nav.classList.toggle("is-loading", loading);
        nav.setAttribute("aria-busy", loading ? "true" : "false");

        const link = nav.querySelector("a");
        if (!link) {
            return;
        }

        if (loading) {
            link.dataset.originalText = link.textContent;
            link.textContent = "Loading more subreddits...";
            return;
        }

        if (link.dataset.originalText) {
            link.textContent = link.dataset.originalText;
            delete link.dataset.originalText;
        }
    };

    const appendNewCards = (currentGrid, nextGrid) => {
        const seen = new Set(Array.from(currentGrid.querySelectorAll(cardSelector)).map(cardKey).filter(Boolean));
        const fragment = document.createDocumentFragment();
        let appended = 0;

        for (const card of Array.from(nextGrid.querySelectorAll(cardSelector))) {
            const key = cardKey(card);
            if (!key || seen.has(key)) {
                continue;
            }
            seen.add(key);
            fragment.appendChild(card);
            appended += 1;
        }

        if (appended > 0) {
            currentGrid.appendChild(fragment);
        }

        return appended;
    };

    const removeIgnoredCards = (root = document) => {
        const ignored = ignoredSubreddits();
        if (ignored.size === 0) {
            writeIgnoredSubreddits(ignored);
            return;
        }

        for (const card of Array.from(root.querySelectorAll?.(cardSelector) || [])) {
            if (ignored.has(cardKey(card))) {
                card.remove();
            }
        }
        writeIgnoredSubreddits(ignored);
    };

    const replaceLoadMore = (nav, nextDoc) => {
        const nextMore = nextDoc.querySelector(moreSelector);
        if (nextMore) {
            nav.replaceWith(nextMore);
            initialize(nextMore.parentElement || document);
            return;
        }

        nav.remove();
    };

    const loadMore = async (link, nav) => {
        if (nav.dataset.loading === "true") {
            return;
        }

        nav.dataset.loading = "true";
        nav.classList.remove("has-error");
        setLoading(nav, true);
        setStatus(nav, "Loading...");

        try {
            const response = await fetch(link.href, {
                credentials: "same-origin",
                headers: { "X-Requested-With": "fetch" },
            });

            if (!response.ok) {
                throw new Error(`HTTP ${response.status}`);
            }

            const html = await response.text();
            const nextDoc = new DOMParser().parseFromString(html, "text/html");
            const currentGrid = document.querySelector(gridSelector);
            const nextGrid = nextDoc.querySelector(gridSelector);

            if (!currentGrid || !nextGrid) {
                throw new Error("Missing suggested subreddit grid");
            }

            const appended = appendNewCards(currentGrid, nextGrid);
            if (appended === 0) {
                throw new Error("No new suggested subreddit cards");
            }

            replaceLoadMore(nav, nextDoc);
            window.refreshDynamicContent?.(currentGrid);
            window.history?.replaceState?.(window.history.state, "", link.href);
        } catch (error) {
            window.console?.warn?.("Unable to load suggested subreddits inline", error);
            nav.classList.add("has-error");
            setLoading(nav, false);
            setStatus(nav, "Could not load more. Try again.");
            delete nav.dataset.loading;
        }
    };

    const handleClick = (event) => {
        if (!window.fetch || !window.DOMParser) {
            return;
        }

        const link = event.currentTarget;
        const nav = link.closest(moreSelector);
        if (!nav) {
            return;
        }

        event.preventDefault();
        loadMore(link, nav);
    };

    const handleIgnoreClick = (event) => {
        const button = event.currentTarget;
        const name = button.dataset.suggestedSubredditIgnore || "";
        const card = button.closest(cardSelector);
        rememberIgnoredSubreddit(name);
        card?.remove();
    };

    function initialize(root = document) {
        removeIgnoredCards(root);
        initializeCardNavigation(root);

        for (const link of Array.from(root.querySelectorAll(`${moreSelector} a`))) {
            if (link.dataset.suggestedSubredditsBound === "true") {
                continue;
            }
            link.dataset.suggestedSubredditsBound = "true";
            link.addEventListener("click", handleClick);
        }

        for (const button of Array.from(root.querySelectorAll(ignoreSelector))) {
            if (button.dataset.suggestedSubredditIgnoreBound === "true") {
                continue;
            }
            button.dataset.suggestedSubredditIgnoreBound = "true";
            button.addEventListener("click", handleIgnoreClick);
        }
    }

    window.initializeSuggestedSubreddits = initialize;
    document.addEventListener("DOMContentLoaded", () => initialize(document));
})();
