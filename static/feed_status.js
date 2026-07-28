(() => {
    const pollIntervalMs = 2000;
    const requestTimeoutMs = 15000;

    const setBannerVisible = (banner, visible) => {
        if (!banner) {
            return;
        }

        banner.classList.toggle("is-hidden", !visible);
        if (visible) {
            banner.removeAttribute("aria-hidden");
        } else {
            banner.setAttribute("aria-hidden", "true");
        }
    };

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
            let postsHavePainted = false;
            let timerId = null;

            const showErrorState = () => {
                if (!banner) {
                    return;
                }
                setBannerVisible(banner, true);
                banner.textContent = "Updating the feed is taking longer than expected.";
                banner.classList.remove("warming");
            };

            const refreshDynamicContent = (dynamicRoot) => {
                window.refreshDynamicContent?.(dynamicRoot || document);
            };

            const hasVisiblePosts = () => {
                const currentPosts = container.querySelector("#posts");
                return !!currentPosts?.querySelector(".post");
            };

            const syncPaintedPostsState = () => {
                if (hasVisiblePosts()) {
                    postsHavePainted = true;
                }
                return postsHavePainted;
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
                const shouldInitialFillPosts =
                    nextPosts &&
                    currentPosts &&
                    !hasVisiblePosts() &&
                    currentPosts.querySelector(".post") === null &&
                    nextPosts.innerHTML !== currentPosts.innerHTML;

                if (shouldInitialFillPosts) {
                    currentPosts.innerHTML = nextPosts.innerHTML;
                    refreshDynamicContent(currentPosts);
                    postsHavePainted = hasVisiblePosts();
                }

                if ((!currentPosts || !nextPosts) && !hasVisiblePosts()) {
                    container.replaceChildren(...nextRoot.childNodes);
                    refreshDynamicContent(container);
                    postsHavePainted = hasVisiblePosts();
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
                    const hasStatusMessage = status.status_message.trim().length > 0;
                    setBannerVisible(banner, hasStatusMessage);
                    banner.textContent = status.status_message;
                    banner.classList.toggle("warming", !!status.warming);
                }

                const shouldRefreshFragment =
                    !syncPaintedPostsState() && (lastRefreshToken === null || lastRefreshToken !== status.refresh_token);

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
