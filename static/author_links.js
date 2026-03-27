(() => {
    const renderAverageVideoLength = (container, avgVideoLength, avgVideoLengthTitle) => {
        const existing = container.querySelector(".author_avg_video_length");
        if (!avgVideoLength) {
            if (existing) {
                existing.remove();
            }
            return;
        }

        const label = existing || document.createElement("span");
        label.className = "author_avg_video_length";
        label.textContent = `avg ${avgVideoLength}`;
        label.title = avgVideoLengthTitle || "";
        if (!existing) {
            container.appendChild(label);
        }
    };

    const loadAuthorLinks = (root = document) => {
        const containers = Array.from(root.querySelectorAll?.(".external_site_icons[data-author-name]") || []).filter(
            (container) => container.dataset.state !== "loaded" && container.dataset.state !== "loading"
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

                    renderAverageVideoLength(container, authorData.avg_video_length, authorData.avg_video_length_title);
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
