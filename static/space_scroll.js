function getScrollPaddingTop() {
    const scrollPaddingTop = window.getComputedStyle(document.documentElement).scrollPaddingTop;
    const parsedValue = Number.parseFloat(scrollPaddingTop);

    return Number.isNaN(parsedValue) ? 0 : parsedValue;
}

function getPostTop(post) {
    return Math.round(window.scrollY + post.getBoundingClientRect().top);
}

function isHighlightedTarget(target) {
    if (!(target instanceof Element)) {
        return false;
    }

    if (target.classList.contains("highlighted")) {
        return true;
    }

    if (!target.classList.contains("comment")) {
        return false;
    }

    return target.querySelector(":scope > .comment_right > .comment_body.highlighted, :scope > .comment_right > .comment_body_filtered.highlighted") !== null;
}

function isVisibleTarget(target) {
    return target.getClientRects().length > 0;
}

function getScrollTargets() {
    return Array.from(document.querySelectorAll(".post, .comment")).filter(
        (target) => !isHighlightedTarget(target) && isVisibleTarget(target),
    );
}

function isInteractiveElement(element) {
    if (!(element instanceof Element)) {
        return false;
    }

    return element.closest(
        "input, textarea, select, button, summary, video, audio, [contenteditable], [role=\"textbox\"]"
    ) !== null;
}

function isTargetFullyVisible(target, viewportTop, viewportBottom) {
    const rect = target.getBoundingClientRect();

    return rect.top >= viewportTop && rect.bottom <= viewportBottom;
}

function findViewportSkipTarget(targets) {
    const viewportTop = getScrollPaddingTop();
    const viewportBottom = window.innerHeight;

    return targets.find((target) => {
        const rect = target.getBoundingClientRect();

        if (rect.bottom <= viewportTop + 1) {
            return false;
        }

        return !isTargetFullyVisible(target, viewportTop, viewportBottom);
    }) || null;
}

function findAdjacentScrollTarget(posts, currentTop, direction) {
    if (direction > 0) {
        return posts.find((post) => getPostTop(post) > currentTop + 1) || null;
    }

    for (let index = posts.length - 1; index >= 0; index -= 1) {
        if (getPostTop(posts[index]) < currentTop - 1) {
            return posts[index];
        }
    }

    return null;
}

function getKeyName(event) {
    return typeof event.key === "string" ? event.key.toLowerCase() : "";
}

function getScrollNavigationDirection(event) {
    if (event.key === " " || event.code === "Space") {
        return event.shiftKey ? -1 : 1;
    }

    if (getKeyName(event) === "j") {
        return 1;
    }

    if (getKeyName(event) === "k") {
        return -1;
    }

    return 0;
}

function getPaginationAccessKey(event) {
    if (getKeyName(event) === "n") {
        return "N";
    }

    if (getKeyName(event) === "p") {
        return "P";
    }

    return null;
}

function getPaginationLink(accessKey) {
    if (accessKey === null) {
        return null;
    }

    return document.querySelector(`a[accesskey='${accessKey}']`);
}

document.addEventListener("keydown", (event) => {
    const isSpaceKey = event.key === " " || event.code === "Space";
    const scrollDirection = getScrollNavigationDirection(event);
    const paginationLink = getPaginationLink(getPaginationAccessKey(event));
    const isScrollNavigationKey = scrollDirection !== 0;
    const isPaginationKey = paginationLink !== null;

    if (event.defaultPrevented || (!isScrollNavigationKey && !isPaginationKey) || event.altKey || event.ctrlKey || event.metaKey) {
        return;
    }

    if (isInteractiveElement(event.target) || isInteractiveElement(document.activeElement)) {
        return;
    }

    if (isPaginationKey) {
        event.preventDefault();
        paginationLink.click();
        return;
    }

    const targets = getScrollTargets();
    if (targets.length === 0) {
        return;
    }

    const scrollPaddingTop = getScrollPaddingTop();
    const targetPost = isSpaceKey && !event.shiftKey
        ? findViewportSkipTarget(targets)
        : findAdjacentScrollTarget(targets, window.scrollY + scrollPaddingTop, scrollDirection);

    if (targetPost === null) {
        return;
    }

    event.preventDefault();
    window.scrollTo({
        top: Math.max(0, getPostTop(targetPost) - scrollPaddingTop),
        behavior: "auto",
    });
});
