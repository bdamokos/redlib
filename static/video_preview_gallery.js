// @license http://www.gnu.org/licenses/agpl-3.0.html AGPL-3.0
(function () {
    if (!("IntersectionObserver" in window) || !window.fetch || !window.crypto || typeof window.crypto.randomUUID !== "function") {
        return;
    }

    var pageSessionId = window.crypto.randomUUID();
    var activeHosts = new Map();
    var heartbeatTimer = null;
    var hostObserver;

    function jsonHeaders() {
        return {
            "Content-Type": "application/json"
        };
    }

    function postJson(url, payload, useBeacon) {
        var body = JSON.stringify(payload);
        if (useBeacon && navigator.sendBeacon) {
            return navigator.sendBeacon(url, new Blob([body], { type: "application/json" }));
        }

        return fetch(url, {
            method: "POST",
            headers: jsonHeaders(),
            body: body,
            keepalive: true
        });
    }

    function ensureHeartbeat() {
        if (heartbeatTimer || activeHosts.size === 0 || document.visibilityState === "hidden") {
            return;
        }

        heartbeatTimer = window.setInterval(function () {
            if (activeHosts.size === 0) {
                clearHeartbeat();
                return;
            }

            postJson("/api/video-previews/heartbeat", {
                page_session_id: pageSessionId,
                job_ids: Array.from(activeHosts.values()).map(function (state) {
                    return state.jobId;
                }).filter(Boolean)
            });
        }, 10000);
    }

    function clearHeartbeat() {
        if (heartbeatTimer) {
            window.clearInterval(heartbeatTimer);
            heartbeatTimer = null;
        }
    }

    function renderGallery(host, frames) {
        var gallery = host.querySelector(".video_preview_gallery");
        if (!gallery) {
            return;
        }

        gallery.innerHTML = "";

        frames.forEach(function (frame) {
            var anchor = document.createElement("a");
            anchor.href = frame.image_path;
            anchor.className = "video_preview_frame_link";

            var image = document.createElement("img");
            image.loading = "lazy";
            image.decoding = "async";
            image.alt = "Video preview frame";
            image.src = frame.image_path;
            if (frame.width > 0) {
                image.width = frame.width;
            }
            if (frame.height > 0) {
                image.height = frame.height;
            }

            var label = document.createElement("span");
            label.className = "video_preview_frame_time";
            label.textContent = formatTimestamp(frame.timestamp_seconds);

            anchor.appendChild(image);
            anchor.appendChild(label);
            gallery.appendChild(anchor);
        });

        gallery.hidden = frames.length === 0;
    }

    function formatTimestamp(seconds) {
        var total = Math.max(0, parseInt(seconds, 10) || 0);
        var hours = Math.floor(total / 3600);
        var minutes = Math.floor((total % 3600) / 60);
        var remainder = total % 60;

        if (hours > 0) {
            return hours + ":" + String(minutes).padStart(2, "0") + ":" + String(remainder).padStart(2, "0");
        }

        return minutes + ":" + String(remainder).padStart(2, "0");
    }

    function pollStatus(host, state) {
        if (!state.jobId || document.visibilityState === "hidden") {
            return;
        }

        fetch("/api/video-previews/" + encodeURIComponent(state.jobId), {
            headers: {
                "Accept": "application/json"
            }
        })
            .then(function (response) {
                if (!response.ok) {
                    throw new Error("status fetch failed");
                }
                return response.json();
            })
            .then(function (payload) {
                var current = activeHosts.get(host);
                if (!current || current.jobId !== state.jobId) {
                    return;
                }

                if (payload.state && payload.state.status === "ready") {
                    current.ready = true;
                    renderGallery(host, payload.frames || []);
                    if (hostObserver) {
                        hostObserver.unobserve(host);
                    }
                    return;
                }

                if (payload.state && payload.state.status === "failed") {
                    current.failed = true;
                    return;
                }

                var delay = payload.poll_after_ms || 1500;
                current.pollTimer = window.setTimeout(function () {
                    pollStatus(host, current);
                }, delay);
            })
            .catch(function () {
                var current = activeHosts.get(host);
                if (!current) {
                    return;
                }

                current.pollTimer = window.setTimeout(function () {
                    pollStatus(host, current);
                }, 5000);
            });
    }

    function leaseHost(host) {
        if (document.visibilityState === "hidden") {
            return;
        }

        var state = activeHosts.get(host);
        if (!state) {
            state = {};
            activeHosts.set(host, state);
        }

        if (state.ready || state.leasing) {
            return;
        }

        state.leasing = true;

        fetch("/api/video-previews/lease", {
            method: "POST",
            headers: jsonHeaders(),
            body: JSON.stringify({
                page_session_id: pageSessionId,
                page_path: host.dataset.pagePath || window.location.pathname,
                post_id: host.dataset.postId || "",
                media_url: host.dataset.mediaUrl || "",
                poster_url: host.dataset.posterUrl || "",
                duration_seconds: Number(host.dataset.durationSeconds || 0),
                width: Number(host.dataset.width || 0),
                height: Number(host.dataset.height || 0)
            })
        })
            .then(function (response) {
                if (!response.ok) {
                    throw new Error("lease failed");
                }
                return response.json();
            })
            .then(function (payload) {
                var current = activeHosts.get(host);
                if (!current) {
                    return;
                }

                current.leasing = false;
                current.jobId = payload.job_id;
                ensureHeartbeat();

                if (payload.state && payload.state.status === "ready") {
                    current.ready = true;
                    renderGallery(host, payload.frames || []);
                    if (hostObserver) {
                        hostObserver.unobserve(host);
                    }
                    return;
                }

                pollStatus(host, current);
            })
            .catch(function () {
                var current = activeHosts.get(host);
                if (current) {
                    current.leasing = false;
                }
            });
    }

    function releaseAll(useBeacon) {
        if (activeHosts.size === 0) {
            return;
        }

        var jobIds = Array.from(activeHosts.values()).map(function (state) {
            return state.jobId;
        }).filter(Boolean);

        Array.from(activeHosts.values()).forEach(function (state) {
            if (state.pollTimer) {
                window.clearTimeout(state.pollTimer);
                state.pollTimer = null;
            }
        });

        postJson("/api/video-previews/release", {
            page_session_id: pageSessionId,
            job_ids: jobIds
        }, useBeacon);

        clearHeartbeat();
    }

    function observeHosts(root) {
        if (!hostObserver) {
            hostObserver = new IntersectionObserver(function (entries) {
                entries.forEach(function (entry) {
                    if (entry.isIntersecting) {
                        leaseHost(entry.target);
                    }
                });
            }, {
                rootMargin: "400px 0px",
                threshold: 0
            });
        }

        (root || document).querySelectorAll(".video_preview_host[data-media-url]").forEach(function (host) {
            if (!activeHosts.has(host)) {
                activeHosts.set(host, {});
            }
            hostObserver.observe(host);
        });
    }

    document.addEventListener("visibilitychange", function () {
        if (document.visibilityState === "hidden") {
            releaseAll(true);
            return;
        }

        observeHosts(document);
        ensureHeartbeat();
    });

    window.addEventListener("pagehide", function () {
        releaseAll(true);
    });

    window.observeVideoPreviewHosts = observeHosts;
    observeHosts(document);
})();
// @license-end
