(() => {
    const initialize = (root = document) => {
        const navigations = Array.from(
            root.querySelectorAll?.(".discovery_navigation:not([data-discovery-navigation-bound])") || []
        );

        for (const navigation of navigations) {
            navigation.dataset.discoveryNavigationBound = "true";
            const markers = Array.from(navigation.querySelectorAll(".discovery_navigation_marker"));
            if (markers.length === 0) {
                continue;
            }

            let activeMarker = null;
            let labelTimer = 0;
            const targets = markers
                .map((marker) => ({
                    marker,
                    target: document.getElementById(marker.dataset.discoveryTarget || ""),
                }))
                .filter(({ target }) => target);
            const currentPage = Number.parseInt(navigation.dataset.discoveryCurrentPage || "0", 10);

            const activate = (marker, announce = false) => {
                if (activeMarker === marker) {
                    return;
                }
                activeMarker?.removeAttribute("aria-current");
                activeMarker?.classList.remove("is-active", "show-label");
                activeMarker = marker;
                activeMarker.classList.add("is-active");
                activeMarker.setAttribute("aria-current", "location");
                activeMarker.scrollIntoView({ block: "nearest" });
                if (announce && window.matchMedia?.("(pointer: coarse)")?.matches) {
                    activeMarker.classList.add("show-label");
                    window.clearTimeout(labelTimer);
                    labelTimer = window.setTimeout(() => activeMarker?.classList.remove("show-label"), 1600);
                }
            };

            for (const marker of markers) {
                marker.addEventListener("click", (event) => {
                    const targetId = marker.dataset.discoveryTarget || "";
                    const target = document.getElementById(targetId);
                    if (!target || marker.getAttribute("href") !== `#${targetId}`) {
                        return;
                    }
                    event.preventDefault();
                    window.history.replaceState(null, "", `#${targetId}`);
                    target.scrollIntoView({
                        behavior: window.matchMedia?.("(prefers-reduced-motion: reduce)")?.matches ? "auto" : "smooth",
                        block: "start",
                    });
                    activate(marker, true);
                });
            }

            const initialMarker =
                markers.filter((marker) => Number.parseInt(marker.dataset.discoveryPage || "0", 10) <= currentPage).at(-1)
                || markers[0];
            activate(initialMarker);

            if (targets.length === 0) {
                continue;
            }

            const hashTargetId = window.location.hash.startsWith("#")
                ? decodeURIComponent(window.location.hash.slice(1))
                : "";
            const hashTarget = hashTargetId ? document.getElementById(hashTargetId) : null;
            const hashMarker = hashTarget
                ? markers.find((marker) => marker.dataset.discoveryTarget === hashTargetId)
                : null;
            if (hashTarget && hashMarker) {
                activate(hashMarker, true);
                const alignHashTarget = () => hashTarget.scrollIntoView({ block: "start" });
                window.requestAnimationFrame(alignHashTarget);
                if ("ResizeObserver" in window) {
                    const resizeObserver = new ResizeObserver(alignHashTarget);
                    resizeObserver.observe(document.getElementById("posts") || hashTarget);
                    const stopAlignment = () => resizeObserver.disconnect();
                    for (const eventName of ["pointerdown", "touchstart", "wheel", "keydown"]) {
                        window.addEventListener(eventName, stopAlignment, { capture: true, once: true });
                    }
                    window.setTimeout(stopAlignment, 4000);
                }
            }

            let scrollFrame = 0;
            const updateFromScroll = () => {
                scrollFrame = 0;
                const targetTop = document.documentElement.classList.contains("fixed_navbar")
                    ? Number.parseFloat(getComputedStyle(document.documentElement).scrollPaddingTop) || 0
                    : 0;
                const current = targets.reduce((closest, candidate) => {
                    const distance = Math.abs(candidate.target.getBoundingClientRect().top - targetTop);
                    return !closest || distance < closest.distance ? { ...candidate, distance } : closest;
                }, null);
                if (current) {
                    activate(current.marker);
                }
            };
            const scheduleScrollUpdate = () => {
                if (!scrollFrame) {
                    scrollFrame = window.requestAnimationFrame(updateFromScroll);
                }
            };
            window.addEventListener("scroll", scheduleScrollUpdate, { passive: true });
            window.addEventListener("resize", scheduleScrollUpdate, { passive: true });
            scheduleScrollUpdate();
        }
    };

    window.initializeDiscoveryNavigation = initialize;
    document.addEventListener("DOMContentLoaded", () => initialize(document));
})();
