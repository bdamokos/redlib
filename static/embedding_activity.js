(() => {
    const rootSelector = "[data-embedding-activity]";
    const gridSelector = "[data-embedding-vector-grid]";
    const averageGridSelector = "[data-embedding-average-grid]";
    const averagePanelSelector = "[data-embedding-average-panel]";
    const stateSelector = "[data-embedding-activity-state]";
    const embeddedSelector = "[data-embedding-activity-embedded]";
    const remainingSelector = "[data-embedding-activity-remaining]";
    const queuedSelector = "[data-embedding-activity-queued]";
    const metaSelector = "[data-embedding-activity-meta]";
    const averageMetaSelector = "[data-embedding-average-meta]";
    const averageStateSelector = "[data-embedding-average-state]";
    const gridSize = 30;
    const activePollMs = 500;
    const idlePollMs = 2500;
    const requestTimeoutMs = 12000;
    const placeholderModes = new Set(["queued", "active", "errored"]);

    const stateLabels = {
        active: "active",
        queued: "queued",
        finished: "settled",
        errored: "error",
        idle: "idle",
    };

    const fetchWithTimeout = async (url) => {
        const controller = new AbortController();
        const timeoutId = window.setTimeout(() => controller.abort(), requestTimeoutMs);
        try {
            return await fetch(url, {
                credentials: "same-origin",
                headers: {
                    "Cache-Control": "no-store",
                    "Accept": "application/json",
                },
                signal: controller.signal,
            });
        } finally {
            window.clearTimeout(timeoutId);
        }
    };

    const shortNumber = (value) => {
        if (!Number.isFinite(value)) {
            return "";
        }
        const abs = Math.abs(value);
        if (abs >= 10) {
            return value.toFixed(1);
        }
        if (abs >= 1) {
            return value.toFixed(2);
        }
        return value.toFixed(3);
    };

    const ensureGridCells = (grid) => {
        if (!grid || grid.children.length === gridSize) {
            return;
        }
        grid.replaceChildren();
        for (let index = 0; index < gridSize; index += 1) {
            const cell = document.createElement("span");
            cell.className = "embedding_vector_cell";
            cell.setAttribute("aria-label", `vector value ${index + 1}`);
            cell.style.setProperty("--embedding-cell-index", index);
            grid.appendChild(cell);
        }
    };

    const setText = (root, selector, text) => {
        const node = root.querySelector(selector);
        if (node) {
            node.textContent = text;
        }
    };

    const setContainerState = (container, status) => {
        const state = status?.state || "idle";
        container.dataset.embeddingState = state;
        container.classList.toggle("is-active", state === "active" || state === "queued");
        container.classList.toggle("is-settled", state === "finished" || state === "idle");
        container.classList.toggle("is-error", state === "errored");
        setText(container, stateSelector, stateLabels[state] || state);
    };

    const setGridMode = (grid, mode) => {
        if (grid) {
            grid.dataset.embeddingGridMode = mode;
        }
    };

    const maxAbsValue = (values) => values.reduce((max, value) => {
        const abs = Math.abs(Number(value));
        return Number.isFinite(abs) ? Math.max(max, abs) : max;
    }, 0);

    const flipCell = (cell) => {
        cell.classList.remove("is-flipping");
        void cell.offsetWidth;
        cell.classList.add("is-flipping");
    };

    const renderValues = (grid, values, changeKey) => {
        ensureGridCells(grid);
        if (!grid) {
            return;
        }
        setGridMode(grid, values.length > 0 ? "values" : "empty");

        const maxAbs = maxAbsValue(values);
        const cells = Array.from(grid.children);
        for (let index = 0; index < cells.length; index += 1) {
            const cell = cells[index];
            const value = Number(values[index]);
            const hasValue = Number.isFinite(value);
            const nextText = hasValue ? shortNumber(value) : "";
            const previousText = cell.textContent;
            const intensity = hasValue && maxAbs > 0 ? Math.min(Math.abs(value) / maxAbs, 1) : 0;
            const hue = value >= 0 ? 156 : 350;
            const lightness = `${(16 + intensity * 26).toFixed(1)}%`;
            const alpha = (0.18 + intensity * 0.62).toFixed(3);
            const borderAlpha = (0.18 + intensity * 0.45).toFixed(3);
            const glow = `${(intensity * 10).toFixed(1)}px`;

            cell.textContent = nextText;
            cell.style.setProperty("--embedding-intensity", intensity.toFixed(3));
            cell.style.setProperty("--embedding-hue", String(hue));
            cell.style.setProperty("--embedding-lightness", lightness);
            cell.style.setProperty("--embedding-alpha", alpha);
            cell.style.setProperty("--embedding-border-alpha", borderAlpha);
            cell.style.setProperty("--embedding-glow", glow);
            cell.classList.toggle("is-positive", hasValue && value >= 0);
            cell.classList.toggle("is-negative", hasValue && value < 0);
            cell.classList.toggle("is-empty", !hasValue);
            cell.classList.remove("is-placeholder", "is-placeholder-error");

            if (changeKey && nextText !== previousText) {
                flipCell(cell);
            }
        }
    };

    const renderPlaceholder = (grid, mode) => {
        ensureGridCells(grid);
        if (!grid) {
            return;
        }
        setGridMode(grid, mode);
        const cells = Array.from(grid.children);
        for (let index = 0; index < cells.length; index += 1) {
            const cell = cells[index];
            cell.textContent = "";
            cell.removeAttribute("style");
            cell.style.setProperty("--embedding-cell-index", index);
            cell.classList.remove("is-positive", "is-negative", "is-empty", "is-flipping");
            cell.classList.add("is-placeholder");
            cell.classList.toggle("is-placeholder-error", mode === "errored");
        }
    };

    const noVectorMode = (status) => {
        const state = status?.state || "idle";
        if (placeholderModes.has(state)) {
            return state;
        }
        if ((status?.queued_candidates || 0) > 0) {
            return "queued";
        }
        if ((status?.active_candidates || 0) > 0 || (status?.active_remaining || 0) > 0) {
            return "active";
        }
        return "empty";
    };

    const noVectorMessage = (status) => {
        if (!status.enabled) {
            return "Embedding stream off";
        }
        if (status.state === "errored" || status.failed > 0) {
            return "Embedding stream failed before a vector arrived";
        }
        if (status.timed_out) {
            return "Embedding stream paused before a vector arrived";
        }
        if (status.state === "queued" || (status.queued_candidates || 0) > 0) {
            return "Queued posts are waiting for vectors";
        }
        if (status.state === "active" || (status.active_remaining || 0) > 0) {
            return "Embedding vectors are being generated";
        }
        return "Waiting for vectors";
    };

    const updateMeta = (container, status) => {
        const embedding = status.latest_embedding;
        if (!embedding) {
            setText(container, metaSelector, noVectorMessage(status));
            return;
        }
        const shown = embedding.displayed_dimensions || embedding.values?.length || 0;
        const dimensions = embedding.dimensions || shown;
        setText(container, metaSelector, `${shown} of ${dimensions} values from the latest vector`);
    };

    const updateAverageMeta = (container, status) => {
        const panel = container.querySelector(averagePanelSelector);
        const average = status.aggregate_embedding;
        if (!average) {
            if (panel) {
                panel.dataset.embeddingAverageReady = "false";
            }
            setText(container, averageStateSelector, "waiting");
            setText(
                container,
                averageMetaSelector,
                status.state === "errored" || status.failed > 0
                    ? "Average unavailable until a vector lands"
                    : status.enabled
                        ? "Waiting for average"
                        : "Embedding stream off"
            );
            return;
        }
        if (panel) {
            panel.dataset.embeddingAverageReady = "true";
        }
        const shown = average.displayed_dimensions || average.values?.length || 0;
        const dimensions = average.dimensions || shown;
        const postCount = average.post_count || 0;
        setText(container, averageStateSelector, `${postCount} posts`);
        setText(container, averageMetaSelector, `${shown} of ${dimensions} averaged values after ${postCount} posts`);
    };

    const shouldShow = (status) =>
        Boolean(
            status?.latest_embedding ||
                status?.aggregate_embedding ||
                status?.queued_candidates ||
                status?.active_candidates ||
                status?.active_remaining ||
                status?.state === "errored" ||
                status?.failed > 0 ||
                status?.timed_out
        );

    const shouldContinuePolling = (status) =>
        Boolean(status?.enabled && (status.state === "active" || status.state === "queued" || status.queued_candidates > 0 || status.active_remaining > 0));

    const renderStatus = (container, status) => {
        const previousEmbedded = Number.parseInt(container.dataset.embeddingLastEmbedded || "0", 10);
        const currentEmbedded = Number(status.embedded || 0);
        container.dataset.embeddingLastEmbedded = String(currentEmbedded);
        container.dataset.embeddingReady = shouldShow(status) ? "true" : "false";
        setContainerState(container, status);
        setText(container, embeddedSelector, String(status.embedded || 0));
        setText(container, remainingSelector, String(status.active_remaining || 0));
        setText(container, queuedSelector, String(status.queued_candidates || 0));
        updateMeta(container, status);
        updateAverageMeta(container, status);
        if (currentEmbedded > previousEmbedded) {
            document.dispatchEvent(new CustomEvent("redlib:content-progress", {
                detail: { embedded: currentEmbedded },
            }));
        }

        const embedding = status.latest_embedding;
        const latestGrid = container.querySelector(gridSelector);
        if (!embedding || !Array.isArray(embedding.values)) {
            renderPlaceholder(latestGrid, noVectorMode(status));
        } else {
            const changeKey = `${status.activity_seq || 0}:${embedding.post_key_hash || ""}:${embedding.embedded_at_unix || 0}`;
            const hasChanged = container.dataset.embeddingLastKey !== changeKey;
            container.dataset.embeddingLastKey = changeKey;
            renderValues(latestGrid, embedding.values, hasChanged ? changeKey : "");
        }

        const average = status.aggregate_embedding;
        const averageGrid = container.querySelector(averageGridSelector);
        if (!average || !Array.isArray(average.values)) {
            renderPlaceholder(averageGrid, noVectorMode(status));
            return;
        }
        const averageChangeKey = `${status.activity_seq || 0}:${average.post_count || 0}:${average.embedded_at_unix || 0}`;
        const averageHasChanged = container.dataset.embeddingAverageLastKey !== averageChangeKey;
        container.dataset.embeddingAverageLastKey = averageChangeKey;
        renderValues(averageGrid, average.values, averageHasChanged ? averageChangeKey : "");
    };

    const initialize = (root = document) => {
        const containers = Array.from(root.querySelectorAll?.(rootSelector) || []).filter(
            (container) => container.dataset.embeddingActivityBound !== "true"
        );

        for (const container of containers) {
            const url = container.dataset.embeddingActivityUrl;
            if (!url) {
                continue;
            }

            container.dataset.embeddingActivityBound = "true";
            const grid = container.querySelector(gridSelector);
            ensureGridCells(grid);
            const averageGrid = container.querySelector(averageGridSelector);
            ensureGridCells(averageGrid);

            let timerId = null;

            const schedule = (delay) => {
                if (timerId) {
                    window.clearTimeout(timerId);
                }
                timerId = window.setTimeout(tick, delay);
            };

            const tick = async () => {
                const response = await fetchWithTimeout(url).catch(() => null);
                if (!response || !response.ok) {
                    container.dataset.embeddingReady = "true";
                    setContainerState(container, { state: "errored" });
                    setText(container, stateSelector, "error");
                    setText(container, metaSelector, "Embedding status unavailable");
                    setText(container, averageStateSelector, "waiting");
                    setText(container, averageMetaSelector, "Average unavailable until the stream recovers");
                    renderPlaceholder(container.querySelector(gridSelector), "errored");
                    renderPlaceholder(container.querySelector(averageGridSelector), "errored");
                    schedule(idlePollMs);
                    return;
                }

                const status = await response.json().catch(() => null);
                if (!status) {
                    schedule(idlePollMs);
                    return;
                }

                renderStatus(container, status);
                if (shouldContinuePolling(status)) {
                    schedule(activePollMs);
                } else if (status.enabled && !status.latest_embedding) {
                    schedule(idlePollMs);
                } else {
                    timerId = null;
                }
            };

            void tick();
        }
    };

    window.initializeEmbeddingActivity = initialize;
    document.addEventListener("DOMContentLoaded", () => initialize(document));
})();
