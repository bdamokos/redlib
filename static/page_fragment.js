document.addEventListener("DOMContentLoaded", () => {
    const container = document.querySelector("#page_fragment[data-fragment-url]");
    if (!container) {
        return;
    }

    const fragmentUrl = container.dataset.fragmentUrl;
    if (!fragmentUrl) {
        return;
    }

    void fetch(fragmentUrl, {
        credentials: "same-origin",
        headers: {
            "Cache-Control": "no-store",
        },
    })
        .then((response) => {
            if (!response.ok) {
                throw new Error(`Fragment request failed: ${response.status}`);
            }
            return response.text();
        })
        .then((html) => {
            container.outerHTML = html;
        })
        .catch(() => {
            const banner = document.querySelector(".feed_status_banner");
            if (banner) {
                banner.textContent = "Loading the page is taking longer than expected.";
            }
        });
});
