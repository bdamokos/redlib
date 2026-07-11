document.addEventListener("DOMContentLoaded", () => {
    const container = document.querySelector("#page_fragment[data-fragment-url]");
    if (!container) {
        return;
    }

    const fragmentUrl = container.dataset.fragmentUrl;
    if (!fragmentUrl) {
        return;
    }

    const sidebarFragmentUrl = container.dataset.sidebarFragmentUrl || "";
    const loadingMessage = container.dataset.loadingMessage || "Loading the page...";
    const errorMessage = container.dataset.errorMessage || "Loading the page is taking longer than expected.";
    const timeoutMs = Number.parseInt(container.dataset.timeoutMs || "15000", 10);
    const maxAttempts = Number.parseInt(container.dataset.maxAttempts || "3", 10);
    const retryDelayMs = Number.parseInt(container.dataset.retryDelayMs || "1500", 10);
    const replaceSelector = container.dataset.replaceSelector || "";
    const appendFragmentContent = container.dataset.appendFragmentContent === "true";
    const banner = document.querySelector(".feed_status_banner");

    const setBannerVisible = (visible) => {
        if (!banner) {
            return;
        }

        banner.classList.toggle("is-hidden", !visible);
        if (visible) {
            banner.removeAttribute("aria-hidden");
        } else {
            banner.setAttribute("aria-hidden", "true");
        }
    };

    const applyDocumentMetadata = (root) => {
        const metadataNode = root?.querySelector?.("[data-document-title]");
        const documentTitle = metadataNode?.dataset?.documentTitle;
        if (documentTitle) {
            document.title = documentTitle;
        }
    };

    const replaceFragment = (html) => {
        const template = document.createElement("template");
        template.innerHTML = html.trim();
        applyDocumentMetadata(template.content);

        if (appendFragmentContent && appendIntoCurrentFragment(template.content)) {
            refreshDynamicContent(document);
            settleBannerState();
            return;
        }

        if (replaceSelector) {
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

    const renderRetryLink = (parent) => {
        const link = document.createElement("a");
        link.href = fragmentUrl;
        link.textContent = "Try loading the content again";
        parent.appendChild(link);
    };

    const showErrorState = () => {
        window.console?.warn?.("page_fragment: failed to load", {
            fragmentUrl,
            replaceSelector,
        });

        if (banner) {
            banner.textContent = "";
            banner.classList.remove("warming");
            banner.appendChild(document.createTextNode(`${errorMessage} `));
            renderRetryLink(banner);
        }
        setBannerVisible(true);

        const target = replaceSelector ? document.querySelector(replaceSelector) : container;
        if (!target) {
            return;
        }

        if (appendFragmentContent) {
            return;
        }

        const firstLine = document.createElement("center");
        firstLine.textContent = errorMessage;
        const secondLine = document.createElement("center");
        const paragraph = document.createElement("p");
        renderRetryLink(paragraph);
        secondLine.appendChild(paragraph);
        target.replaceChildren(firstLine, secondLine);
    };

    const postMergeKeys = (post) => {
        const keys = new Set();
        if (post.id) {
            keys.add(post.id);
        }
        for (const key of (post.dataset.postMergeKeys || "").split("\t")) {
            const trimmed = key.trim();
            if (trimmed) {
                keys.add(trimmed);
            }
        }
        return [...keys];
    };

    const registerPostKeys = (post, existingByKey) => {
        for (const key of postMergeKeys(post)) {
            existingByKey.set(key, post);
        }
    };

    const matchingExistingPost = (post, existingByKey) => {
        for (const key of postMergeKeys(post)) {
            const existing = existingByKey.get(key);
            if (existing) {
                return existing;
            }
        }
        return null;
    };

    const refreshDuplicateDetails = (currentPost, nextPost) => {
        const nextDuplicates = nextPost.querySelector(":scope > .post_duplicates");
        if (!nextDuplicates) {
            return;
        }

        const currentDuplicates = currentPost.querySelector(":scope > .post_duplicates");
        if (currentDuplicates) {
            currentDuplicates.replaceWith(nextDuplicates.cloneNode(true));
        } else {
            currentPost.appendChild(nextDuplicates.cloneNode(true));
        }
    };

    const replaceOrAppendFooter = (currentColumn, nextColumn) => {
        const nextFooter = nextColumn.querySelector(":scope > footer");
        if (!nextFooter) {
            return;
        }

        const currentFooter = currentColumn.querySelector(":scope > footer");
        if (currentFooter) {
            currentFooter.replaceWith(nextFooter.cloneNode(true));
        } else {
            currentColumn.appendChild(nextFooter.cloneNode(true));
        }
    };

    const appendPostNodes = (currentPosts, nextPosts) => {
        const existingByKey = new Map();
        currentPosts.querySelectorAll(":scope > .post[id]").forEach((post) => {
            registerPostKeys(post, existingByKey);
        });

        let pendingNodes = [];
        for (const node of Array.from(nextPosts.childNodes)) {
            pendingNodes.push(node);
            if (!(node instanceof Element) || !node.matches(".post[id]")) {
                continue;
            }

            const existing = matchingExistingPost(node, existingByKey);
            if (existing) {
                refreshDuplicateDetails(existing, node);
                pendingNodes = [];
                continue;
            }

            for (const pendingNode of pendingNodes) {
                const clone = pendingNode.cloneNode(true);
                currentPosts.appendChild(clone);
                if (clone instanceof Element && clone.matches(".post[id]")) {
                    registerPostKeys(clone, existingByKey);
                }
            }
            pendingNodes = [];
        }
    };

    const appendIntoCurrentFragment = (nextRoot) => {
        if (!replaceSelector) {
            return false;
        }

        const currentColumn = document.querySelector(replaceSelector);
        const nextColumn = nextRoot.querySelector(replaceSelector);
        if (!currentColumn || !nextColumn) {
            return false;
        }

        const currentPosts = currentColumn.querySelector(":scope > #posts");
        const nextPosts = nextColumn.querySelector(":scope > #posts");
        if (currentPosts && nextPosts) {
            appendPostNodes(currentPosts, nextPosts);
            replaceOrAppendFooter(currentColumn, nextColumn);
            return true;
        }

        if (!currentPosts && nextPosts) {
            const currentFooter = currentColumn.querySelector(":scope > footer");
            currentColumn.insertBefore(nextPosts.cloneNode(true), currentFooter);
            replaceOrAppendFooter(currentColumn, nextColumn);
            return true;
        }

        replaceOrAppendFooter(currentColumn, nextColumn);
        return true;
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
            setBannerVisible(true);
            return;
        }

        banner.textContent = "";
        banner.classList.remove("warming");
        setBannerVisible(false);
    };

    const showRetryState = (attempt) => {
        if (!banner) {
            return;
        }

        setBannerVisible(true);
        banner.classList.add("warming");
        banner.textContent = attempt < maxAttempts ? `${loadingMessage} Retrying…` : errorMessage;
    };

    const waitForRetry = () => new Promise((resolve) => {
        window.setTimeout(resolve, retryDelayMs);
    });

    const shouldRetryResponse = (status) => status === 408 || status === 425 || status === 429 || status >= 500;

    const loadSidebarFragment = async () => {
        if (!sidebarFragmentUrl) {
            return;
        }

        try {
            const response = await fetch(sidebarFragmentUrl, {
                credentials: "same-origin",
                headers: {
                    "Cache-Control": "no-store",
                },
            });
            if (!response.ok) {
                window.console?.warn?.("page_fragment: sidebar non-OK response", {
                    sidebarFragmentUrl,
                    status: response.status,
                });
                return;
            }

            const html = await response.text();
            const template = document.createElement("template");
            template.innerHTML = html.trim();
            const nextSidebar = template.content.querySelector("aside");
            const currentSidebar = document.querySelector("aside");
            if (!nextSidebar || !currentSidebar) {
                return;
            }
            currentSidebar.replaceWith(nextSidebar);
            refreshDynamicContent(document);
        } catch (error) {
            window.console?.warn?.("page_fragment: sidebar fetch failed", {
                sidebarFragmentUrl,
                error: String(error),
            });
        }
    };

    let loadInFlight = false;
    let reloadRequested = false;

    const loadFragmentAttempt = async (attempt = 1) => {
        if (banner) {
            setBannerVisible(true);
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
            replaceFragment(html);
            void loadSidebarFragment();
        } catch (error) {
            window.console?.warn?.("page_fragment: fetch attempt failed", {
                fragmentUrl,
                attempt,
                error: String(error),
            });
            if (attempt < maxAttempts) {
                showRetryState(attempt);
                await waitForRetry();
                return loadFragmentAttempt(attempt + 1);
            }
            showErrorState();
        } finally {
            window.clearTimeout(timeoutId);
        }
    };

    const loadFragment = async () => {
        if (loadInFlight) {
            return;
        }
        reloadRequested = false;
        loadInFlight = true;
        try {
            await loadFragmentAttempt(1);
        } finally {
            loadInFlight = false;
            if (reloadRequested && !document.querySelector("#page_fragment .post")) {
                reloadRequested = false;
                void loadFragment();
            }
        }
    };

    document.addEventListener("redlib:content-progress", () => {
        if (!document.querySelector("#page_fragment .post")) {
            reloadRequested = true;
            void loadFragment();
        }
    });

    void loadFragment();
});
