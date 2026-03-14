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

function findScrollTarget(posts, currentTop, direction) {
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

document.addEventListener("keydown", (event) => {
    if (event.defaultPrevented || (event.key !== " " && event.code !== "Space") || event.altKey || event.ctrlKey || event.metaKey) {
        return;
    }

    if (isInteractiveElement(event.target) || isInteractiveElement(document.activeElement)) {
        return;
    }

    const targets = getScrollTargets();
    if (targets.length === 0) {
        return;
    }

    const scrollPaddingTop = getScrollPaddingTop();
    const currentTop = window.scrollY + scrollPaddingTop;
    const direction = event.shiftKey ? -1 : 1;
    const targetPost = findScrollTarget(targets, currentTop, direction);

    if (targetPost === null) {
        return;
    }

    event.preventDefault();
    window.scrollTo({
        top: Math.max(0, getPostTop(targetPost) - scrollPaddingTop),
        behavior: "auto",
    });
});
