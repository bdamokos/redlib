(() => {
    const gridSelector = "[data-suggested-profile-group]";
    const moreSelector = "[data-suggested-profiles-more]";
    const cardSelector = "[data-suggested-profile]";
    const statusSelector = "[data-suggested-profiles-status]";
    const activeCardClass = "is-keyboard-active";
    let keyboardNavigationBound = false;

    const cardKey = (card) => (card.dataset.suggestedProfile || "").toLowerCase();
    const cards = () => Array.from(document.querySelectorAll(cardSelector));

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

    const activeCardIndex = (profileCards) => {
        const focusedCard = document.activeElement?.closest?.(cardSelector);
        if (focusedCard) {
            return profileCards.indexOf(focusedCard);
        }

        const activeCard = document.querySelector(`${cardSelector}.${activeCardClass}`);
        if (activeCard) {
            return profileCards.indexOf(activeCard);
        }

        return -1;
    };

    const visibleCardIndex = (profileCards) => {
        const viewportAnchor = Math.max(0, Math.round(window.innerHeight * 0.18));
        const firstVisibleIndex = profileCards.findIndex((card) => card.getBoundingClientRect().bottom > viewportAnchor);
        return firstVisibleIndex >= 0 ? firstVisibleIndex : profileCards.length - 1;
    };

    const navigateCards = (direction) => {
        const profileCards = cards();
        if (profileCards.length === 0) {
            return;
        }

        const activeIndex = activeCardIndex(profileCards);
        const currentIndex = activeIndex >= 0 ? activeIndex : visibleCardIndex(profileCards);
        const targetIndex = activeIndex >= 0 ? currentIndex + direction : currentIndex + Math.min(direction, 0);
        const nextIndex = Math.max(0, Math.min(profileCards.length - 1, targetIndex));
        focusCard(profileCards[nextIndex]);
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
            link.textContent = "Loading more profiles...";
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
                throw new Error("Missing suggested profile grid");
            }

            const appended = appendNewCards(currentGrid, nextGrid);
            if (appended === 0) {
                throw new Error("No new suggested profile cards");
            }

            replaceLoadMore(nav, nextDoc);
            window.refreshDynamicContent?.(currentGrid);
            window.history?.replaceState?.(window.history.state, "", link.href);
        } catch (error) {
            window.console?.warn?.("Unable to load suggested profiles inline", error);
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

    function initialize(root = document) {
        initializeCardNavigation(root);

        for (const link of Array.from(root.querySelectorAll(`${moreSelector} a`))) {
            if (link.dataset.suggestedProfilesBound === "true") {
                continue;
            }
            link.dataset.suggestedProfilesBound = "true";
            link.addEventListener("click", handleClick);
        }
    }

    window.initializeSuggestedProfiles = initialize;
    document.addEventListener("DOMContentLoaded", () => initialize(document));
})();
