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
            let previewMarker = null;
            let armedMarker = null;
            let touchGesture = null;
            let suppressClickUntil = 0;
            let labelTimer = 0;
            const targets = markers
                .map((marker) => ({
                    marker,
                    target: document.getElementById(marker.dataset.discoveryTarget || ""),
                }))
                .filter(({ target }) => target);
            const currentPage = Number.parseInt(navigation.dataset.discoveryCurrentPage || "0", 10);

            const clearPreview = () => {
                window.clearTimeout(labelTimer);
                previewMarker?.classList.remove("show-label");
                previewMarker = null;
                armedMarker = null;
            };

            const showPreview = (marker, arm = false) => {
                window.clearTimeout(labelTimer);
                if (previewMarker !== marker) {
                    previewMarker?.classList.remove("show-label");
                    previewMarker = marker;
                }
                previewMarker.classList.add("show-label");
                if (armedMarker !== marker) {
                    armedMarker = null;
                }
                if (arm) {
                    armedMarker = marker;
                }
            };

            const activate = (marker, announce = false) => {
                if (activeMarker === marker) {
                    return;
                }
                activeMarker?.removeAttribute("aria-current");
                activeMarker?.classList.remove("is-active");
                activeMarker = marker;
                activeMarker.classList.add("is-active");
                activeMarker.setAttribute("aria-current", "location");
                activeMarker.scrollIntoView({ block: "nearest" });
                if (announce && window.matchMedia?.("(pointer: coarse)")?.matches) {
                    showPreview(activeMarker);
                    labelTimer = window.setTimeout(clearPreview, 1600);
                }
            };

            const navigateToMarker = (marker) => {
                const targetId = marker.dataset.discoveryTarget || "";
                const target = document.getElementById(targetId);
                if (!target || marker.getAttribute("href") !== `#${targetId}`) {
                    window.location.assign(marker.href);
                    return;
                }
                clearPreview();
                window.history.replaceState(null, "", `#${targetId}`);
                target.scrollIntoView({
                    behavior: window.matchMedia?.("(prefers-reduced-motion: reduce)")?.matches ? "auto" : "smooth",
                    block: "start",
                });
                activate(marker);
            };

            for (const marker of markers) {
                marker.addEventListener("click", (event) => {
                    if (Date.now() < suppressClickUntil) {
                        event.preventDefault();
                        return;
                    }
                    const targetId = marker.dataset.discoveryTarget || "";
                    const target = document.getElementById(targetId);
                    if (!target || marker.getAttribute("href") !== `#${targetId}`) {
                        return;
                    }
                    event.preventDefault();
                    navigateToMarker(marker);
                });
            }

            const markerAtTouch = (touch, fallbackTarget) =>
                document.elementFromPoint(touch.clientX, touch.clientY)?.closest?.(".discovery_navigation_marker")
                || fallbackTarget?.closest?.(".discovery_navigation_marker")
                || null;

            navigation.addEventListener("touchstart", (event) => {
                if (event.touches.length !== 1) {
                    touchGesture = null;
                    return;
                }
                const marker = markerAtTouch(event.touches[0], event.target);
                if (!marker || !navigation.contains(marker)) {
                    touchGesture = null;
                    return;
                }
                touchGesture = {
                    lastMarker: marker,
                    scrubbed: false,
                };
                showPreview(marker);
            }, { passive: true });

            navigation.addEventListener("touchmove", (event) => {
                if (!touchGesture || event.touches.length !== 1) {
                    return;
                }
                const marker = markerAtTouch(event.touches[0], event.target);
                if (!marker || !navigation.contains(marker) || marker === touchGesture.lastMarker) {
                    return;
                }
                touchGesture.lastMarker = marker;
                touchGesture.scrubbed = true;
                showPreview(marker);
            }, { passive: true });

            const finishTouch = (event, cancelled = false) => {
                if (!touchGesture) {
                    return;
                }
                const { lastMarker, scrubbed } = touchGesture;
                touchGesture = null;
                if (event.cancelable) {
                    event.preventDefault();
                }
                suppressClickUntil = Date.now() + 800;
                if (!cancelled && !scrubbed && armedMarker === lastMarker) {
                    navigateToMarker(lastMarker);
                    return;
                }
                showPreview(lastMarker, true);
            };
            navigation.addEventListener("touchend", (event) => finishTouch(event), { passive: false });
            navigation.addEventListener("touchcancel", (event) => finishTouch(event, true), { passive: false });
            document.addEventListener("pointerdown", (event) => {
                if (!navigation.contains(event.target)) {
                    clearPreview();
                }
            }, { passive: true });
            document.addEventListener("keydown", (event) => {
                if (event.key === "Escape") {
                    clearPreview();
                }
            });

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
