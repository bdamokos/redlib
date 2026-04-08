(() => {
    if (!window.fetch || !window.crypto || typeof window.crypto.randomUUID !== "function") {
        return;
    }

    const sessionId = window.crypto.randomUUID();
    const heartbeatIntervalMs = 10000;
    let timerId = null;

    const postJson = (url, payload, useBeacon = false) => {
        const body = JSON.stringify(payload);
        if (useBeacon && navigator.sendBeacon) {
            return navigator.sendBeacon(url, new Blob([body], { type: "application/json" }));
        }

        return fetch(url, {
            method: "POST",
            headers: {
                "Content-Type": "application/json",
            },
            body,
            keepalive: true,
            credentials: "same-origin",
        });
    };

    const heartbeat = () => {
        if (document.visibilityState !== "visible") {
            return;
        }

        void postJson("/api/browser-sessions/heartbeat", {
            session_id: sessionId,
            path: window.location.pathname + window.location.search,
            visible: true,
        });
    };

    const ensureHeartbeat = () => {
        if (timerId || document.visibilityState !== "visible") {
            return;
        }

        heartbeat();
        timerId = window.setInterval(heartbeat, heartbeatIntervalMs);
    };

    const release = (useBeacon = false) => {
        if (timerId) {
            window.clearInterval(timerId);
            timerId = null;
        }

        postJson("/api/browser-sessions/release", {
            session_id: sessionId,
        }, useBeacon);
    };

    document.addEventListener("visibilitychange", () => {
        if (document.visibilityState === "visible") {
            ensureHeartbeat();
        } else {
            release(true);
        }
    });

    window.addEventListener("pagehide", () => {
        release(true);
    });

    ensureHeartbeat();
})();
