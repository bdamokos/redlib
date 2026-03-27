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

    const refresh = async () => {
        const statusResponse = await fetch(statusUrl, {
            credentials: "same-origin",
            headers: {
                "Cache-Control": "no-store",
            },
        });
        if (!statusResponse.ok) {
            return false;
        }

        const status = await statusResponse.json();
        const banner = document.querySelector(".feed_status_banner");
        if (banner && typeof status.status_message === "string") {
            banner.textContent = status.status_message;
            banner.classList.toggle("warming", !!status.warming);
        }

        if (status.warming || status.current_page_ready || status.next_page_ready) {
            const fragmentResponse = await fetch(fragmentUrl, {
                credentials: "same-origin",
                headers: {
                    "Cache-Control": "no-store",
                },
            });
            if (fragmentResponse.ok) {
                container.innerHTML = await fragmentResponse.text();
            }
        }

        return !!status.warming;
    };

    let timerId = null;
    const tick = async () => {
        const shouldContinue = await refresh();
        if (shouldContinue) {
            timerId = window.setTimeout(tick, 2000);
        } else if (timerId) {
            window.clearTimeout(timerId);
            timerId = null;
        }
    };

    void tick();
});
