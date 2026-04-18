// @license http://www.gnu.org/licenses/agpl-3.0.html AGPL-3.0
(function () {
    if (!window.fetch || !window.crypto || typeof window.crypto.randomUUID !== "function") {
        return;
    }

    var pageSessionId = window.crypto.randomUUID();
    var activeHosts = new Map();
    var heartbeatTimer = null;
    var pendingRenderTimer = null;

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

    function previewVideo(host) {
        return host.querySelector("video.post_media_video");
    }

    function previewLayer(host) {
        return host.querySelector(".video_preview_layer");
    }

    function previewImage(host) {
        var layer = previewLayer(host);
        return layer ? layer.querySelector(".video_preview_image") : null;
    }

    function hideAnimatedPreview(host) {
        var layer = previewLayer(host);
        if (!layer) {
            return;
        }

        layer.classList.remove("is-visible");
    }

    function syncAnimatedPreviewVisibility(host) {
        var layer = previewLayer(host);
        var video = previewVideo(host);
        if (!layer || !video) {
            return;
        }

        var shouldShow = Boolean(layer.dataset.previewSrc) && video.paused && video.currentTime <= 0.05;
        layer.classList.toggle("is-visible", shouldShow);
    }

    function stopPreviewGenerationForPlayback(host) {
        var state = activeHosts.get(host);
        if (!state) {
            return;
        }

        state.suppressPreviewLease = true;
        if (state.pollTimer) {
            window.clearTimeout(state.pollTimer);
            state.pollTimer = null;
        }

        if (state.jobId) {
            postJson("/api/video-previews/release", {
                page_session_id: pageSessionId,
                job_ids: [state.jobId]
            });
        }

        setLoadingState(host, "ready");
        hideAnimatedPreview(host);
    }

    function bindPreviewLifecycle(host) {
        if (host.dataset.previewLifecycleBound === "true") {
            return;
        }

        host.dataset.previewLifecycleBound = "true";
        var video = previewVideo(host);
        if (!video) {
            return;
        }

        function onPlaybackStarted() {
            hideAnimatedPreview(host);
            stopPreviewGenerationForPlayback(host);
        }

        video.addEventListener("play", onPlaybackStarted);
        video.addEventListener("playing", onPlaybackStarted);
        ["pause", "emptied", "loadstart", "loadeddata", "loadedmetadata", "seeked", "ended"].forEach(function (eventName) {
            video.addEventListener(eventName, function () {
                syncAnimatedPreviewVisibility(host);
            });
        });

        syncAnimatedPreviewVisibility(host);
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
                job_ids: Array.from(activeHosts.values())
                    .filter(function (state) {
                        return state.jobId && !state.suppressPreviewLease;
                    })
                    .map(function (state) {
                        return state.jobId;
                    })
            });
        }, 10000);
    }

    function clearHeartbeat() {
        if (heartbeatTimer) {
            window.clearInterval(heartbeatTimer);
            heartbeatTimer = null;
        }
    }

    function shouldRenderHost(host) {
        var post = host.closest(".post");
        var target = post || host;
        var rect = target.getBoundingClientRect();
        return rect.top <= window.innerHeight;
    }

    function renderAnimatedPreview(host, state) {
        if (!state || !state.preview || !state.preview.image_path) {
            state.rendered = true;
            return;
        }

        var layer = previewLayer(host);
        var image = previewImage(host);
        var video = previewVideo(host);
        if (!layer || !image || !video) {
            state.rendered = true;
            return;
        }

        var nextPath = state.preview.image_path;
        if (layer.dataset.previewSrc === nextPath) {
            state.rendered = true;
            syncAnimatedPreviewVisibility(host);
            return;
        }

        if (state.renderingPreviewPath === nextPath) {
            return;
        }

        state.renderingPreviewPath = nextPath;

        var preload = new Image();
        preload.decoding = "async";
        preload.onload = function () {
            var current = activeHosts.get(host);
            if (!current || !current.preview || current.preview.image_path !== nextPath) {
                return;
            }

            layer.dataset.previewSrc = nextPath;
            image.src = nextPath;
            video.removeAttribute("poster");
            current.rendered = true;
            current.renderingPreviewPath = "";
            syncAnimatedPreviewVisibility(host);
        };
        preload.onerror = function () {
            var current = activeHosts.get(host);
            if (current) {
                current.renderingPreviewPath = "";
            }
        };
        preload.src = nextPath;
    }

    function flushReadyHost(host, state) {
        if (!state || !state.ready || state.rendered) {
            return;
        }

        renderAnimatedPreview(host, state);
    }

    function flushPendingRenders() {
        pendingRenderTimer = null;

        activeHosts.forEach(function (state, host) {
            if (!state || !state.ready || state.rendered) {
                return;
            }

            if (shouldRenderHost(host)) {
                flushReadyHost(host, state);
            }
        });
    }

    function schedulePendingRenderCheck() {
        if (pendingRenderTimer) {
            return;
        }

        pendingRenderTimer = window.requestAnimationFrame(flushPendingRenders);
    }

    function setLoadingState(host, state) {
        var loading = host.querySelector(".video_preview_loading");
        if (!loading) {
            return;
        }

        host.dataset.previewState = state;
        var visible = state === "loading" || state === "queued";
        loading.hidden = !visible;

        var spinner = loading.querySelector(".video_preview_spinner");
        var statusText = loading.querySelector(".video_preview_status_text");
        if (statusText) {
            statusText.textContent = state === "queued" ? "Queued for animated preview generation…" : "Generating animated preview…";
        }
        if (spinner) {
            spinner.hidden = state === "queued";
        }
    }

    function pollStatus(host, state) {
        if (!state.jobId || document.visibilityState === "hidden" || state.suppressPreviewLease) {
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
                if (!current || current.jobId !== state.jobId || current.suppressPreviewLease) {
                    return;
                }

                if (payload.state && payload.state.status === "ready") {
                    current.ready = true;
                    current.preview = payload.preview || null;
                    current.frames = payload.frames || [];
                    setLoadingState(host, "ready");
                    flushReadyHost(host, current);
                    return;
                }

                if (payload.state && payload.state.status === "failed") {
                    current.failed = true;
                    setLoadingState(host, "failed");
                    return;
                }

                if (payload.state && payload.state.status === "pending") {
                    setLoadingState(host, "queued");
                } else {
                    setLoadingState(host, "loading");
                }

                var delay = payload.poll_after_ms || 1500;
                current.pollTimer = window.setTimeout(function () {
                    pollStatus(host, current);
                }, delay);
            })
            .catch(function () {
                var current = activeHosts.get(host);
                if (!current || current.suppressPreviewLease) {
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

        if (state.ready || state.leasing || state.suppressPreviewLease) {
            return;
        }

        state.leasing = true;
        setLoadingState(host, "queued");

        fetch("/api/video-previews/lease", {
            method: "POST",
            headers: jsonHeaders(),
            body: JSON.stringify({
                page_session_id: pageSessionId,
                page_path: host.dataset.pagePath || window.location.pathname,
                page_origin: window.location.origin,
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

                if (current.suppressPreviewLease) {
                    postJson("/api/video-previews/release", {
                        page_session_id: pageSessionId,
                        job_ids: [current.jobId]
                    });
                    setLoadingState(host, "ready");
                    return;
                }

                ensureHeartbeat();

                if (payload.state && payload.state.status === "ready") {
                    current.ready = true;
                    current.preview = payload.preview || null;
                    current.frames = payload.frames || [];
                    setLoadingState(host, "ready");
                    flushReadyHost(host, current);
                    return;
                }

                if (payload.state && payload.state.status === "pending") {
                    setLoadingState(host, "queued");
                } else {
                    setLoadingState(host, "loading");
                }

                pollStatus(host, current);
            })
            .catch(function () {
                var current = activeHosts.get(host);
                if (current) {
                    current.leasing = false;
                    setLoadingState(host, "failed");
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
        (root || document).querySelectorAll(".video_preview_host[data-media-url]").forEach(function (host) {
            bindPreviewLifecycle(host);

            if (!activeHosts.has(host)) {
                activeHosts.set(host, {});
            }

            leaseHost(host);
        });
    }

    document.addEventListener("visibilitychange", function () {
        if (document.visibilityState === "hidden") {
            releaseAll(true);
            return;
        }

        observeHosts(document);
        ensureHeartbeat();
        schedulePendingRenderCheck();
    });

    window.addEventListener("pagehide", function () {
        releaseAll(true);
    });

    window.addEventListener("scroll", schedulePendingRenderCheck, { passive: true });
    window.addEventListener("resize", schedulePendingRenderCheck);

    window.observeVideoPreviewHosts = observeHosts;
    observeHosts(document);
})();
// @license-end
