(() => {
    const RG_PROFILE_CLASSES = ["post_rg_profiles_2", "post_rg_profiles_3plus"];
    const AVG_RETRY_DELAYS_MS = [2000, 6000, 15000, 30000];
    let avgRetryTimer = null;

    const rgProfileCountFromTitle = (title) => {
        const match = String(title || "").match(/(?:^|\s)(\d+)\s+rg profiles(?:\s|$)/);
        return match ? Number.parseInt(match[1], 10) || 0 : 0;
    };

    const updatePostRgProfileClass = (container, rgProfileCount) => {
        const post = container.closest(".post");
        if (!post) {
            return;
        }

        post.classList.remove(...RG_PROFILE_CLASSES);
        delete post.dataset.rgProfileCount;

        if (rgProfileCount === 2) {
            post.classList.add("post_rg_profiles_2");
            post.dataset.rgProfileCount = "2";
        } else if (rgProfileCount >= 3) {
            post.classList.add("post_rg_profiles_3plus");
            post.dataset.rgProfileCount = String(rgProfileCount);
        }
    };

    const renderAverageVideoLength = (container, avgVideoLength, avgVideoLengthTitle, avgVideoLengthLabel, avgVideoLengthRgProfiles) => {
        const existing = container.querySelector(".author_avg_video_length");
        const rgProfileCount = Number.parseInt(avgVideoLengthRgProfiles, 10) || rgProfileCountFromTitle(avgVideoLengthTitle);
        if (!avgVideoLength) {
            if (existing) {
                existing.remove();
            }
            updatePostRgProfileClass(container, 0);
            return;
        }

        const label = existing || document.createElement("span");
        label.className = "author_avg_video_length";
        label.textContent = avgVideoLengthLabel || `avg ${avgVideoLength}`;
        label.title = avgVideoLengthTitle || "";
        label.dataset.rgProfileCount = rgProfileCount > 1 ? String(rgProfileCount) : "";
        updatePostRgProfileClass(container, rgProfileCount);
        if (!existing) {
            container.appendChild(label);
        }
    };

    const scheduleAverageRetry = (delayMs) => {
        if (avgRetryTimer) {
            return;
        }

        avgRetryTimer = window.setTimeout(() => {
            avgRetryTimer = null;
            loadAuthorLinks(document);
        }, delayMs);
    };

    const updateAverageRetryState = (container, hasAverageVideoLength) => {
        if (hasAverageVideoLength) {
            delete container.dataset.avgRefresh;
            delete container.dataset.avgRetryCount;
            return;
        }

        const retryCount = Number.parseInt(container.dataset.avgRetryCount, 10) || 0;
        if (retryCount >= AVG_RETRY_DELAYS_MS.length) {
            delete container.dataset.avgRefresh;
            delete container.dataset.avgRetryCount;
            return;
        }

        container.dataset.avgRefresh = "pending";
        container.dataset.avgRetryCount = String(retryCount + 1);
        scheduleAverageRetry(AVG_RETRY_DELAYS_MS[retryCount]);
    };

    const loadAuthorLinks = (root = document) => {
        const containers = Array.from(root.querySelectorAll?.(".external_site_icons[data-author-name]") || []).filter(
            (container) => container.dataset.state !== "loading" && (container.dataset.state !== "loaded" || container.dataset.avgRefresh === "pending")
        );
        if (containers.length === 0) {
            return;
        }

        const authors = [...new Set(
            containers
                .map((container) => container.getAttribute("data-author-name") || "")
                .map((author) => author.trim())
                .filter((author) => author !== "" && author !== "[deleted]")
        )];

        if (authors.length === 0) {
            return;
        }

        for (const container of containers) {
            container.dataset.state = "loading";
        }

        fetch(`/author_links?names=${encodeURIComponent(authors.join(","))}`, {
            credentials: "same-origin",
        })
            .then((response) => {
                if (!response.ok) {
                    throw new Error(`author_links request failed: ${response.status}`);
                }
                return response.json();
            })
            .then((linksByAuthor) => {
                for (const container of containers) {
                    const author = (container.getAttribute("data-author-name") || "").trim();
                    const authorData = linksByAuthor[author] || {};
                    const links = Array.isArray(authorData.links) ? authorData.links : [];
                    container.replaceChildren();

                    for (const link of links) {
                        const anchor = document.createElement("a");
                        anchor.className = "external_site_link";
                        if (link.has_coomer) {
                            anchor.classList.add("external_site_link--coomer");
                        }
                        anchor.href = link.url;
                        anchor.rel = "nofollow noopener noreferrer";
                        anchor.title = link.label;

                        const image = document.createElement("img");
                        image.loading = "lazy";
                        image.src = link.icon_path;
                        image.alt = link.label;
                        image.width = 16;
                        image.height = 16;

                        anchor.appendChild(image);
                        container.appendChild(anchor);
                    }

                    renderAverageVideoLength(
                        container,
                        authorData.avg_video_length,
                        authorData.avg_video_length_title,
                        authorData.avg_video_length_label,
                        authorData.avg_video_length_rg_profiles
                    );
                    updateAverageRetryState(container, Boolean(authorData.avg_video_length));
                    container.dataset.state = "loaded";
                }
            })
            .catch(() => {
                for (const container of containers) {
                    container.dataset.state = "error";
                }
            });
    };

    window.loadAuthorLinks = loadAuthorLinks;
    document.addEventListener("DOMContentLoaded", () => loadAuthorLinks(document));
})();
