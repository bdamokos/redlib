(() => {
    const MAX_CONCURRENT = 4;
    const shared = new Map();
    const queue = [];
    const navigationController = new AbortController();
    let active = 0;

    window.addEventListener("pagehide", () => navigationController.abort(), { once: true });

    const fetchStable = (url) => {
        if (shared.has(url)) {
            return shared.get(url);
        }
        const request = new Promise((resolve, reject) => {
            queue.push({ url, resolve, reject });
            drain();
        });
        shared.set(url, request);
        request.catch(() => shared.delete(url));
        return request;
    };

    const fetchAttempt = async (url, attempt = 1) => {
        const response = await fetch(url, {
            cache: "force-cache",
            credentials: "omit",
            signal: navigationController.signal,
        });
        if (response.ok) {
            return response.text();
        }
        if (attempt === 1 && response.status >= 500) {
            return fetchAttempt(url, 2);
        }
        throw new Error(`Stable post component failed: ${response.status}`);
    };

    const drain = () => {
        while (active < MAX_CONCURRENT && queue.length > 0) {
            const item = queue.shift();
            active += 1;
            fetchAttempt(item.url).then(item.resolve, item.reject).finally(() => {
                active -= 1;
                drain();
            });
        }
    };

    const hydrate = async (wrapper) => {
        if (wrapper.dataset.postComponentState) {
            return;
        }
        wrapper.dataset.postComponentState = "loading";
        const slot = wrapper.querySelector("[data-post-component-slot]");
        try {
            const html = await fetchStable(wrapper.dataset.postComponentUrl);
            const template = document.createElement("template");
            template.innerHTML = html.trim();
            const component = template.content.firstElementChild;
            if (!component
                || component.dataset.postContentId !== wrapper.dataset.postComponentId
                || component.dataset.postContentRevision !== wrapper.dataset.postComponentRevision) {
                throw new Error("Stable post component identity mismatch");
            }
            if (wrapper.dataset.allowStableMedia === "true") {
                component.querySelectorAll("[data-stable-media-src]").forEach((media) => {
                    media.src = media.dataset.stableMediaSrc;
                });
            }
            slot.replaceChildren(component);
            slot.setAttribute("aria-busy", "false");
            wrapper.dataset.postComponentState = "ready";
            window.refreshDynamicContent?.(wrapper);
        } catch (error) {
            wrapper.dataset.postComponentState = "fallback";
            slot?.setAttribute("aria-busy", "false");
            window.console?.warn?.("post_components: hydration failed", String(error));
        }
    };

    const initialize = (root = document) => {
        const wrappers = root.querySelectorAll?.("[data-post-component-url]:not([data-post-component-observed])") || [];
        const observer = new IntersectionObserver((entries) => {
            entries.forEach((entry) => {
                if (entry.isIntersecting) {
                    observer.unobserve(entry.target);
                    void hydrate(entry.target);
                }
            });
        }, { rootMargin: "100% 0px" });
        wrappers.forEach((wrapper) => {
            wrapper.dataset.postComponentObserved = "true";
            observer.observe(wrapper);
        });
    };

    window.initializePostComponents = initialize;
    document.addEventListener("DOMContentLoaded", () => initialize(document));
})();
