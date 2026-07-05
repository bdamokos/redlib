(() => {
    const numberFrom = (value, fallback = 0) => {
        const parsed = Number.parseInt(value, 10);
        return Number.isFinite(parsed) ? parsed : fallback;
    };

    const makeElement = (tag, className, text) => {
        const element = document.createElement(tag);
        if (className) {
            element.className = className;
        }
        if (text !== undefined) {
            element.textContent = text;
        }
        return element;
    };

    const appendIcon = (container, link) => {
        const anchor = makeElement("a", `creator_link_icon${link.is_archive ? " creator_link_icon_archive" : ""}`);
        anchor.href = link.url || "#";
        anchor.rel = "nofollow noopener noreferrer";
        anchor.title = link.label || link.url || "Creator link";

        const image = document.createElement("img");
        image.loading = "lazy";
        image.src = link.icon_path || "/favicon.ico";
        image.alt = link.label || "Creator link";
        image.width = 16;
        image.height = 16;

        anchor.appendChild(image);
        container.appendChild(anchor);
    };

    const renderRow = (grid, row) => {
        const name = String(row.name || "").trim();
        const links = Array.isArray(row.links) ? row.links : [];
        if (!name || links.length === 0) {
            return false;
        }

        const card = makeElement("article", "creator_link_card");
        const profilePath = `/user/${encodeURIComponent(name)}`;
        const profile = makeElement("a", "creator_link_name", `u/${name}`);
        profile.href = profilePath;

        const icons = makeElement("div", "creator_link_icons");
        icons.setAttribute("aria-label", `Links for u/${name}`);

        const redlib = makeElement("a", "creator_link_icon creator_link_icon_redlib");
        redlib.href = profilePath;
        redlib.title = `Redlib profile for u/${name}`;

        const redlibImage = document.createElement("img");
        redlibImage.loading = "lazy";
        redlibImage.src = "/favicon.ico";
        redlibImage.alt = `Redlib profile for u/${name}`;
        redlibImage.width = 16;
        redlibImage.height = 16;
        redlib.appendChild(redlibImage);
        icons.appendChild(redlib);

        for (const link of links) {
            appendIcon(icons, link || {});
        }

        card.append(profile, icons);
        grid.appendChild(card);
        return true;
    };

    const page = document.querySelector(".creator_links_page[data-creator-links-url]");
    if (!page) {
        return;
    }

    const followedCount = numberFrom(page.dataset.creatorLinksFollowedCount);
    if (followedCount === 0) {
        return;
    }

    const endpoint = page.dataset.creatorLinksUrl || "/creator-links.json";
    const batchSize = Math.max(1, numberFrom(page.dataset.creatorLinksBatchSize, 8));
    const grid = page.querySelector("[data-creator-links-grid]");
    const loading = page.querySelector("[data-creator-links-loading]");
    const empty = page.querySelector("[data-creator-links-empty]");
    const note = page.querySelector("[data-creator-links-note]");
    const status = page.querySelector("[data-creator-links-status]");
    const progress = page.querySelector("[data-creator-links-progress]");
    const stats = {
        rows: page.querySelector('[data-creator-links-stat="rows"]'),
        links: page.querySelector('[data-creator-links-stat="links"]'),
        archives: page.querySelector('[data-creator-links-stat="archives"]'),
    };

    let offset = 0;
    let rowCount = 0;
    let linkCount = 0;
    let archiveCount = 0;
    let retryButton = null;

    const setStatus = (text) => {
        if (status) {
            status.textContent = text;
        }
    };

    const updateStats = () => {
        if (stats.rows) {
            stats.rows.textContent = String(rowCount);
        }
        if (stats.links) {
            stats.links.textContent = String(linkCount);
        }
        if (stats.archives) {
            stats.archives.textContent = String(archiveCount);
        }
        if (progress) {
            progress.style.width = `${Math.min(100, Math.round((offset / followedCount) * 100))}%`;
        }
    };

    const showRetry = () => {
        if (!loading || retryButton) {
            return;
        }
        retryButton = makeElement("button", "creator_links_retry", "Retry");
        retryButton.type = "button";
        retryButton.addEventListener("click", () => {
            retryButton.remove();
            retryButton = null;
            loadNextBatch();
        });
        loading.appendChild(retryButton);
    };

    const finish = () => {
        if (loading) {
            loading.hidden = true;
        }
        if (rowCount === 0 && empty) {
            empty.hidden = false;
        }
        if (note && rowCount > 0) {
            const skipped = Math.max(0, followedCount - rowCount);
            note.textContent = `${skipped} followed profiles had no creator URL metadata for this overview.`;
            note.hidden = false;
        }
        updateStats();
    };

    async function loadNextBatch() {
        setStatus(`Checked ${offset} of ${followedCount} profiles...`);
        const url = new URL(endpoint, window.location.origin);
        url.searchParams.set("offset", String(offset));
        url.searchParams.set("limit", String(batchSize));

        let payload;
        try {
            const response = await fetch(url, { credentials: "same-origin" });
            if (!response.ok) {
                throw new Error(`creator links batch failed: ${response.status}`);
            }
            payload = await response.json();
        } catch (_error) {
            setStatus(`Could not load profiles after ${offset} of ${followedCount}.`);
            showRetry();
            return;
        }

        const rows = Array.isArray(payload.rows) ? payload.rows : [];
        for (const row of rows) {
            const links = Array.isArray(row.links) ? row.links : [];
            if (renderRow(grid, row)) {
                rowCount += 1;
                linkCount += links.length;
                archiveCount += links.filter((link) => link && link.is_archive).length;
            }
        }

        const processedCount = numberFrom(payload.processed_count);
        const payloadOffset = numberFrom(payload.offset, offset);
        offset = payload.next_offset === null || payload.next_offset === undefined
            ? Math.min(followedCount, payloadOffset + processedCount)
            : Math.min(followedCount, numberFrom(payload.next_offset, offset + processedCount));

        updateStats();
        if (offset >= followedCount || payload.next_offset === null || payload.next_offset === undefined) {
            finish();
            return;
        }

        setStatus(`Checked ${offset} of ${followedCount} profiles...`);
        window.setTimeout(loadNextBatch, 0);
    }

    updateStats();
    loadNextBatch();
})();
