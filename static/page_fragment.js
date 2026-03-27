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
    const banner = document.querySelector(".feed_status_banner");

    const showErrorState = () => {
        if (banner) {
            banner.textContent = errorMessage;
            banner.classList.remove("warming");
        }

        container.innerHTML = `
            <div id="column_one">
                <center>${errorMessage}</center>
                <center><p><a href="${fragmentUrl}">Try loading the content again</a></p></center>
            </div>
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
                throw new Error(`Fragment request failed: ${response.status}`);
            }

            const html = await response.text();
            container.outerHTML = html;
            refreshDynamicContent(document);
            settleBannerState();
        } catch (_) {
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

    void loadFragment();
});
