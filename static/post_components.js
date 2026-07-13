(() => {
    const MAX_CONCURRENT = 4;
    const MAX_EAGER_WARMS = 6;
    const MAX_REMEMBERED_COMPONENTS = 32;
    const NAVIGATION_CACHE_KEY = "redlib:post-content-navigation-v1";
    const NAVIGATION_TTL_MS = 2 * 60 * 1000;
    const NAVIGATION_HTML_LIMIT = 256 * 1024;
    const shared = new Map();
    const queued = new Map();
    const liveShared = new Map();
    const stableHtml = new Map();
    const queue = [];
    const navigationController = new AbortController();
    let active = 0;
    let eagerWarms = 0;

    window.addEventListener("pagehide", () => navigationController.abort(), { once: true });

    const rememberStableHtml = (url, html) => {
        if (!url || typeof html !== "string" || html.length > NAVIGATION_HTML_LIMIT) {
            return;
        }
        if (stableHtml.has(url)) {
            stableHtml.delete(url);
        }
        stableHtml.set(url, html);
        while (stableHtml.size > MAX_REMEMBERED_COMPONENTS) {
            stableHtml.delete(stableHtml.keys().next().value);
        }
    };

    const parseStableComponent = (html, postId, revision) => {
        const template = document.createElement("template");
        template.innerHTML = html.trim();
        const component = template.content.firstElementChild;
        if (!component
            || template.content.children.length !== 1
            || component.dataset.postContentId !== postId
            || component.dataset.postContentRevision !== revision) {
            throw new Error("Stable post component identity mismatch");
        }
        return component;
    };

    const fetchStable = (url, priority = "high") => {
        if (shared.has(url)) {
            const queuedItem = queued.get(url);
            if (queuedItem && priority === "high") {
                queuedItem.priority = "high";
                drain();
            }
            return shared.get(url);
        }
        const request = new Promise((resolve, reject) => {
            const item = { url, resolve, reject, priority };
            queue.push(item);
            queued.set(url, item);
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
            const html = await response.text();
            rememberStableHtml(url, html);
            return html;
        }
        if (attempt === 1 && response.status >= 500) {
            return fetchAttempt(url, 2);
        }
        throw new Error(`Stable post component failed: ${response.status}`);
    };

    const drain = () => {
        while (active < MAX_CONCURRENT && queue.length > 0) {
            const highPriorityIndex = queue.findIndex((item) => item.priority === "high");
            if (highPriorityIndex < 0 && active > 0) {
                return;
            }
            const item = queue.splice(highPriorityIndex < 0 ? 0 : highPriorityIndex, 1)[0];
            queued.delete(item.url);
            active += 1;
            fetchAttempt(item.url).then(item.resolve, item.reject).finally(() => {
                active -= 1;
                drain();
            });
        }
    };

    const fetchLiveMedia = (url, force = false) => {
        if (force) {
            liveShared.delete(url);
        }
        if (liveShared.has(url)) {
            return liveShared.get(url);
        }
        const request = fetch(url, {
            cache: "no-store",
            credentials: "same-origin",
            headers: { "Cache-Control": "no-store" },
            signal: navigationController.signal,
        }).then(async (response) => {
            if (!response.ok) {
                throw new Error(`Live post media failed: ${response.status}`);
            }
            return response.text();
        });
        liveShared.set(url, request);
        request.catch(() => liveShared.delete(url));
        return request;
    };

    const activateStableMedia = (wrapper, component) => {
        if (wrapper.dataset.allowStableMedia !== "true") {
            return;
        }
        component.querySelectorAll("[data-stable-media-src]").forEach((media) => {
            media.src = media.dataset.stableMediaSrc;
        });
    };

    const configureConcealedMedia = (wrapper, component) => {
        const placeholder = component.querySelector("[data-post-live-media-kind]");
        if (!placeholder) {
            const stableMedia = component.querySelector(".post_cached_media");
            if (!stableMedia
                || !component.querySelector("[data-stable-media-src]")
                || component.querySelector("[data-post-stable-media-reveal]")) {
                return;
            }
            const button = document.createElement("button");
            button.type = "button";
            button.className = "post_cached_media_reveal";
            button.dataset.postStableMediaReveal = "";
            button.textContent = "Show media";
            button.addEventListener("click", () => {
                wrapper.dataset.allowStableMedia = "true";
                wrapper.classList.add("post_media_revealed");
                activateStableMedia(wrapper, component);
                button.remove();
                window.refreshDynamicContent?.(wrapper);
            }, { once: true });
            stableMedia.before(button);
            return;
        }
        if (placeholder.dataset.postLiveMediaRevealReady) {
            return;
        }
        placeholder.dataset.postLiveMediaRevealReady = "true";
        placeholder.setAttribute("role", "button");
        placeholder.setAttribute("tabindex", "0");
        placeholder.setAttribute("aria-label", "Show media");
        placeholder.textContent = "Show media";

        const reveal = () => {
            if (wrapper.dataset.postLiveMediaState === "loading" || wrapper.dataset.postLiveMediaState === "ready") {
                return;
            }
            wrapper.dataset.allowStableMedia = "true";
            wrapper.classList.add("post_media_revealed");
            placeholder.removeAttribute("role");
            placeholder.removeAttribute("tabindex");
            placeholder.removeAttribute("aria-label");
            placeholder.textContent = "Loading current media…";
            void hydrateLiveMedia(wrapper, component, true);
        };
        placeholder.addEventListener("click", reveal, { once: true });
        placeholder.addEventListener("keydown", (event) => {
            if (event.key === "Enter" || event.key === " ") {
                event.preventDefault();
                reveal();
            }
        }, { once: true });
    };

    const hydrateLiveMedia = async (wrapper, component, force = false) => {
        const placeholder = component.querySelector("[data-post-live-media-kind]");
        const url = wrapper.dataset.postLiveMediaUrl;
        if (!placeholder) {
            return;
        }
        if (!url) {
            if (wrapper.closest("#page_fragment[data-post-shell-id]")) {
                placeholder.textContent = "Loading fresh media…";
            }
            return;
        }
        if (wrapper.dataset.allowStableMedia !== "true") {
            configureConcealedMedia(wrapper, component);
            return;
        }
        if (!force && ["loading", "ready"].includes(wrapper.dataset.postLiveMediaState)) {
            return;
        }

        wrapper.dataset.postLiveMediaState = "loading";
        placeholder.textContent = "Loading current media…";
        try {
            const html = await fetchLiveMedia(url, force);
            const template = document.createElement("template");
            template.innerHTML = html.trim();
            const media = template.content.firstElementChild;
            if (!media
                || template.content.children.length !== 1
                || media.dataset.postLiveMediaId !== wrapper.dataset.postComponentId
                || media.dataset.postLiveMediaRevision !== wrapper.dataset.postComponentRevision) {
                throw new Error("Live post media identity mismatch");
            }
            placeholder.replaceWith(media);
            wrapper.dataset.postLiveMediaState = "ready";
            window.refreshDynamicContent?.(wrapper);
        } catch (error) {
            wrapper.dataset.postLiveMediaState = "fallback";
            placeholder.textContent = "Media is temporarily unavailable.";
            window.console?.warn?.("post_components: live media hydration failed", String(error));
        }
    };

    const installStableComponent = (wrapper, component) => {
        const slot = wrapper.querySelector("[data-post-component-slot]");
        if (!slot) {
            throw new Error("Stable post component slot missing");
        }
        if (wrapper.dataset.allowStableMedia === "true") {
            activateStableMedia(wrapper, component);
        } else {
            configureConcealedMedia(wrapper, component);
        }
        slot.replaceChildren(component);
        slot.setAttribute("aria-busy", "false");
        wrapper.setAttribute("aria-busy", "false");
        wrapper.dataset.postComponentState = "ready";
        window.refreshDynamicContent?.(wrapper);
        void hydrateLiveMedia(wrapper, component);
    };

    const hydrate = async (wrapper, priority = "high") => {
        const existing = wrapper.querySelector("[data-post-component-slot] > [data-post-content-id]");
        if (existing && wrapper.dataset.postComponentState === "ready") {
            if (!stableHtml.has(wrapper.dataset.postComponentUrl)) {
                rememberStableHtml(wrapper.dataset.postComponentUrl, existing.outerHTML);
            }
            if (wrapper.dataset.allowStableMedia === "true") {
                activateStableMedia(wrapper, existing);
            } else {
                configureConcealedMedia(wrapper, existing);
            }
            void hydrateLiveMedia(wrapper, existing);
            return;
        }
        if (wrapper.dataset.postComponentState === "loading") {
            if (priority === "high") {
                void fetchStable(wrapper.dataset.postComponentUrl, "high");
            }
            return;
        }

        wrapper.dataset.postComponentState = "loading";
        const slot = wrapper.querySelector("[data-post-component-slot]");
        try {
            const html = await fetchStable(wrapper.dataset.postComponentUrl, priority);
            const component = parseStableComponent(
                html,
                wrapper.dataset.postComponentId,
                wrapper.dataset.postComponentRevision,
            );
            installStableComponent(wrapper, component);
        } catch (error) {
            wrapper.dataset.postComponentState = "fallback";
            slot?.setAttribute("aria-busy", "false");
            wrapper.setAttribute("aria-busy", "false");
            const loading = slot?.querySelector("[data-post-component-loading]");
            if (loading) {
                loading.textContent = "Post content is temporarily unavailable.";
            }
            window.console?.warn?.("post_components: hydration failed", String(error));
        }
    };

    const warm = async (wrapper) => {
        if (wrapper.dataset.postComponentWarmObserved) {
            return;
        }
        wrapper.dataset.postComponentWarmObserved = "true";
        try {
            const html = await fetchStable(wrapper.dataset.postComponentWarmUrl, "low");
            parseStableComponent(
                html,
                wrapper.dataset.postComponentWarmId,
                wrapper.dataset.postComponentWarmRevision,
            );
        } catch (error) {
            window.console?.warn?.("post_components: cache warm failed", String(error));
        }
    };

    const safeSessionStorage = () => {
        try {
            return window.sessionStorage;
        } catch (_) {
            return null;
        }
    };

    const clearNavigationHint = () => {
        try {
            safeSessionStorage()?.removeItem(NAVIGATION_CACHE_KEY);
        } catch (_) {
            // Storage is an optional optimization.
        }
    };

    const saveNavigationHint = (anchor) => {
        const wrapper = anchor.closest("[data-post-component-id], [data-post-component-warm-id]");
        if (!wrapper) {
            return;
        }
        const postId = wrapper.dataset.postComponentId || wrapper.dataset.postComponentWarmId;
        const revision = wrapper.dataset.postComponentRevision || wrapper.dataset.postComponentWarmRevision;
        const componentUrl = wrapper.dataset.postComponentUrl || wrapper.dataset.postComponentWarmUrl;
        const html = stableHtml.get(componentUrl);
        if (!/^[a-z0-9]{1,24}$/.test(postId || "")
            || !/^[a-f0-9]{64}$/.test(revision || "")
            || !componentUrl
            || !html
            || html.length > NAVIGATION_HTML_LIMIT) {
            return;
        }

        let destination;
        try {
            destination = new URL(anchor.href, window.location.href);
        } catch (_) {
            return;
        }
        if (destination.origin !== window.location.origin
            || !(new RegExp(`/comments/${postId}(?:/|$)`)).test(destination.pathname)) {
            return;
        }
        try {
            parseStableComponent(html, postId, revision);
            safeSessionStorage()?.setItem(NAVIGATION_CACHE_KEY, JSON.stringify({
                version: 1,
                postId,
                revision,
                componentUrl,
                destinationPath: destination.pathname,
                storedAt: Date.now(),
            }));
        } catch (_) {
            clearNavigationHint();
        }
    };

    const restoreNavigationHint = async () => {
        const container = document.querySelector("#page_fragment[data-post-shell-id]");
        const storage = safeSessionStorage();
        if (!container || !storage) {
            return;
        }
        let hint;
        try {
            const raw = storage.getItem(NAVIGATION_CACHE_KEY);
            storage.removeItem(NAVIGATION_CACHE_KEY);
            if (!raw) {
                return;
            }
            hint = JSON.parse(raw);
        } catch (_) {
            return;
        }
        if (hint?.version !== 1
            || hint.postId !== container.dataset.postShellId
            || !/^[a-z0-9]{1,24}$/.test(hint.postId || "")
            || !/^[a-f0-9]{64}$/.test(hint.revision || "")
            || hint.destinationPath !== window.location.pathname
            || !Number.isFinite(hint.storedAt)
            || hint.storedAt > Date.now()
            || Date.now() - hint.storedAt > NAVIGATION_TTL_MS
            || hint.componentUrl !== `/post-content/${hint.postId}/${hint.revision}/card`) {
            return;
        }

        const current = container.querySelector(`[data-post-component-id="${CSS.escape(hint.postId)}"]`);
        if (current?.querySelector("[data-post-content-id]")) {
            return;
        }

        let component;
        try {
            const html = await fetchStable(hint.componentUrl);
            component = parseStableComponent(html, hint.postId, hint.revision);
        } catch (error) {
            window.console?.warn?.("post_components: navigation cache restore failed", String(error));
            return;
        }
        if (component.classList.contains("post_cached_content_nsfw")
            || component.classList.contains("post_cached_content_spoiler")) {
            return;
        }
        const column = container.querySelector("#column_one");
        if (!column) {
            return;
        }
        const wrapper = document.createElement("div");
        wrapper.id = hint.postId;
        wrapper.className = "post highlighted post_component_wrapper";
        wrapper.dataset.postComponentId = hint.postId;
        wrapper.dataset.postComponentRevision = hint.revision;
        wrapper.dataset.postComponentUrl = hint.componentUrl;
        wrapper.dataset.postComponentState = "ready";
        wrapper.dataset.allowStableMedia = "false";
        wrapper.setAttribute("aria-busy", "false");
        const slot = document.createElement("div");
        slot.className = "post_component_slot";
        slot.dataset.postComponentSlot = "";
        slot.setAttribute("aria-busy", "false");
        slot.appendChild(component);
        wrapper.appendChild(slot);
        column.replaceChildren(wrapper);
        initialize(column);
    };

    const pristineCurrentComponent = (wrapper) => {
        const url = wrapper.dataset.postComponentUrl;
        const remembered = stableHtml.get(url);
        if (remembered) {
            return parseStableComponent(
                remembered,
                wrapper.dataset.postComponentId,
                wrapper.dataset.postComponentRevision,
            );
        }
        const current = wrapper.querySelector("[data-post-component-slot] > [data-post-content-id]");
        if (!current) {
            return null;
        }
        const clone = current.cloneNode(true);
        const liveMedia = clone.querySelector("[data-post-live-media-id]");
        if (liveMedia) {
            const placeholder = document.createElement("div");
            placeholder.className = "post_cached_media_placeholder";
            placeholder.dataset.postLiveMediaKind = liveMedia.dataset.postLiveMediaKind || "media";
            placeholder.textContent = "Media loads with fresh post metadata.";
            liveMedia.replaceWith(placeholder);
        }
        return clone;
    };

    const preparePostFragment = async (root) => {
        const next = root?.querySelector?.(".post_component_wrapper[data-post-component-id]");
        const shell = document.querySelector("#page_fragment[data-post-shell-id]");
        const current = shell?.querySelector(".post_component_wrapper[data-post-component-id]");
        if (!next || !current || next.dataset.postComponentId !== current.dataset.postComponentId) {
            return;
        }
        const slot = next.querySelector("[data-post-component-slot]");
        if (!slot) {
            return;
        }

        let component;
        if (next.dataset.postComponentRevision === current.dataset.postComponentRevision) {
            component = pristineCurrentComponent(current);
        } else {
            try {
                const html = await fetchStable(next.dataset.postComponentUrl);
                component = parseStableComponent(
                    html,
                    next.dataset.postComponentId,
                    next.dataset.postComponentRevision,
                );
            } catch (error) {
                component = pristineCurrentComponent(current);
                if (component) {
                    next.dataset.postComponentRevision = current.dataset.postComponentRevision;
                    next.dataset.postComponentUrl = current.dataset.postComponentUrl;
                    delete next.dataset.postLiveMediaUrl;
                }
                window.console?.warn?.("post_components: fresh stable card unavailable", String(error));
            }
        }
        if (!component) {
            return;
        }
        slot.replaceChildren(component);
        slot.setAttribute("aria-busy", "false");
        next.setAttribute("aria-busy", "false");
        next.dataset.postComponentState = "ready";
        if (next.dataset.postLiveMediaUrl) {
            liveShared.delete(next.dataset.postLiveMediaUrl);
        }
    };

    const initialize = (root = document) => {
        const wrappers = root.querySelectorAll?.("[data-post-component-url]:not([data-post-component-observed])") || [];
        if (!("IntersectionObserver" in window)) {
            wrappers.forEach((wrapper) => {
                wrapper.dataset.postComponentObserved = "true";
                void hydrate(wrapper);
            });
        } else {
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
        }

        if (!document.hidden) {
            wrappers.forEach((wrapper) => void hydrate(wrapper, "low"));
        }

        const warmers = root.querySelectorAll?.("[data-post-component-warm-url]:not([data-post-component-warm-observed]):not([data-post-component-warm-scheduled])") || [];
        for (const wrapper of warmers) {
            if (eagerWarms >= MAX_EAGER_WARMS) {
                break;
            }
            eagerWarms += 1;
            wrapper.dataset.postComponentWarmScheduled = "true";
            window.setTimeout(() => {
                const run = () => void warm(wrapper);
                if ("requestIdleCallback" in window) {
                    window.requestIdleCallback(run, { timeout: 1000 });
                } else {
                    run();
                }
            }, 750);
        }
    };

    document.addEventListener("click", (event) => {
        const anchor = event.target.closest?.("a[href]");
        if (anchor) {
            saveNavigationHint(anchor);
        }
    }, { capture: true });

    document.addEventListener("visibilitychange", () => {
        if (!document.hidden) {
            document.querySelectorAll("[data-post-component-url]").forEach((wrapper) => void hydrate(wrapper, "low"));
        }
    });

    window.initializePostComponents = initialize;
    window.preparePostFragment = preparePostFragment;
    document.addEventListener("DOMContentLoaded", () => {
        void restoreNavigationHint();
        initialize(document);
    });
})();
