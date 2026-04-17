document.addEventListener("DOMContentLoaded", () => {
    const container = document.querySelector("#page_fragment[data-fragment-url]");
    if (!container) {
        return;
    }

    const fragmentUrl = container.dataset.fragmentUrl;
    if (!fragmentUrl) {
        return;
    }

    const loadingMessage = container.dataset.loadingMessage || "Loading the page...";
    const errorMessage = container.dataset.errorMessage || "Loading the page is taking longer than expected.";
    const timeoutMs = Number.parseInt(container.dataset.timeoutMs || "15000", 10);
    const maxAttempts = Number.parseInt(container.dataset.maxAttempts || "3", 10);
    const retryDelayMs = Number.parseInt(container.dataset.retryDelayMs || "1500", 10);
    const replaceSelector = container.dataset.replaceSelector || "";
    const banner = document.querySelector(".feed_status_banner");
    const cacheVersion = container.dataset.fragmentCacheVersion || "1";
    const cacheKey = `page-fragment:v${cacheVersion}:${fragmentUrl}`;
    const cacheEnabled = container.dataset.fragmentCacheEnabled !== "false";

    const readCachedFragment = () => {
        if (!cacheEnabled) {
            return null;
        }
        try {
            return window.sessionStorage.getItem(cacheKey);
        } catch (_) {
            return null;
        }
    };

    const writeCachedFragment = (html) => {
        if (!cacheEnabled) {
            return;
        }
        try {
            window.sessionStorage.setItem(cacheKey, html);
        } catch (_) {
            // Ignore sessionStorage failures.
        }
    };

    const replaceFragment = (html) => {
        if (replaceSelector) {
            const template = document.createElement("template");
            template.innerHTML = html.trim();
            const nextNode = template.content.querySelector(replaceSelector);
            const currentNode = document.querySelector(replaceSelector);
            if (nextNode && currentNode) {
                currentNode.replaceWith(nextNode);
                refreshDynamicContent(document);
                settleBannerState();
                return;
            }
        }

        container.outerHTML = html;
        refreshDynamicContent(document);
        settleBannerState();
    };

    const showErrorState = () => {
        window.console?.warn?.("page_fragment: failed to load", {
            fragmentUrl,
            replaceSelector,
        });

        if (banner) {
            banner.textContent = errorMessage;
            banner.classList.remove("warming");
        }

        const target = replaceSelector ? document.querySelector(replaceSelector) : container;
        if (!target) {
            return;
        }

        target.innerHTML = `
            <center>${errorMessage}</center>
            <center><p><a href="${fragmentUrl}">Try loading the content again</a></p></center>
        `;
    };

    const refreshDynamicContent = (root) => {
        window.refreshDynamicContent?.(root || document);
    };

    const settleBannerState = () => {
        if (!banner) {
            return;
        }

        const activeFeed = document.querySelector("#feed_content[data-feed-session-id]");
        if (activeFeed) {
            banner.hidden = false;
            return;
        }

        banner.hidden = true;
        banner.textContent = "";
        banner.classList.remove("warming");
    };

    const showRetryState = (attempt) => {
        if (!banner) {
            return;
        }

        banner.hidden = false;
        banner.classList.add("warming");
        banner.textContent = attempt < maxAttempts ? `${loadingMessage} Retrying…` : errorMessage;
    };

    const waitForRetry = () => new Promise((resolve) => {
        window.setTimeout(resolve, retryDelayMs);
    });

    const shouldRetryResponse = (status) => status >= 500;

    const loadFragment = async (attempt = 1) => {
        if (banner) {
            banner.hidden = false;
            banner.textContent = loadingMessage;
        }

        const controller = new AbortController();
        const timeoutId = window.setTimeout(() => controller.abort(), timeoutMs);

        try {
            const response = await fetch(fragmentUrl, {
                credentials: "same-origin",
                headers: {
                    "Cache-Control": "no-store",
                },
                signal: controller.signal,
            });

            if (!response.ok) {
                window.console?.warn?.("page_fragment: non-OK response", {
                    fragmentUrl,
                    status: response.status,
                    attempt,
                });
                if (!shouldRetryResponse(response.status)) {
                    showErrorState();
                    return;
                }
                throw new Error(`Fragment request failed: ${response.status}`);
            }

            const html = await response.text();
            writeCachedFragment(html);
            replaceFragment(html);
        } catch (error) {
            window.console?.warn?.("page_fragment: fetch attempt failed", {
                fragmentUrl,
                attempt,
                error: String(error),
            });
            if (attempt < maxAttempts) {
                showRetryState(attempt);
                await waitForRetry();
                return loadFragment(attempt + 1);
            }
            showErrorState();
        } finally {
            window.clearTimeout(timeoutId);
        }
    };

    const cachedHtml = readCachedFragment();
    if (cachedHtml) {
        replaceFragment(cachedHtml);
    }

    void loadFragment();
});
