(() => {
    const THIRTY_DAYS_SECONDS = 30 * 24 * 60 * 60;

    const formatRelativeAge = (createdSeconds, nowSeconds) => {
        if (!Number.isFinite(createdSeconds)) {
            return null;
        }

        const deltaSeconds = Math.abs(nowSeconds - createdSeconds);
        if (deltaSeconds > THIRTY_DAYS_SECONDS) {
            return null;
        }

        const isFuture = createdSeconds > nowSeconds;
        const suffix = isFuture ? " left" : " ago";

        const wholeMinutes = Math.floor(deltaSeconds / 60);
        const wholeHours = Math.floor(deltaSeconds / (60 * 60));
        const wholeDays = Math.floor(deltaSeconds / (24 * 60 * 60));

        if (wholeDays > 0) {
            return `${wholeDays}d${suffix}`;
        }

        if (wholeHours > 0) {
            return `${wholeHours}h${suffix}`;
        }

        return `${Math.max(0, wholeMinutes)}m${suffix}`;
    };

    const refreshPostTimes = (root = document) => {
        const nodes = root.querySelectorAll?.(".created[data-created-ts]") || [];
        const nowSeconds = Date.now() / 1000;

        for (const node of nodes) {
            const createdSeconds = Number.parseFloat(node.dataset.createdTs || "");
            const nextText = formatRelativeAge(createdSeconds, nowSeconds);
            if (nextText) {
                node.textContent = nextText;
            }
        }
    };

    globalThis.refreshPostTimes = refreshPostTimes;

    refreshPostTimes(document);
    window.addEventListener("load", () => refreshPostTimes(document));

    window.setInterval(() => refreshPostTimes(document), 60 * 1000);
})();

