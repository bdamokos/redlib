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
                container.innerHTML = await fragmentResponse.text();
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
