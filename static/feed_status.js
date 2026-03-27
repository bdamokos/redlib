document.addEventListener("DOMContentLoaded", () => {
    const container = document.querySelector("#feed_content[data-feed-session-id]");
    if (!container) {
        return;
    }

    const statusUrl = container.dataset.feedStatusUrl;
    const fragmentUrl = container.dataset.feedFragmentUrl;
    if (!statusUrl || !fragmentUrl) {
        return;
    }

    const banner = document.querySelector(".feed_status_banner");
    const pollIntervalMs = 2000;
    const requestTimeoutMs = 15000;
    let lastRefreshToken = null;
    let lastFragmentHtml = null;

    const fetchWithTimeout = async (url, options = {}) => {
        const controller = new AbortController();
        const timeoutId = window.setTimeout(() => controller.abort(), requestTimeoutMs);

        try {
            return await fetch(url, {
                ...options,
                signal: controller.signal,
            });
        } finally {
            window.clearTimeout(timeoutId);
        }
    };

    const showErrorState = () => {
        if (banner) {
            banner.textContent = "Updating the feed is taking longer than expected.";
            banner.classList.remove("warming");
        }
    };

    const refreshDynamicContent = (root) => {
        window.refreshDynamicContent?.(root || document);
    };

    const updateFeedContent = (html) => {
        if (lastFragmentHtml === html) {
            return;
        }

        const fragmentTemplate = document.createElement("template");
        fragmentTemplate.innerHTML = html.trim();
        const nextRoot = fragmentTemplate.content.firstElementChild;
        if (!nextRoot) {
            return;
        }

        const nextPosts = nextRoot.querySelector("#posts");
        const currentPosts = container.querySelector("#posts");
        if (nextPosts && currentPosts && nextPosts.innerHTML !== currentPosts.innerHTML) {
            currentPosts.innerHTML = nextPosts.innerHTML;
            refreshDynamicContent(currentPosts);
        }

        const nextFooter = nextRoot.querySelector("footer");
        const currentFooter = container.querySelector("footer");
        if (nextFooter && currentFooter && nextFooter.innerHTML !== currentFooter.innerHTML) {
            currentFooter.innerHTML = nextFooter.innerHTML;
        }

        if (!currentPosts || !nextPosts) {
            container.innerHTML = html;
            refreshDynamicContent(container);
        }

        lastFragmentHtml = html;
    };

    const refresh = async () => {
        const statusResponse = await fetchWithTimeout(statusUrl, {
            credentials: "same-origin",
            headers: {
                "Cache-Control": "no-store",
            },
        }).catch(() => null);
        if (!statusResponse || !statusResponse.ok) {
            showErrorState();
            return false;
        }

        const status = await statusResponse.json();
        if (banner && typeof status.status_message === "string") {
            banner.textContent = status.status_message;
            banner.classList.toggle("warming", !!status.warming);
        }

        const shouldRefreshFragment = lastRefreshToken === null || lastRefreshToken !== status.refresh_token;

        if (shouldRefreshFragment) {
            const fragmentResponse = await fetchWithTimeout(fragmentUrl, {
                credentials: "same-origin",
                headers: {
                    "Cache-Control": "no-store",
                },
            }).catch(() => null);

            if (fragmentResponse && fragmentResponse.ok) {
                updateFeedContent(await fragmentResponse.text());
                lastRefreshToken = status.refresh_token;
            } else {
                showErrorState();
            }
        }

        return !!status.warming;
    };

    let timerId = null;
    const tick = async () => {
        const shouldContinue = await refresh();
        if (shouldContinue) {
            timerId = window.setTimeout(tick, pollIntervalMs);
        } else if (timerId) {
            window.clearTimeout(timerId);
            timerId = null;
        }
    };

    void tick();
});
