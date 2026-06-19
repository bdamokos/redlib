function getScrollPaddingTop() {
    const scrollPaddingTop = window.getComputedStyle(document.documentElement).scrollPaddingTop;
    const parsedValue = Number.parseFloat(scrollPaddingTop);

    return Number.isNaN(parsedValue) ? 0 : parsedValue;
}

// Media should fit after the visible title and source metadata while leaving a hint for overflow below.
const POST_MEDIA_SELECTOR = [
    ".post_media_column .video_preview_stage",
    ".post_media_content > .post_media_embed",
    ".post_media_content > .post_media_image > img",
    ".post_media_content > .post_media_image > svg",
    ".gallery figure img",
    ".gallery figure svg",
].join(", ");

const POST_MEDIA_FIT_BOTTOM_GAP = 8;
const POST_MEDIA_CONTINUATION_PEEK_LINES = 1.5;
const POST_MEDIA_CONTINUATION_PEEK_MIN_HEIGHT = 32;
const POST_MEDIA_FIT_MIN_HEIGHT = 96;
const POST_MEDIA_STRETCHED_HEADER_TOLERANCE = 64;
const POST_CONTINUATION_SELECTOR = ".post_body, .post_poll, .post_following_comments, .post_duplicates";
const POST_MEDIA_HEADER_SELECTOR = ".post_header, .post_title, .post_notification";
const EXPANDABLE_POST_PREVIEW_SELECTOR = ".post_preview.truncated:not(.expanded)";
const PAGE_EDGE_TOLERANCE = 1;
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

function isMediaPost(post) {
    if (!(post instanceof Element) || !post.classList.contains("post")) {
        return false;
    }

    const postType = post.dataset.postType;
    if (!postType) {
        return post.querySelector(POST_MEDIA_SELECTOR) !== null;
    }

    return postType === "image" || postType === "video" || postType === "gif" || postType === "gallery";
}

function getPostMediaElements(post) {
    if (!isMediaPost(post)) {
        return [];
    }

    return Array.from(post.querySelectorAll(POST_MEDIA_SELECTOR))
        .filter((element) => isMeasurableElement(element) || hasKnownMediaSize(element));
}

function getMediaFitGroup(mediaElement) {
    return mediaElement.closest(".post_media_column, .post_media_content > .post_media_embed, .post_media_content, .gallery figure") || mediaElement;
}

function hasContinuationContent(element) {
    return element.textContent.trim() !== "" || element.querySelector("img, video, iframe, svg, li, .poll_option") !== null;
}

function getContinuationElements(post) {
    return Array.from(post.querySelectorAll(POST_CONTINUATION_SELECTOR))
        .filter(hasContinuationContent);
}

function getContinuationStartElement(post, mediaElements) {
    if (mediaElements.length > 1) {
        return getMediaFitGroup(mediaElements[1]);
    }

    const continuationElements = getContinuationElements(post);
    if (continuationElements.length > 0) {
        return continuationElements[0];
    }

    return null;
}

function getContinuationPeekHeight(post, continuationElements) {
    const sampleElement = continuationElements[0] || post;
    const styles = window.getComputedStyle(sampleElement);
    const fontSize = Number.parseFloat(styles.fontSize) || 15;
    let lineHeight = Number.parseFloat(styles.lineHeight);

    if (Number.isNaN(lineHeight)) {
        lineHeight = fontSize * 1.45;
    }

    return Math.max(
        POST_MEDIA_CONTINUATION_PEEK_MIN_HEIGHT,
        Math.ceil(lineHeight * POST_MEDIA_CONTINUATION_PEEK_LINES),
    );
}

function getContentBottom(element) {
    const children = Array.from(element.children)
        .map((child) => child.getBoundingClientRect())
        .filter((rect) => rect.width > 0 && rect.height > 0);

    if (children.length === 0) {
        return element.getBoundingClientRect().bottom;
    }

    return Math.max(...children.map((rect) => rect.bottom));
}

function getStableTopOverheadHeight(post, mediaGroupElement, actualTopOverheadHeight) {
    const postRect = post.getBoundingClientRect();
    const mediaGroupRect = mediaGroupElement.getBoundingClientRect();
    const mediaTop = mediaGroupRect.top;
    const contentBottom = Array.from(post.querySelectorAll(POST_MEDIA_HEADER_SELECTOR))
        .reduce((bottom, element) => {
            const rect = element.getBoundingClientRect();
            if (rect.width <= 0 || rect.height <= 0 || rect.top >= mediaTop) {
                return bottom;
            }

            return Math.max(bottom, getContentBottom(element) - postRect.top);
        }, 0);

    if (contentBottom <= 0) {
        return actualTopOverheadHeight;
    }

    const topGap = Math.max(
        0,
        Number.parseFloat(window.getComputedStyle(mediaGroupElement).marginTop) || 0,
    );

    const stableTopOverheadHeight = contentBottom + topGap;
    if (actualTopOverheadHeight <= stableTopOverheadHeight + POST_MEDIA_STRETCHED_HEADER_TOLERANCE) {
        return actualTopOverheadHeight;
    }

    return stableTopOverheadHeight;
}

function getContinuationGapHeight(mediaGroupElement, continuationStartElement) {
    if (continuationStartElement === null) {
        return 0;
    }

    return Math.max(
        0,
        continuationStartElement.getBoundingClientRect().top - mediaGroupElement.getBoundingClientRect().bottom,
    );
}

function getContinuationFitHeight(post, mediaElement, mediaElements, availablePostHeight, baseMaxHeight) {
    const postRect = post.getBoundingClientRect();
    const mediaRect = mediaElement.getBoundingClientRect();
    const mediaHeight = Math.max(0, mediaRect.height);
    const mediaGroupElement = getMediaFitGroup(mediaElement);
    const mediaGroupRect = mediaGroupElement.getBoundingClientRect();
    const continuationElements = getContinuationElements(post);
    const hasContinuation = mediaElements.length > 1 || continuationElements.length > 0;
    const continuationStartElement = getContinuationStartElement(post, mediaElements);

    if (!hasContinuation) {
        return null;
    }

    const actualTopOverheadHeight = Math.max(0, mediaGroupRect.top - postRect.top);
    const topOverheadHeight = getStableTopOverheadHeight(post, mediaGroupElement, actualTopOverheadHeight);
    const mediaChromeHeight = Math.max(0, mediaGroupRect.height - mediaHeight);
    const continuationGapHeight = getContinuationGapHeight(mediaGroupElement, continuationStartElement);
    const continuationPeekHeight = getContinuationPeekHeight(post, continuationElements);

    return Math.min(
        baseMaxHeight,
        Math.max(
            POST_MEDIA_FIT_MIN_HEIGHT,
            availablePostHeight - topOverheadHeight - mediaChromeHeight - continuationGapHeight - continuationPeekHeight,
        ),
    );
}

function getContinuationOverflow(post, mediaElement, mediaElements, availablePostHeight) {
    const mediaGroupElement = getMediaFitGroup(mediaElement);
    const postRect = post.getBoundingClientRect();
    const mediaGroupRect = mediaGroupElement.getBoundingClientRect();
    const actualTopOverheadHeight = Math.max(0, mediaGroupRect.top - postRect.top);
    const topOverheadHeight = getStableTopOverheadHeight(post, mediaGroupElement, actualTopOverheadHeight);
    const continuationElements = getContinuationElements(post);
    const continuationStartElement = getContinuationStartElement(post, mediaElements);
    const continuationStartHeight = continuationStartElement === null
        ? topOverheadHeight + mediaGroupRect.height
        : Math.max(0, continuationStartElement.getBoundingClientRect().top - postRect.top);

    return continuationStartHeight
        + getContinuationPeekHeight(post, continuationElements)
        - availablePostHeight;
}

function fitPostMediaToViewport(post) {
    if (!(post instanceof HTMLElement)) {
        return;
    }

    post.style.removeProperty("--post-media-fit-height");

    const mediaElements = getPostMediaElements(post);
    const mediaElement = mediaElements[0] || null;
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
    const continuationFitHeight = getContinuationFitHeight(post, mediaElement, mediaElements, availablePostHeight, baseMaxHeight);
    const nonMediaHeight = Math.max(0, postRect.height - mediaHeight);
    let fitHeight = continuationFitHeight ?? Math.min(
        baseMaxHeight,
        Math.max(POST_MEDIA_FIT_MIN_HEIGHT, availablePostHeight - nonMediaHeight),
    );

    if (continuationFitHeight === null && mediaHeight > 0 && postRect.height <= availablePostHeight + 1) {
        return;
    }

    post.style.setProperty("--post-media-fit-height", `${Math.floor(fitHeight)}px`);

    for (let pass = 0; pass < 4; pass += 1) {
        const overflow = continuationFitHeight === null
            ? post.getBoundingClientRect().height - availablePostHeight
            : getContinuationOverflow(post, mediaElement, mediaElements, availablePostHeight);
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
    getPostsFromRoot(root).forEach(fitPostMediaToViewport);
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

function getMaxScrollTop() {
    return Math.max(
        0,
        Math.max(
            document.documentElement.scrollHeight,
            document.body?.scrollHeight || 0,
        ) - window.innerHeight,
    );
}

function isAtPageEdge(direction) {
    if (direction > 0) {
        return window.scrollY >= getMaxScrollTop() - PAGE_EDGE_TOLERANCE;
    }

    if (direction < 0) {
        return window.scrollY <= PAGE_EDGE_TOLERANCE;
    }

    return false;
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

function isVisibleInViewport(element) {
    const rect = element.getBoundingClientRect();

    return rect.bottom > getScrollPaddingTop() + 1 && rect.top < window.innerHeight - 1;
}

function findExpandablePostPreview() {
    const expandablePreviews = Array.from(document.querySelectorAll(EXPANDABLE_POST_PREVIEW_SELECTOR))
        .filter(isVisibleTarget);
    const visiblePreview = expandablePreviews.find(isVisibleInViewport);

    return visiblePreview || expandablePreviews.find((preview) => preview.getBoundingClientRect().top >= getScrollPaddingTop()) || null;
}

function expandPostPreview(preview) {
    if (!(preview instanceof Element)) {
        return false;
    }

    preview.classList.add("expanded");
    preview.classList.remove("truncated");
    return true;
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

function isFollowShortcutKey(event) {
    return !event.repeat && getKeyName(event) === "f";
}

function isExpandPreviewShortcutKey(event) {
    return !event.repeat && getKeyName(event) === "m";
}

function getPaginationAccessKeyForDirection(direction) {
    if (direction > 0) {
        return "N";
    }

    if (direction < 0) {
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

function getPaginationLinkForDirection(direction) {
    return getPaginationLink(getPaginationAccessKeyForDirection(direction));
}

document.addEventListener("keydown", (event) => {
    const isSpaceKey = isSpaceNavigationKey(event);
    const isFollowShortcut = isFollowShortcutKey(event);
    const isExpandPreviewShortcut = isExpandPreviewShortcutKey(event);
    const scrollDirection = getScrollNavigationDirection(event);
    const paginationLink = getPaginationLink(getPaginationAccessKey(event));
    const isScrollNavigationKey = scrollDirection !== 0;
    const isPaginationKey = paginationLink !== null;

    if (
        event.defaultPrevented
        || (!isScrollNavigationKey && !isPaginationKey && !isFollowShortcut && !isExpandPreviewShortcut)
        || event.altKey
        || event.ctrlKey
        || event.metaKey
    ) {
        return;
    }

    if (isInteractiveElement(event.target) || isInteractiveElement(document.activeElement)) {
        return;
    }

    if (isFollowShortcut) {
        if (window.togglePrimarySubscription?.() === true) {
            event.preventDefault();
        }
        return;
    }

    if (isExpandPreviewShortcut) {
        if (expandPostPreview(findExpandablePostPreview())) {
            event.preventDefault();
        }
        return;
    }

    if (isPaginationKey) {
        event.preventDefault();
        paginationLink.click();
        return;
    }

    if (isSpaceKey) {
        event.preventDefault();
        window.scrollTo({
            top: Math.max(0, window.scrollY + (getViewportAdvanceHeight() * scrollDirection)),
            behavior: "auto",
        });
        return;
    }

    const paginationLinkForDirection = getPaginationLinkForDirection(scrollDirection);
    if (paginationLinkForDirection !== null && isAtPageEdge(scrollDirection)) {
        event.preventDefault();
        paginationLinkForDirection.click();
        return;
    }

    const targets = getScrollTargets();
    if (targets.length === 0) {
        return;
    }

    const scrollPaddingTop = getScrollPaddingTop();
    const targetPost = findAdjacentScrollTarget(targets, window.scrollY + scrollPaddingTop, scrollDirection);

    if (targetPost === null) {
        if (paginationLinkForDirection !== null) {
            event.preventDefault();
            paginationLinkForDirection.click();
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
