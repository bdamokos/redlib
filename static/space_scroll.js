function getScrollPaddingTop() {
    const scrollPaddingTop = window.getComputedStyle(document.documentElement).scrollPaddingTop;
    const parsedValue = Number.parseFloat(scrollPaddingTop);

    return Number.isNaN(parsedValue) ? 0 : parsedValue;
}

// Single media posts should fit after the visible title, body, footer, and media metadata.
const SINGLE_POST_MEDIA_SELECTOR = [
    ".post_media_column .video_preview_stage",
    ".post_media_content > .post_media_embed",
    ".post_media_content > .post_media_image > img",
    ".post_media_content > .post_media_image > svg",
].join(", ");

const POST_MEDIA_FIT_BOTTOM_GAP = 8;
const POST_MEDIA_FIT_MIN_HEIGHT = 96;
let postMediaFitFrame = null;

function parsePostMediaLength(value, fallback) {
    const trimmedValue = value.trim().toLowerCase();
    const parsedValue = Number.parseFloat(trimmedValue);

    if (Number.isNaN(parsedValue)) {
        return fallback;
    }

    if (trimmedValue.endsWith("px")) {
        return parsedValue;
    }

    if (trimmedValue.endsWith("vh") || trimmedValue.endsWith("dvh")) {
        return (window.innerHeight * parsedValue) / 100;
    }

    return fallback;
}

function getPostMediaBaseMaxHeight(post) {
    const fallback = window.innerHeight * 0.85;
    const defaultMaxHeight = window
        .getComputedStyle(post)
        .getPropertyValue("--post-media-default-max-height");

    return parsePostMediaLength(defaultMaxHeight, fallback);
}

function getAvailablePostHeight() {
    return Math.max(0, window.innerHeight - getScrollPaddingTop() - POST_MEDIA_FIT_BOTTOM_GAP);
}

function isMeasurableElement(element) {
    const rect = element.getBoundingClientRect();

    return rect.width > 0 && rect.height > 0;
}

function hasKnownMediaSize(element) {
    const width = Number.parseFloat(element.getAttribute("width") || "");
    const height = Number.parseFloat(element.getAttribute("height") || "");

    return width > 0 && height > 0;
}

function isSingleMediaPost(post) {
    if (!(post instanceof Element) || !post.classList.contains("post")) {
        return false;
    }

    if (post.querySelector(".gallery")) {
        return false;
    }

    const postType = post.dataset.postType;
    if (!postType) {
        return true;
    }

    return postType === "image" || postType === "video" || postType === "gif";
}

function getSinglePostMediaElement(post) {
    if (!isSingleMediaPost(post)) {
        return null;
    }

    const mediaElements = Array.from(post.querySelectorAll(SINGLE_POST_MEDIA_SELECTOR))
        .filter((element) => isMeasurableElement(element) || hasKnownMediaSize(element));

    return mediaElements.length === 1 ? mediaElements[0] : null;
}

function fitSinglePostMediaToViewport(post) {
    if (!(post instanceof HTMLElement)) {
        return;
    }

    post.style.removeProperty("--post-media-fit-height");

    const mediaElement = getSinglePostMediaElement(post);
    if (mediaElement === null) {
        return;
    }

    const availablePostHeight = getAvailablePostHeight();
    const baseMaxHeight = getPostMediaBaseMaxHeight(post);
    if (availablePostHeight <= 0 || baseMaxHeight <= 0) {
        return;
    }

    const mediaRect = mediaElement.getBoundingClientRect();
    const mediaHeight = Math.max(0, mediaRect.height);
    if (mediaHeight <= 0 && !hasKnownMediaSize(mediaElement)) {
        return;
    }

    const postRect = post.getBoundingClientRect();
    if (mediaHeight > 0 && postRect.height <= availablePostHeight + 1) {
        return;
    }

    const nonMediaHeight = Math.max(0, postRect.height - mediaHeight);
    let fitHeight = Math.min(
        baseMaxHeight,
        Math.max(POST_MEDIA_FIT_MIN_HEIGHT, availablePostHeight - nonMediaHeight),
    );

    post.style.setProperty("--post-media-fit-height", `${Math.floor(fitHeight)}px`);

    for (let pass = 0; pass < 2; pass += 1) {
        const overflow = post.getBoundingClientRect().height - availablePostHeight;
        if (overflow <= 1) {
            break;
        }

        const currentMediaHeight = mediaElement.getBoundingClientRect().height;
        if (currentMediaHeight <= POST_MEDIA_FIT_MIN_HEIGHT + 1) {
            break;
        }

        fitHeight = Math.max(POST_MEDIA_FIT_MIN_HEIGHT, fitHeight - Math.ceil(overflow));
        post.style.setProperty("--post-media-fit-height", `${Math.floor(fitHeight)}px`);
    }
}

function getPostsFromRoot(root) {
    const container = root || document;
    if (container instanceof Element && container.classList.contains("post")) {
        return [container];
    }

    if (typeof container.querySelectorAll !== "function") {
        return [];
    }

    return Array.from(container.querySelectorAll(".post"));
}

function resizeSinglePostMediaToViewport(root = document) {
    getPostsFromRoot(root).forEach(fitSinglePostMediaToViewport);
}

function scheduleSinglePostMediaResize(root = document) {
    if (postMediaFitFrame !== null) {
        window.cancelAnimationFrame(postMediaFitFrame);
    }

    postMediaFitFrame = window.requestAnimationFrame(() => {
        postMediaFitFrame = null;
        resizeSinglePostMediaToViewport(root);
    });
}

function handlePostMediaLoaded(event) {
    const post = event.target instanceof Element ? event.target.closest(".post") : null;
    if (post !== null) {
        scheduleSinglePostMediaResize(post);
    }
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

function getViewportAdvanceHeight() {
    const paddingTop = getScrollPaddingTop();
    const viewportHeight = window.innerHeight;

    return Math.max(1, viewportHeight - paddingTop);
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

function isSpaceNavigationKey(event) {
    return getKeyName(event) === " " || event.code === "Space" || event.code === "Spacebar";
}

function getScrollNavigationDirection(event) {
    if (isSpaceNavigationKey(event)) {
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
    const isSpaceKey = isSpaceNavigationKey(event);
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

    resizeSinglePostMediaToViewport(document);

    const targets = getScrollTargets();
    if (targets.length === 0) {
        return;
    }

    const scrollPaddingTop = getScrollPaddingTop();
    const targetPost = isSpaceKey && !event.shiftKey
        ? findViewportSkipTarget(targets)
        : findAdjacentScrollTarget(targets, window.scrollY + scrollPaddingTop, scrollDirection);

    if (targetPost === null) {
        if (isSpaceKey) {
            event.preventDefault();
            window.scrollTo({
                top: Math.max(0, window.scrollY + (getViewportAdvanceHeight() * scrollDirection)),
                behavior: "auto",
            });
        }

        return;
    }

    event.preventDefault();
    window.scrollTo({
        top: Math.max(0, getPostTop(targetPost) - scrollPaddingTop),
        behavior: "auto",
    });
});

window.resizeSinglePostMediaToViewport = resizeSinglePostMediaToViewport;
document.addEventListener("DOMContentLoaded", () => resizeSinglePostMediaToViewport(document));
window.addEventListener("resize", () => scheduleSinglePostMediaResize(document));
window.addEventListener("orientationchange", () => scheduleSinglePostMediaResize(document));
document.addEventListener("load", handlePostMediaLoaded, true);
document.addEventListener("loadedmetadata", handlePostMediaLoaded, true);
