(() => {
    const pollIntervalMs = 2000;
    const requestTimeoutMs = 15000;

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

    const initializeFeedStatus = (root = document) => {
        const containers = Array.from(root.querySelectorAll?.("#feed_content[data-feed-session-id]") || []).filter(
            (container) => container.dataset.feedStatusBound !== "true"
        );

        for (const container of containers) {
            const statusUrl = container.dataset.feedStatusUrl;
            const fragmentUrl = container.dataset.feedFragmentUrl;
            if (!statusUrl || !fragmentUrl) {
                continue;
            }

            container.dataset.feedStatusBound = "true";

            const banner = document.querySelector(".feed_status_banner");
            let lastRefreshToken = null;
            let lastFragmentHtml = null;
            let timerId = null;

            const showErrorState = () => {
                if (!banner) {
                    return;
                }
                banner.hidden = false;
                banner.textContent = "Updating the feed is taking longer than expected.";
                banner.classList.remove("warming");
            };

            const refreshDynamicContent = (dynamicRoot) => {
                window.refreshDynamicContent?.(dynamicRoot || document);
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
                // Once the user can see posts for the current page, keep that list stable.
                // We still update the footer/banner so navigation and warmup state stay current.
                const shouldReplaceVisiblePosts =
                    nextPosts &&
                    currentPosts &&
                    currentPosts.children.length === 0 &&
                    nextPosts.innerHTML !== currentPosts.innerHTML;

                if (shouldReplaceVisiblePosts) {
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
                    banner.hidden = false;
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
        }
    };

    window.initializeFeedStatus = initializeFeedStatus;
    document.addEventListener("DOMContentLoaded", () => initializeFeedStatus(document));
})();
