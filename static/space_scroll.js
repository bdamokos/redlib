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
const TEMPORARY_AUTHOR_FILTER_STORAGE_KEY = "redlib-temporary-author-filters-v1";
const TEMPORARY_AUTHOR_FILTER_DURATION_MS = 24 * 60 * 60 * 1000;
const FILTERED_AUTHOR_SELECTOR = "[data-temporary-author-filtered='true'], [data-permanent-author-filtered='true']";
const PAGE_BOUNDARY_NAVIGATION_STORAGE_KEY = "redlib-page-boundary-navigation-v1";
const PAGE_BOUNDARY_NAVIGATION_MAX_AGE_MS = 30 * 1000;
const PAGE_EDGE_TOLERANCE = 1;
const POST_MEDIA_FIT_READY_ATTR = "data-post-media-fit-ready";
let postMediaFitFrame = null;
let temporaryAuthorFilterMemory = [];
let redlibToastTimeout = null;
let lastTemporaryAuthorFilterToastKey = "";

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

function isMobileViewport() {
    return window.matchMedia?.("(max-width: 800px)")?.matches ?? window.innerWidth <= 800;
}

function isFirstListingPost(post) {
    const postsContainer = post instanceof Element ? post.closest("#posts") : null;
    if (postsContainer === null) {
        return false;
    }

    return Array.from(postsContainer.children)
        .find((child) => child instanceof Element && child.classList.contains("post")) === post;
}

function isStackedUserProfileVisibleAbovePost(post) {
    if (!isUserPage() || !isMobileViewport() || !isFirstListingPost(post)) {
        return false;
    }

    const main = post.closest("main");
    const userProfile = main?.querySelector(":scope > aside #user") || null;
    if (!(userProfile instanceof Element)) {
        return false;
    }

    const viewportTop = getScrollPaddingTop();
    const profileRect = userProfile.getBoundingClientRect();
    const postRect = post.getBoundingClientRect();

    return profileRect.bottom > viewportTop + 1 && profileRect.top < postRect.top;
}

function getAvailablePostHeight(post = null, options = {}) {
    const viewportTop = getScrollPaddingTop();
    let frameTop = viewportTop;

    if (options.useCurrentPostTop && post instanceof Element && !isStackedUserProfileVisibleAbovePost(post)) {
        const postRect = post.getBoundingClientRect();
        if (postRect.top > viewportTop && postRect.top < window.innerHeight) {
            frameTop = postRect.top;
        }
    }

    return Math.max(0, window.innerHeight - frameTop - POST_MEDIA_FIT_BOTTOM_GAP);
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

function markPostMediaFitReady(post) {
    post.setAttribute(POST_MEDIA_FIT_READY_ATTR, "true");
}

function isPostMediaFitReady(post) {
    return post.getAttribute(POST_MEDIA_FIT_READY_ATTR) === "true";
}

function fitPostMediaToViewport(post, options = {}) {
    if (!(post instanceof HTMLElement)) {
        return;
    }

    if (!options.force && isPostMediaFitReady(post)) {
        return;
    }

    post.style.removeProperty("--post-media-fit-height");

    const mediaElements = getPostMediaElements(post);
    const mediaElement = mediaElements[0] || null;
    if (mediaElement === null) {
        return;
    }

    const availablePostHeight = getAvailablePostHeight(post, {
        useCurrentPostTop: options.viewportAnchorPost === post,
    });
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
        markPostMediaFitReady(post);
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

    markPostMediaFitReady(post);
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

function resizeSinglePostMediaToViewport(root = document, options = {}) {
    const viewportAnchorPost = options.viewportAnchorPost || findCurrentViewedPost();
    getPostsFromRoot(root).forEach((post) => fitPostMediaToViewport(post, {
        ...options,
        viewportAnchorPost,
    }));
}

function resizePostMediaToViewportOnLoad() {
    resizeSinglePostMediaToViewport(document);
}

function scheduleSinglePostMediaResize(root = document, options = {}) {
    if (postMediaFitFrame !== null) {
        window.cancelAnimationFrame(postMediaFitFrame);
    }

    postMediaFitFrame = window.requestAnimationFrame(() => {
        postMediaFitFrame = null;
        resizeSinglePostMediaToViewport(root, options);
    });
}

function isIpadViewportDevice() {
    const navigatorInfo = window.navigator || {};
    const userAgent = navigatorInfo.userAgent || "";
    const platform = navigatorInfo.platform || "";
    const maxTouchPoints = Number(navigatorInfo.maxTouchPoints) || 0;

    return /\biPad\b/.test(userAgent) || (platform === "MacIntel" && maxTouchPoints > 1);
}

function schedulePostMediaResizeAfterIpadOrientationChange() {
    if (!isIpadViewportDevice()) {
        return;
    }

    scheduleSinglePostMediaResize(document, { force: true });
}

function handlePostMediaLoaded(event) {
    const post = event.target instanceof Element ? event.target.closest(".post") : null;
    if (post !== null && !isPostMediaFitReady(post)) {
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
    if (!(target instanceof Element)) {
        return false;
    }

    if (target.closest(FILTERED_AUTHOR_SELECTOR) !== null) {
        return false;
    }

    if (target instanceof HTMLElement && target.hidden) {
        return false;
    }

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

function isOpenPostContextShortcutKey(event) {
    return !event.repeat && getKeyName(event) === "u";
}

function isOpenPostOnRedditShortcutKey(event) {
    return !event.repeat && getKeyName(event) === "r";
}

function isFilterPostAuthorShortcutKey(event) {
    return !event.repeat && getKeyName(event) === "x";
}

function isTemporaryFilterPostAuthorShortcutKey(event) {
    return !event.repeat && getKeyName(event) === "y";
}

function isLikePostFeedbackShortcutKey(event) {
    return !event.repeat && getKeyName(event) === "w";
}

function isDislikePostFeedbackShortcutKey(event) {
    return !event.repeat && getKeyName(event) === "s";
}

function isUserPage() {
    return /^\/(?:user|u)\//.test(window.location.pathname);
}

function isSubredditListingPagePath(pathname) {
    const match = pathname.match(/^\/r\/[^/]+(?:\/([^/]+))?\/?$/);
    const listing = match?.[1] || "";

    return match !== null && (listing === "" || ["best", "hot", "new", "top", "rising", "controversial", "video-length"].includes(listing));
}

function subredditTemporaryAuthorFilterScope(pathname) {
    const match = pathname.match(/^\/r\/([^/]+)(?:\/([^/]+))?\/?$/);
    if (match === null || !isSubredditListingPagePath(pathname)) {
        return "";
    }

    return `subreddit:${decodeURIComponent(match[1]).toLowerCase()}`;
}

function isFrontPageListingPagePath(pathname) {
    return /^\/(?:best|hot|new|random|top|rising|controversial|video-length)?\/?$/.test(pathname);
}

function temporaryAuthorFilterScope() {
    return subredditTemporaryAuthorFilterScope(window.location.pathname)
        || (isFrontPageListingPagePath(window.location.pathname) ? "front" : "");
}

function isUserListingPagePath(pathname) {
    const match = pathname.match(/^\/(?:user|u)\/[^/]+(?:\/([^/]+))?\/?$/);
    const listing = match?.[1] || "";

    return match !== null && (listing === "" || ["overview", "comments", "submitted", "videos", "hot", "new", "top", "controversial"].includes(listing));
}

function isListingPageWithOwnRedditTarget() {
    return isSubredditListingPagePath(window.location.pathname) || isUserListingPagePath(window.location.pathname);
}

function findCurrentViewedPost() {
    const posts = getPostsFromRoot(document).filter(isVisibleTarget);
    const viewportTop = getScrollPaddingTop();
    const viewportBottom = window.innerHeight;
    const visiblePosts = posts.filter((post) => {
        const rect = post.getBoundingClientRect();

        return rect.bottom > viewportTop + 1 && rect.top < viewportBottom - 1;
    });

    const containingViewportTop = visiblePosts
        .filter((post) => {
            const rect = post.getBoundingClientRect();

            return rect.top <= viewportTop + 1 && rect.bottom > viewportTop + 1;
        })
        .sort((left, right) => right.getBoundingClientRect().top - left.getBoundingClientRect().top);

    return containingViewportTop[0] || visiblePosts[0] || posts.find((post) => post.getBoundingClientRect().top >= viewportTop) || null;
}

function getPostContextLink(post) {
    if (!(post instanceof Element)) {
        return null;
    }

    const selector = isUserPage() ? ".post_subreddit[href^='/r/']" : ".post_author[href]";

    return post.querySelector(selector);
}

function getPostAuthorPreferenceValue(post) {
    if (!(post instanceof Element)) {
        return "";
    }

    const authorLink = post.querySelector(".post_author[href]");
    if (!(authorLink instanceof HTMLAnchorElement)) {
        return "";
    }

    const match = authorLink.pathname.match(/^\/(?:u|user)\/([^/?#]+)/);
    if (match === null) {
        return "";
    }

    const username = decodeURIComponent(match[1]).trim();
    if (username === "" || username === "[deleted]") {
        return "";
    }

    return `u_${username}`;
}

function normalizeTemporaryAuthorFilterAuthor(value) {
    const author = value.trim();
    const normalized = author.startsWith("u_") ? author : `u_${author}`;

    return /^u_[A-Za-z0-9_-]+$/.test(normalized) ? normalized.toLowerCase() : "";
}

function normalizeTemporaryAuthorFilterEntry(entry, now = Date.now()) {
    if (typeof entry !== "object" || entry === null) {
        return null;
    }

    const scope = typeof entry.scope === "string" ? entry.scope.trim().toLowerCase() : "";
    const author = typeof entry.author === "string" ? normalizeTemporaryAuthorFilterAuthor(entry.author) : "";
    const expiresAt = Number(entry.expiresAt);

    if (scope === "" || author === "" || !Number.isFinite(expiresAt) || expiresAt <= now) {
        return null;
    }

    return { scope, author, expiresAt };
}

function compactTemporaryAuthorFilters(entries, now = Date.now()) {
    const compacted = new Map();

    for (const entry of entries) {
        const normalized = normalizeTemporaryAuthorFilterEntry(entry, now);
        if (normalized === null) {
            continue;
        }

        const key = `${normalized.scope}\t${normalized.author}`;
        const existing = compacted.get(key);
        if (!existing || normalized.expiresAt > existing.expiresAt) {
            compacted.set(key, normalized);
        }
    }

    return Array.from(compacted.values());
}

function readTemporaryAuthorFilters() {
    try {
        const rawValue = window.localStorage?.getItem(TEMPORARY_AUTHOR_FILTER_STORAGE_KEY);
        if (rawValue) {
            return compactTemporaryAuthorFilters(JSON.parse(rawValue));
        }
    } catch (_error) {
        // Fall back to memory-only filtering if storage is unavailable or corrupt.
    }

    return compactTemporaryAuthorFilters(temporaryAuthorFilterMemory);
}

function writeTemporaryAuthorFilters(entries) {
    const compacted = compactTemporaryAuthorFilters(entries);
    temporaryAuthorFilterMemory = compacted;

    try {
        if (compacted.length === 0) {
            window.localStorage?.removeItem(TEMPORARY_AUTHOR_FILTER_STORAGE_KEY);
        } else {
            window.localStorage?.setItem(TEMPORARY_AUTHOR_FILTER_STORAGE_KEY, JSON.stringify(compacted));
        }
    } catch (_error) {
        // The in-memory copy still handles the current page.
    }

    return compacted;
}

function separatorForPost(post) {
    const separator = post.previousElementSibling;

    return separator instanceof Element && separator.matches("hr.sep") ? separator : null;
}

function stopStreamingPlayer(video) {
    const players = [
        video._dashjs_player,
        video._hlsjs_player,
    ].filter(Boolean);

    for (const player of players) {
        try {
            player.pause?.();
            player.stopLoad?.();
            player.reset?.();
            player.destroy?.();
        } catch (_error) {}
    }

    delete video._dashjs_player;
    delete video._hlsjs_player;
}

function unloadVideoElement(video) {
    stopStreamingPlayer(video);
    video.muted = true;
    video.pause();
    video.preload = "none";
    video.removeAttribute("autoplay");
    video.removeAttribute("poster");
    video.querySelectorAll("source").forEach((source) => {
        source.removeAttribute("src");
        source.removeAttribute("srcset");
    });
    video.removeAttribute("src");
    try {
        video.srcObject = null;
    } catch (_error) {}
    if (video.classList.contains("video_preview_fallback_video")) {
        video.hidden = true;
    }
    video.load();
}

function unloadImageElement(image) {
    image.removeAttribute("src");
    image.removeAttribute("srcset");
    image.removeAttribute("sizes");
    image.removeAttribute("data-full-src");
    image.removeAttribute("data-lazy-src");
    image.removeAttribute("href");
    image.removeAttribute("xlink:href");
}

function unloadIframeElement(iframe) {
    iframe.setAttribute("src", "about:blank");
    iframe.removeAttribute("srcdoc");
}

function unloadPostMedia(post) {
    post.querySelectorAll("video").forEach(unloadVideoElement);
    post.querySelectorAll("audio").forEach(unloadVideoElement);
    post.querySelectorAll("img, image").forEach(unloadImageElement);
    post.querySelectorAll("iframe").forEach(unloadIframeElement);
    post.dispatchEvent(new CustomEvent("redlib:post-media-unloaded", {
        bubbles: true,
        detail: { post },
    }));
}

function setPostAuthorFiltered(post, filtered, attributeName) {
    if (!(post instanceof HTMLElement)) {
        return;
    }

    const separator = separatorForPost(post);
    if (filtered) {
        post.setAttribute(attributeName, "true");
        unloadPostMedia(post);
        post.hidden = true;
        if (separator !== null) {
            separator.setAttribute(attributeName, "true");
            separator.hidden = true;
        }
        return;
    }

    if (post.getAttribute(attributeName) === "true") {
        post.removeAttribute(attributeName);
        post.hidden = post.matches(FILTERED_AUTHOR_SELECTOR);
    }
    if (separator !== null && separator.getAttribute(attributeName) === "true") {
        separator.removeAttribute(attributeName);
        separator.hidden = separator.matches(FILTERED_AUTHOR_SELECTOR);
    }
}

function setPostTemporarilyAuthorFiltered(post, filtered) {
    setPostAuthorFiltered(post, filtered, "data-temporary-author-filtered");
}

function setPostPermanentlyAuthorFiltered(post, filtered) {
    setPostAuthorFiltered(post, filtered, "data-permanent-author-filtered");
}

function temporaryAuthorFilterDisplayName(author) {
    return author.startsWith("u_") ? `u/${author.slice(2)}` : author;
}

function showRedlibToast(message) {
    if (message === "") {
        return;
    }

    let toast = document.getElementById("redlib_toast");
    if (!toast) {
        toast = document.createElement("div");
        toast.id = "redlib_toast";
        toast.className = "redlib_toast";
        toast.setAttribute("role", "status");
        toast.setAttribute("aria-live", "polite");
        document.body.append(toast);
    }

    toast.textContent = message;
    toast.classList.add("visible");

    if (redlibToastTimeout !== null) {
        window.clearTimeout(redlibToastTimeout);
    }
    redlibToastTimeout = window.setTimeout(() => {
        toast.classList.remove("visible");
        redlibToastTimeout = null;
    }, 2600);
}

function showTemporaryAuthorFilterToast(authorCounts) {
    const visibleAuthorCounts = Array.from(authorCounts.entries()).filter(([, count]) => count > 0);
    if (visibleAuthorCounts.length === 0) {
        return;
    }

    const toastKey = visibleAuthorCounts
        .map(([author, count]) => `${author}:${count}`)
        .sort()
        .join("|");
    if (toastKey === lastTemporaryAuthorFilterToastKey) {
        return;
    }
    lastTemporaryAuthorFilterToastKey = toastKey;

    const authors = visibleAuthorCounts.map(([author, count]) => {
        const label = temporaryAuthorFilterDisplayName(author);
        return count === 1 ? label : `${label} (${count})`;
    });
    const authorList = authors.length > 2
        ? `${authors.slice(0, 2).join(", ")} and ${authors.length - 2} more`
        : authors.join(" and ");
    const message = `Temporarily filtered out ${authorList} for 24h`;

    showRedlibToast(message);
}

function applyTemporaryAuthorFilters(root = document, options = {}) {
    const scope = temporaryAuthorFilterScope();
    const entries = writeTemporaryAuthorFilters(readTemporaryAuthorFilters());
    const activeAuthors = new Set(entries.filter((entry) => entry.scope === scope).map((entry) => entry.author));
    const filteredCounts = new Map();

    for (const post of getPostsFromRoot(root)) {
        const author = normalizeTemporaryAuthorFilterAuthor(getPostAuthorPreferenceValue(post));
        const filtered = scope !== "" && activeAuthors.has(author);
        setPostTemporarilyAuthorFiltered(post, filtered);
        if (filtered) {
            filteredCounts.set(author, (filteredCounts.get(author) || 0) + 1);
        }
    }

    if (options.toast !== false) {
        showTemporaryAuthorFilterToast(filteredCounts);
    }
}

function filterPostAuthor(post) {
    const filterValue = getPostAuthorPreferenceValue(post);
    if (filterValue === "" || typeof window.filterPreferenceValue !== "function") {
        return false;
    }

    window.filterPreferenceValue(filterValue);
    const author = normalizeTemporaryAuthorFilterAuthor(filterValue);
    let filteredAnyPost = false;
    for (const post of getPostsFromRoot(document)) {
        const postAuthor = normalizeTemporaryAuthorFilterAuthor(getPostAuthorPreferenceValue(post));
        if (author !== "" && postAuthor === author) {
            setPostPermanentlyAuthorFiltered(post, true);
            filteredAnyPost = true;
        }
    }

    return filteredAnyPost;
}

function filterCurrentPostAuthor() {
    return filterPostAuthor(findCurrentViewedPost());
}

function temporarilyFilterPostAuthor(post) {
    const scope = temporaryAuthorFilterScope();
    if (scope === "") {
        return false;
    }

    const author = normalizeTemporaryAuthorFilterAuthor(getPostAuthorPreferenceValue(post));
    if (author === "") {
        return false;
    }

    const entries = readTemporaryAuthorFilters()
        .filter((entry) => entry.scope !== scope || entry.author !== author);
    entries.push({
        scope,
        author,
        expiresAt: Date.now() + TEMPORARY_AUTHOR_FILTER_DURATION_MS,
    });
    writeTemporaryAuthorFilters(entries);
    applyTemporaryAuthorFilters(document);
    return true;
}

function temporarilyFilterCurrentPostAuthor() {
    return temporarilyFilterPostAuthor(findCurrentViewedPost());
}

function submitCurrentPostFeedback(signal) {
    if (typeof window.submitPostFeedback !== "function") {
        return false;
    }

    return window.submitPostFeedback(findCurrentViewedPost(), signal, { source: "keyboard" }) === true;
}

function postFeedbackToastMessage(signal, requestedSignal) {
    if (signal === "like") {
        return "Post liked";
    }
    if (signal === "dislike") {
        return "Post disliked";
    }
    if (requestedSignal === "like" || requestedSignal === "dislike" || requestedSignal === "clear") {
        return "Post rating cleared";
    }

    return "";
}

function isCurrentPostAuthorMarkedFollowed(post) {
    if (!(post instanceof Element)) {
        return false;
    }

    return post.querySelector(".post_header .followed_author_marker") !== null;
}

function followCurrentPostAuthor() {
    const post = findCurrentViewedPost();
    const followValue = getPostAuthorPreferenceValue(post);
    if (followValue === "" || typeof window.followPreferenceValue !== "function") {
        return false;
    }

    return window.followPreferenceValue(followValue, {
        alreadyFollowed: isCurrentPostAuthorMarkedFollowed(post),
    }) === true;
}

function initializePostAuthorActions(root = document) {
    const temporaryFilterAvailable = temporaryAuthorFilterScope() !== "";
    const container = root || document;
    const temporaryFilterButtons = container.querySelectorAll?.("[data-post-author-action='temporary-filter']") || [];

    for (const button of temporaryFilterButtons) {
        button.hidden = !temporaryFilterAvailable;
    }
}

function handlePostAuthorActionClick(event) {
    const actionButton = event.target?.closest?.("[data-post-author-action]");
    if (!(actionButton instanceof HTMLButtonElement)) {
        return;
    }

    const post = actionButton.closest(".post");
    const action = actionButton.dataset.postAuthorAction || "";
    const handled = action === "temporary-filter"
        ? temporarilyFilterPostAuthor(post)
        : action === "permanent-filter"
            ? filterPostAuthor(post)
            : false;

    if (handled) {
        event.preventDefault();
    }
}

function openCurrentPostContext() {
    const link = getPostContextLink(findCurrentViewedPost());
    if (!(link instanceof HTMLAnchorElement) || link.getAttribute("href") === "") {
        return false;
    }

    const openedWindow = window.open(link.href, "_blank", "noopener");
    openedWindow?.blur?.();
    window.focus();
    return true;
}

function getPostRedditUrl(post) {
    if (!(post instanceof Element)) {
        return "";
    }

    const redditUrl = post.getAttribute("data-reddit-url") || "";
    if (redditUrl !== "") {
        return redditUrl;
    }

    const permalink = post.querySelector(".post_comments[href^='/r/'], .post_title a[href^='/r/'], #post_links a[href^='/r/']");
    if (!(permalink instanceof HTMLAnchorElement)) {
        return "";
    }

    return `https://www.reddit.com${permalink.pathname}${permalink.search}${permalink.hash}`;
}

function redditUrlFromLink(link) {
    if (!(link instanceof HTMLAnchorElement)) {
        return "";
    }

    return /^https:\/\/(?:www\.)?reddit\.com\//.test(link.href) ? link.href : "";
}

function getCurrentPageRedditUrl() {
    if (!isListingPageWithOwnRedditTarget()) {
        return "";
    }

    return redditUrlFromLink(document.getElementById("reddit_link")) || redditUrlFromLink(document.getElementById("toreddit"));
}

function openCurrentPostOnReddit() {
    const redditUrl = getCurrentPageRedditUrl() || getPostRedditUrl(findCurrentViewedPost());
    if (redditUrl === "") {
        return false;
    }

    window.location.assign(redditUrl);
    return true;
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

function normalizedNavigationUrl(value) {
    try {
        const url = new URL(value, window.location.href);
        url.hash = "";

        return `${url.origin}${url.pathname}${url.search}`;
    } catch (_error) {
        return "";
    }
}

function rememberBoundaryPaginationTarget(direction, paginationLink) {
    if (direction >= 0 || !(paginationLink instanceof HTMLAnchorElement)) {
        return;
    }

    const targetUrl = normalizedNavigationUrl(paginationLink.href);
    if (targetUrl === "") {
        return;
    }

    try {
        window.sessionStorage?.setItem(PAGE_BOUNDARY_NAVIGATION_STORAGE_KEY, JSON.stringify({
            targetUrl,
            direction,
            createdAt: Date.now(),
        }));
    } catch (_error) {
        // Pagination should still work if session storage is unavailable.
    }
}

function followPaginationLinkForDirection(direction, paginationLink) {
    rememberBoundaryPaginationTarget(direction, paginationLink);
    paginationLink.click();
}

function consumeBoundaryPaginationTarget() {
    let rawValue = "";

    try {
        rawValue = window.sessionStorage?.getItem(PAGE_BOUNDARY_NAVIGATION_STORAGE_KEY) || "";
        if (rawValue !== "") {
            window.sessionStorage?.removeItem(PAGE_BOUNDARY_NAVIGATION_STORAGE_KEY);
        }
    } catch (_error) {
        return null;
    }

    if (rawValue === "") {
        return null;
    }

    try {
        const value = JSON.parse(rawValue);
        const createdAt = Number(value.createdAt);
        const direction = Number(value.direction);

        if (
            typeof value.targetUrl !== "string"
            || value.targetUrl === ""
            || !Number.isFinite(createdAt)
            || Date.now() - createdAt > PAGE_BOUNDARY_NAVIGATION_MAX_AGE_MS
            || direction >= 0
            || value.targetUrl !== normalizedNavigationUrl(window.location.href)
        ) {
            return null;
        }

        return { direction };
    } catch (_error) {
        return null;
    }
}

function findLastVisiblePost() {
    const posts = getPostsFromRoot(document).filter(isVisibleTarget);

    return posts.length > 0 ? posts[posts.length - 1] : null;
}

function scrollToPostTop(post) {
    if (!(post instanceof Element)) {
        return false;
    }

    window.scrollTo({
        top: Math.max(0, getPostTop(post) - getScrollPaddingTop()),
        behavior: "auto",
    });
    return true;
}

function restoreBoundaryPaginationPosition() {
    const target = consumeBoundaryPaginationTarget();
    if (target === null || target.direction >= 0) {
        return;
    }

    window.requestAnimationFrame(() => {
        if (scrollToPostTop(findLastVisiblePost())) {
            window.setTimeout(() => scrollToPostTop(findLastVisiblePost()), 0);
        }
    });
}

document.addEventListener("redlib:post-feedback", (event) => {
    const detail = event.detail || {};
    if (detail.source !== "keyboard") {
        return;
    }

    showRedlibToast(postFeedbackToastMessage(detail.signal || "", detail.requestedSignal || ""));
});

document.addEventListener("keydown", (event) => {
    const isSpaceKey = isSpaceNavigationKey(event);
    const isFollowShortcut = isFollowShortcutKey(event);
    const isExpandPreviewShortcut = isExpandPreviewShortcutKey(event);
    const isOpenPostContextShortcut = isOpenPostContextShortcutKey(event);
    const isOpenPostOnRedditShortcut = isOpenPostOnRedditShortcutKey(event);
    const isFilterPostAuthorShortcut = isFilterPostAuthorShortcutKey(event);
    const isTemporaryFilterPostAuthorShortcut = isTemporaryFilterPostAuthorShortcutKey(event);
    const isLikePostFeedbackShortcut = isLikePostFeedbackShortcutKey(event);
    const isDislikePostFeedbackShortcut = isDislikePostFeedbackShortcutKey(event);
    const scrollDirection = getScrollNavigationDirection(event);
    const paginationLink = getPaginationLink(getPaginationAccessKey(event));
    const isScrollNavigationKey = scrollDirection !== 0;
    const isPaginationKey = paginationLink !== null;

    if (
        event.defaultPrevented
        || (
            !isScrollNavigationKey
            && !isPaginationKey
            && !isFollowShortcut
            && !isExpandPreviewShortcut
            && !isOpenPostContextShortcut
            && !isOpenPostOnRedditShortcut
            && !isFilterPostAuthorShortcut
            && !isTemporaryFilterPostAuthorShortcut
            && !isLikePostFeedbackShortcut
            && !isDislikePostFeedbackShortcut
        )
        || event.altKey
        || event.ctrlKey
        || event.metaKey
    ) {
        return;
    }

    if (isInteractiveElement(event.target) || isInteractiveElement(document.activeElement)) {
        return;
    }

    if (isTemporaryFilterPostAuthorShortcut) {
        if (temporarilyFilterCurrentPostAuthor()) {
            event.preventDefault();
        }
        return;
    }

    if (isLikePostFeedbackShortcut) {
        if (submitCurrentPostFeedback("like")) {
            event.preventDefault();
        }
        return;
    }

    if (isDislikePostFeedbackShortcut) {
        if (submitCurrentPostFeedback("dislike")) {
            event.preventDefault();
        }
        return;
    }

    if (isFollowShortcut) {
        if ((!isUserPage() && followCurrentPostAuthor()) || window.togglePrimarySubscription?.() === true) {
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

    if (isFilterPostAuthorShortcut) {
        if (filterCurrentPostAuthor()) {
            event.preventDefault();
        }
        return;
    }

    if (isOpenPostContextShortcut) {
        if (openCurrentPostContext()) {
            event.preventDefault();
        }
        return;
    }

    if (isOpenPostOnRedditShortcut) {
        if (openCurrentPostOnReddit()) {
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
        followPaginationLinkForDirection(scrollDirection, paginationLinkForDirection);
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
            followPaginationLinkForDirection(scrollDirection, paginationLinkForDirection);
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
window.applyTemporaryAuthorFilters = applyTemporaryAuthorFilters;
window.initializePostAuthorActions = initializePostAuthorActions;
document.addEventListener("click", handlePostAuthorActionClick);
document.addEventListener("DOMContentLoaded", () => applyTemporaryAuthorFilters(document));
document.addEventListener("DOMContentLoaded", () => initializePostAuthorActions(document));
document.addEventListener("DOMContentLoaded", resizePostMediaToViewportOnLoad);
document.addEventListener("DOMContentLoaded", restoreBoundaryPaginationPosition);
window.addEventListener("orientationchange", schedulePostMediaResizeAfterIpadOrientationChange);
window.addEventListener("storage", (event) => {
    if (event.key === TEMPORARY_AUTHOR_FILTER_STORAGE_KEY) {
        applyTemporaryAuthorFilters(document);
    }
});
document.addEventListener("load", handlePostMediaLoaded, true);
document.addEventListener("loadedmetadata", handlePostMediaLoaded, true);
