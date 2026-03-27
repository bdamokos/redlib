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

    const loadFragment = async () => {
        if (banner) {
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
        } catch (_) {
            showErrorState();
        } finally {
            window.clearTimeout(timeoutId);
        }
    };

    void loadFragment();
});
