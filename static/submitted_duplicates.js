const SIMILAR_DUPLICATE_WINDOW_MS = 72 * 60 * 60 * 1000;
const HASH_SIZE = 12;
const VISUAL_KEY_SIZE = 32;

function getPostKind(post) {
    const kind = post.dataset.postType || post.querySelector(".post_thumbnail span")?.textContent?.trim();
    if (kind === "image" || kind === "gallery" || kind === "gif") {
        return kind;
    }

    if (post.querySelector(".post_media_image")) {
        return "image";
    }

    if (post.querySelector(".post_media_video") || post.querySelector("iframe")) {
        return "gif";
    }

    return null;
}

function normalizeMediaUrl(url) {
    if (!url) {
        return null;
    }

    try {
        const parsed = new URL(url, window.location.origin);
        parsed.hash = "";
        parsed.search = "";
        parsed.pathname = parsed.pathname.replace(/\/+$/, "");
        return parsed.toString();
    } catch {
        return url.split(/[?#]/, 1)[0].replace(/\/+$/, "");
    }
}

function getExactMediaKey(post, kind) {
    const iframeSrc = post.querySelector(".post_media_embed iframe, iframe")?.getAttribute("src");
    if (iframeSrc) {
        const normalized = normalizeMediaUrl(iframeSrc);
        return normalized ? `${kind}:embed:${normalized}` : null;
    }

    if (kind === "gif") {
        const videoSrc =
            post.querySelector(".post_media_video source")?.getAttribute("src") ||
            post.querySelector(".post_media_video video source")?.getAttribute("src") ||
            post.querySelector(".post_media_video video")?.getAttribute("src") ||
            post.querySelector(".post_media_video")?.getAttribute("src");

        if (videoSrc) {
            const normalized = normalizeMediaUrl(videoSrc);
            if (normalized) {
                return `${kind}:video:${normalized}`;
            }
        }
    }

    return null;
}

function getThumbnailUrl(post, kind) {
    if (kind === "gif") {
        return (
            post.querySelector(".post_media_video")?.getAttribute("poster") ||
            post.querySelector(".post_thumbnail img")?.getAttribute("src") ||
            post.querySelector(".post_thumbnail image")?.getAttribute("href") ||
            null
        );
    }

    return (
        post.querySelector(".post_media_image img")?.getAttribute("src") ||
        post.querySelector(".post_media_image image")?.getAttribute("href") ||
        post.querySelector(".post_thumbnail img")?.getAttribute("src") ||
        post.querySelector(".post_thumbnail image")?.getAttribute("href") ||
        null
    );
}

function getCreatedAt(post) {
    const createdTs = Number.parseInt(post.dataset.createdTs || "", 10);
    if (Number.isFinite(createdTs) && createdTs > 0) {
        return createdTs * 1000;
    }

    const created = post.querySelector(".created")?.getAttribute("title");
    if (!created) {
        return null;
    }

    const parsed = Date.parse(created.replace(" UTC", " GMT"));
    return Number.isNaN(parsed) ? null : parsed;
}

function averageHash(image) {
    const canvas = document.createElement("canvas");
    canvas.width = HASH_SIZE;
    canvas.height = HASH_SIZE;

    const context = canvas.getContext("2d", { willReadFrequently: true });
    if (!context) {
        return null;
    }

    context.drawImage(image, 0, 0, HASH_SIZE, HASH_SIZE);
    const { data } = context.getImageData(0, 0, HASH_SIZE, HASH_SIZE);
    const luminance = [];

    for (let index = 0; index < data.length; index += 4) {
        luminance.push((data[index] * 299 + data[index + 1] * 587 + data[index + 2] * 114) / 1000);
    }

    const average = luminance.reduce((sum, value) => sum + value, 0) / luminance.length;
    return luminance.map((value) => (value >= average ? "1" : "0")).join("");
}

async function exactVisualKey(image) {
    if (!globalThis.crypto?.subtle) {
        return null;
    }

    const canvas = document.createElement("canvas");
    canvas.width = VISUAL_KEY_SIZE;
    canvas.height = VISUAL_KEY_SIZE;

    const context = canvas.getContext("2d", { willReadFrequently: true });
    if (!context) {
        return null;
    }

    context.drawImage(image, 0, 0, VISUAL_KEY_SIZE, VISUAL_KEY_SIZE);
    const { data } = context.getImageData(0, 0, VISUAL_KEY_SIZE, VISUAL_KEY_SIZE);
    const grayscale = new Uint8Array(VISUAL_KEY_SIZE * VISUAL_KEY_SIZE);

    for (let index = 0, pixel = 0; index < data.length; index += 4, pixel += 1) {
        grayscale[pixel] = Math.round((data[index] * 299 + data[index + 1] * 587 + data[index + 2] * 114) / 1000);
    }

    const digest = await crypto.subtle.digest("SHA-256", grayscale);
    const hex = [...new Uint8Array(digest)].map((value) => value.toString(16).padStart(2, "0")).join("");
    return `${image.naturalWidth}x${image.naturalHeight}:${hex}`;
}

function fingerprintThumbnail(url) {
    return new Promise((resolve) => {
        const image = new Image();
        image.decoding = "async";
        image.referrerPolicy = "no-referrer";

        image.addEventListener("load", async () => {
            try {
                resolve({
                    hash: averageHash(image),
                    visualKey: await exactVisualKey(image),
                });
            } catch {
                resolve(null);
            }
        });

        image.addEventListener("error", () => resolve(null));
        image.src = url;
    });
}

function ensureDuplicateBlock(primary) {
    let block = primary.querySelector(".post_duplicates");
    if (block) {
        return block;
    }

    block = document.createElement("div");
    block.className = "post_duplicates";

    const label = document.createElement("p");
    label.className = "post_duplicates_label";
    block.append(label);

    const list = document.createElement("ul");
    list.className = "post_duplicates_list";
    block.append(list);

    const preview = primary.querySelector(".post_body.post_preview");
    const footer = primary.querySelector(".post_footer");
    primary.insertBefore(block, preview || footer || null);

    return block;
}

function updateDuplicateLabel(block) {
    const count = block.querySelectorAll(".post_duplicates_list li").length;
    const noun = count === 1 ? "subreddit" : "subreddits";
    block.querySelector(".post_duplicates_label").textContent = `Also posted to ${count} other ${noun}`;
}

function appendDuplicate(primary, duplicate) {
    const block = ensureDuplicateBlock(primary);
    const list = block.querySelector(".post_duplicates_list");

    const titleLink = duplicate.querySelector(".post_title a:last-of-type");
    const communityLink = duplicate.querySelector(".post_subreddit");
    const created = duplicate.querySelector(".created");

    if (!titleLink || !communityLink || !created) {
        return;
    }

    const item = document.createElement("li");
    const link = document.createElement("a");
    link.href = titleLink.getAttribute("href") || "#";
    link.textContent = titleLink.textContent || "";
    item.append(link);

    const meta = document.createElement("span");
    meta.textContent = `${communityLink.textContent || ""} • ${created.textContent || ""}`;
    item.append(meta);

    list.append(item);
    updateDuplicateLabel(block);

    if (!primary.querySelector(".post_body.post_preview")) {
        const duplicatePreview = duplicate.querySelector(".post_body.post_preview");
        if (duplicatePreview) {
            primary.insertBefore(duplicatePreview.cloneNode(true), primary.querySelector(".post_footer"));
        }
    }
}

function removeDuplicatePost(post) {
    const separator = post.previousElementSibling;
    if (separator?.matches("hr.sep")) {
        separator.remove();
    }

    post.remove();
}

function clusterByTime(posts, windowMs) {
    const sorted = [...posts].sort((left, right) => (left.createdAt ?? 0) - (right.createdAt ?? 0));
    const clusters = [];
    let current = [];

    for (const post of sorted) {
        const previous = current[current.length - 1];
        const shouldSplit =
            previous &&
            previous.createdAt != null &&
            post.createdAt != null &&
            post.createdAt - previous.createdAt > windowMs;

        if (shouldSplit) {
            clusters.push(current);
            current = [];
        }

        current.push(post);
    }

    if (current.length > 0) {
        clusters.push(current);
    }

    return clusters;
}

function collapseGroup(group) {
    const ordered = [...group].sort((left, right) => left.domIndex - right.domIndex).filter((entry) => document.body.contains(entry.post));
    if (ordered.length < 2) {
        return;
    }

    const primary = ordered[0].post;
    for (const entry of ordered.slice(1)) {
        if (entry.post === primary) {
            continue;
        }

        appendDuplicate(primary, entry.post);
        removeDuplicatePost(entry.post);
    }
}

async function collapseSubmittedDuplicates() {
    const posts = [...document.querySelectorAll("#posts > .post")];
    if (posts.length === 0) {
        return;
    }

    const entries = (
        await Promise.all(
            posts.map(async (post, domIndex) => {
                const kind = getPostKind(post);
                if (!kind) {
                    return null;
                }

                const exactKey = getExactMediaKey(post, kind);
                const thumbnailUrl = getThumbnailUrl(post, kind);
                const fingerprint = thumbnailUrl ? await fingerprintThumbnail(thumbnailUrl) : null;

                return {
                    post,
                    domIndex,
                    createdAt: getCreatedAt(post),
                    exactKey:
                        exactKey ||
                        ((kind === "image" || kind === "gif") && fingerprint?.visualKey ? `${kind}:visual:${fingerprint.visualKey}` : null),
                    similarKey: fingerprint?.hash ? `${kind}:hash:${fingerprint.hash}` : null,
                };
            }),
        )
    ).filter(Boolean);

    const exactGroups = new Map();
    const similarGroups = new Map();

    for (const entry of entries) {
        if (entry.exactKey) {
            const group = exactGroups.get(entry.exactKey) || [];
            group.push(entry);
            exactGroups.set(entry.exactKey, group);
        }

        if (entry.similarKey) {
            const group = similarGroups.get(entry.similarKey) || [];
            group.push(entry);
            similarGroups.set(entry.similarKey, group);
        }
    }

    for (const group of exactGroups.values()) {
        if (group.length > 1) {
            collapseGroup(group);
        }
    }

    for (const group of similarGroups.values()) {
        if (group.length < 2) {
            continue;
        }

        for (const cluster of clusterByTime(group, SIMILAR_DUPLICATE_WINDOW_MS)) {
            if (cluster.length > 1) {
                collapseGroup(cluster);
            }
        }
    }
}

collapseSubmittedDuplicates();
