// @license http://www.gnu.org/licenses/agpl-3.0.html AGPL-3.0
(function () {
    if (!window.fetch) {
        return;
    }

    var PAGE_SETTLED_DELAY_MS = 1200;
    var PREVIEW_FALLBACK_DELAY_MS = 4000;
    var PREVIEW_FALLBACK_POLL_MS = 500;
    var DESKTOP_PREVIEW_FALLBACK_QUERY = "(min-width: 1000px) and (hover: hover) and (pointer: fine)";
    var PREVIEW_SOUND_STORAGE_KEY = "redlib_video_preview_sound";

    function createPageSessionId() {
        if (window.crypto && typeof window.crypto.randomUUID === "function") {
            return window.crypto.randomUUID();
        }

        return "page-" + Date.now().toString(36) + "-" + Math.random().toString(36).slice(2);
    }

    var pageSessionId = createPageSessionId();
    var activeHosts = new Map();
    var heartbeatTimer = null;
    var pendingRenderTimer = null;
    var previewFallbackSweepTimer = null;
    var previewFallbackLoadingHost = null;
    var pageSettled = false;
    var pageSettledTimer = null;
    var previewSoundEnabled = false;
    var previewSoundButton = null;

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

    function previewFallbackVideo(host) {
        var layer = previewLayer(host);
        return layer ? layer.querySelector(".video_preview_fallback_video") : null;
    }

    function readPreviewSoundEnabled() {
        try {
            return window.localStorage.getItem(PREVIEW_SOUND_STORAGE_KEY) === "on";
        } catch (error) {
            return false;
        }
    }

    function writePreviewSoundEnabled(enabled) {
        try {
            window.localStorage.setItem(PREVIEW_SOUND_STORAGE_KEY, enabled ? "on" : "off");
        } catch (error) {}
    }

    function scrollPaddingTop() {
        var value = window.getComputedStyle(document.documentElement).scrollPaddingTop;
        var parsed = Number.parseFloat(value || "0");
        return Number.isFinite(parsed) ? parsed : 0;
    }

    function postForHost(host) {
        return host ? host.closest(".post") || host : null;
    }

    function visibleFallbackHosts() {
        var viewportTop = scrollPaddingTop();
        var viewportBottom = window.innerHeight;
        return Array.from(activeHosts.keys()).filter(function (host) {
            var fallback = previewFallbackVideo(host);
            if (!fallback || fallback.hidden) {
                return false;
            }

            var target = postForHost(host);
            var rect = target.getBoundingClientRect();
            return rect.bottom > viewportTop && rect.top < viewportBottom;
        });
    }

    function currentPreviewSoundHost() {
        var viewportTop = scrollPaddingTop();
        var hosts = visibleFallbackHosts();
        var containingViewportTop = hosts.filter(function (host) {
            var rect = postForHost(host).getBoundingClientRect();
            return rect.top <= viewportTop + 1 && rect.bottom > viewportTop + 1;
        }).sort(function (left, right) {
            return postForHost(right).getBoundingClientRect().top - postForHost(left).getBoundingClientRect().top;
        });

        if (containingViewportTop.length) {
            return containingViewportTop[0];
        }

        hosts.sort(function (left, right) {
            return postForHost(left).getBoundingClientRect().top - postForHost(right).getBoundingClientRect().top;
        });

        return hosts[0] || null;
    }

    function syncPreviewSoundButton() {
        if (!previewSoundButton) {
            return;
        }

        previewSoundButton.setAttribute("aria-pressed", previewSoundEnabled ? "true" : "false");
        previewSoundButton.setAttribute("title", previewSoundEnabled ? "Preview sound on" : "Preview sound off");
        previewSoundButton.setAttribute("aria-label", previewSoundEnabled ? "Turn preview sound off" : "Turn preview sound on");

        var icon = previewSoundButton.querySelector("[aria-hidden]");
        if (icon) {
            icon.textContent = previewSoundEnabled ? "🔊" : "🔇";
        }
    }

    function syncPreviewSoundMode() {
        var soundHost = previewSoundEnabled ? currentPreviewSoundHost() : null;

        Array.from(activeHosts.keys()).forEach(function (host) {
            var fallback = previewFallbackVideo(host);
            if (!fallback) {
                return;
            }

            var shouldPlaySound = host === soundHost && !fallback.hidden;
            fallback.muted = !shouldPlaySound;
            if (shouldPlaySound) {
                fallback.volume = 1;
                var playPromise = fallback.play();
                if (playPromise && typeof playPromise.catch === "function") {
                    playPromise.catch(function () {});
                }
            }
        });

        syncPreviewSoundButton();
    }

    function bindPreviewSoundToggle() {
        previewSoundButton = document.getElementById("video_preview_sound_toggle");
        previewSoundEnabled = readPreviewSoundEnabled();
        syncPreviewSoundButton();

        if (!previewSoundButton || previewSoundButton.dataset.previewSoundBound === "true") {
            return;
        }

        previewSoundButton.dataset.previewSoundBound = "true";
        previewSoundButton.addEventListener("click", function () {
            previewSoundEnabled = !previewSoundEnabled;
            writePreviewSoundEnabled(previewSoundEnabled);
            syncPreviewSoundMode();
        });
    }

    function isInitialPausedFrame(video) {
        return Boolean(video && video.paused && video.currentTime <= 0.05);
    }

    function layerHasPreviewContent(layer) {
        return Boolean(layer.dataset.previewSrc || layer.dataset.previewFallbackActive === "true");
    }

    function suppressInitialPreviewControls(host, video) {
        if (!host || !video || video.dataset.previewControlsSuppressed === "true") {
            return;
        }

        video.dataset.previewControlsSuppressed = "true";
        video.dataset.previewHadControls = video.hasAttribute("controls") ? "true" : "false";
        if (video.hasAttribute("controls")) {
            video.removeAttribute("controls");
        }

        if (!video.hasAttribute("tabindex")) {
            video.dataset.previewAddedTabindex = "true";
            video.setAttribute("tabindex", "0");
        }
        if (!video.hasAttribute("aria-label")) {
            video.dataset.previewAddedAriaLabel = "true";
            video.setAttribute("aria-label", "Play video");
        }

        host.dataset.previewControlsSuppressed = "true";
    }

    function restoreInitialPreviewControls(host, video) {
        if (host) {
            delete host.dataset.previewControlsSuppressed;
        }
        if (!video || video.dataset.previewControlsSuppressed !== "true") {
            return;
        }

        if (video.dataset.previewHadControls === "true") {
            video.setAttribute("controls", "");
        }
        if (video.dataset.previewAddedTabindex === "true") {
            video.removeAttribute("tabindex");
        }
        if (video.dataset.previewAddedAriaLabel === "true") {
            video.removeAttribute("aria-label");
        }

        delete video.dataset.previewControlsSuppressed;
        delete video.dataset.previewHadControls;
        delete video.dataset.previewAddedTabindex;
        delete video.dataset.previewAddedAriaLabel;
    }

    function syncInitialPreviewControls(host, shouldSuppress) {
        var video = previewVideo(host);
        if (!video) {
            return;
        }

        if (shouldSuppress) {
            suppressInitialPreviewControls(host, video);
        } else {
            restoreInitialPreviewControls(host, video);
        }
    }

    function previewControlsSuppressed(host) {
        return Boolean(host && host.dataset.previewControlsSuppressed === "true");
    }

    function hideAnimatedPreview(host) {
        var layer = previewLayer(host);
        syncInitialPreviewControls(host, false);
        if (!layer) {
            return;
        }

        layer.classList.remove("is-visible");
        layer.hidden = true;
    }

    function syncAnimatedPreviewVisibility(host) {
        var layer = previewLayer(host);
        var video = previewVideo(host);
        if (!layer || !video) {
            return;
        }

        var initialPausedFrame = isInitialPausedFrame(video);
        var shouldShow = layerHasPreviewContent(layer) && initialPausedFrame;
        layer.hidden = !shouldShow;
        layer.classList.toggle("is-visible", shouldShow);
        syncInitialPreviewControls(host, initialPausedFrame);
    }

    function playFromInitialPreview(host) {
        var video = previewVideo(host);
        if (!video || !previewControlsSuppressed(host)) {
            return;
        }

        hideAnimatedPreview(host);

        var player = video._dashjs_player;
        var playPromise = player && typeof player.play === "function" ? player.play() : video.play();
        if (playPromise && typeof playPromise.catch === "function") {
            playPromise.catch(function () {});
        }
    }

    function desktopPreviewFallbackAllowed() {
        if (!window.matchMedia || !window.matchMedia(DESKTOP_PREVIEW_FALLBACK_QUERY).matches) {
            return false;
        }

        var connection = navigator.connection || navigator.mozConnection || navigator.webkitConnection;
        return !(connection && connection.saveData);
    }

    function stopPreviewFallback(host, unloadSource) {
        var layer = previewLayer(host);
        var image = previewImage(host);
        var fallback = previewFallbackVideo(host);
        var state = activeHosts.get(host);

        if (layer) {
            delete layer.dataset.previewFallbackActive;
        }
        if (image) {
            image.hidden = false;
        }
        if (fallback) {
            fallback.muted = true;
            fallback.pause();
            fallback.hidden = true;
            if (unloadSource && fallback.getAttribute("src")) {
                fallback.removeAttribute("src");
                fallback.load();
            }
        }
        if (state) {
            state.previewFallbackActive = false;
            state.previewFallbackLoading = false;
        }
        if (previewFallbackLoadingHost === host) {
            previewFallbackLoadingHost = null;
        }
        syncPreviewSoundMode();
    }

    function onPreviewFallbackReady(host) {
        var current = activeHosts.get(host);
        var layer = previewLayer(host);
        var image = previewImage(host);
        var fallback = previewFallbackVideo(host);
        if (!current || !current.previewFallbackLoading || !layer || !fallback) {
            return;
        }

        current.previewFallbackLoading = false;
        current.previewFallbackActive = true;
        current.rendered = true;
        layer.dataset.previewFallbackActive = "true";
        if (image) {
            image.hidden = true;
        }
        fallback.hidden = false;
        fallback.muted = true;
        setLoadingState(host, "ready");
        syncAnimatedPreviewVisibility(host);

        var playPromise = fallback.play();
        if (playPromise && typeof playPromise.catch === "function") {
            playPromise.then(function () {
                syncPreviewSoundMode();
            }).catch(function () {});
        } else {
            syncPreviewSoundMode();
        }

        if (previewFallbackLoadingHost === host) {
            previewFallbackLoadingHost = null;
        }
        schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);
    }

    function onPreviewFallbackFailed(host) {
        var current = activeHosts.get(host);
        stopPreviewFallback(host, true);
        if (current) {
            current.previewFallbackFailed = true;
        }
        schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);
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

        stopPreviewFallback(host, true);

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
        host.addEventListener("click", function () {
            playFromInitialPreview(host);
        });
        host.addEventListener("keydown", function (event) {
            if (event.key !== "Enter" && event.key !== " ") {
                return;
            }
            if (!previewControlsSuppressed(host)) {
                return;
            }

            event.preventDefault();
            playFromInitialPreview(host);
        });

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

        var fallback = previewFallbackVideo(host);
        if (fallback) {
            fallback.addEventListener("loadeddata", function () {
                onPreviewFallbackReady(host);
            });
            fallback.addEventListener("canplay", function () {
                onPreviewFallbackReady(host);
            });
            fallback.addEventListener("error", function () {
                onPreviewFallbackFailed(host);
            });
            ["playing", "pause", "ended", "emptied"].forEach(function (eventName) {
                fallback.addEventListener(eventName, function () {
                    syncAnimatedPreviewVisibility(host);
                });
            });
        }

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

    function hostDocumentTop(host) {
        return host.getBoundingClientRect().top + window.scrollY;
    }

    function renderAnimatedPreview(host, state) {
        if (!state || !state.preview || !state.preview.image_path) {
            state.rendered = true;
            return;
        }

        if (state.previewFallbackActive || state.previewFallbackLoading) {
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
            if (!current || !current.preview || current.preview.image_path !== nextPath || current.previewFallbackActive || current.previewFallbackLoading) {
                return;
            }

            var fallback = previewFallbackVideo(host);
            layer.dataset.previewSrc = nextPath;
            image.hidden = false;
            image.src = nextPath;
            if (fallback) {
                fallback.hidden = true;
            }
            video.removeAttribute("poster");
            current.rendered = true;
            current.renderingPreviewPath = "";
            syncPreviewSoundMode();
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

    function schedulePreviewFallbackSweep(delayMs) {
        if (previewFallbackSweepTimer) {
            return;
        }

        previewFallbackSweepTimer = window.setTimeout(function () {
            previewFallbackSweepTimer = null;
            processPreviewFallbackQueue();
        }, delayMs || PREVIEW_FALLBACK_POLL_MS);
    }

    function hostSupportsPreviewFallback(host, state) {
        var video = previewVideo(host);
        return Boolean(
            state &&
            host.dataset.previewFallbackVideoUrl &&
            !state.ready &&
            !state.failed &&
            !state.rendered &&
            !state.previewFallbackActive &&
            !state.previewFallbackLoading &&
            !state.suppressPreviewLease &&
            video &&
            !video.autoplay
        );
    }

    function hasWaitingPreviewFallbackHosts() {
        var waiting = false;
        activeHosts.forEach(function (state, host) {
            if (hostSupportsPreviewFallback(host, state)) {
                waiting = true;
            }
        });
        return waiting;
    }

    function eligiblePreviewFallbackHosts() {
        var now = performance.now();
        var candidates = [];

        activeHosts.forEach(function (state, host) {
            if (!hostSupportsPreviewFallback(host, state)) {
                return;
            }
            if (!shouldRenderHost(host)) {
                return;
            }
            if (!state.previewGenerationStartedAt || now - state.previewGenerationStartedAt < PREVIEW_FALLBACK_DELAY_MS) {
                return;
            }

            candidates.push({
                host: host,
                state: state,
                top: hostDocumentTop(host)
            });
        });

        candidates.sort(function (left, right) {
            return right.top - left.top;
        });

        return candidates;
    }

    function activatePreviewFallback(host, state) {
        var layer = previewLayer(host);
        var fallback = previewFallbackVideo(host);
        var nextPath = host.dataset.previewFallbackVideoUrl || "";
        if (!layer || !fallback || !nextPath || state.previewFallbackLoading || state.previewFallbackActive) {
            return;
        }

        previewFallbackLoadingHost = host;
        state.previewFallbackLoading = true;
        state.previewFallbackFailed = false;
        fallback.hidden = true;

        if (fallback.getAttribute("src") !== nextPath) {
            fallback.setAttribute("src", nextPath);
            fallback.load();
        } else {
            onPreviewFallbackReady(host);
        }
    }

    function processPreviewFallbackQueue() {
        if (!pageSettled || document.visibilityState === "hidden" || !desktopPreviewFallbackAllowed()) {
            if (hasWaitingPreviewFallbackHosts()) {
                schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);
            }
            return;
        }

        if (previewFallbackLoadingHost) {
            return;
        }

        var candidates = eligiblePreviewFallbackHosts();
        if (!candidates.length) {
            if (hasWaitingPreviewFallbackHosts()) {
                schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);
            }
            return;
        }

        activatePreviewFallback(candidates[0].host, candidates[0].state);
    }

    function markPreviewGenerationStart(state) {
        if (!state.previewGenerationStartedAt) {
            state.previewGenerationStartedAt = performance.now();
        }
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
            statusText.textContent = state === "queued" ? "Queued for animated preview generation..." : "Generating animated preview...";
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
                    if (!current.previewFallbackActive) {
                        flushReadyHost(host, current);
                    }
                    return;
                }

                if (payload.state && payload.state.status === "failed") {
                    current.failed = true;
                    setLoadingState(host, "failed");
                    return;
                }

                markPreviewGenerationStart(current);
                if (payload.state && payload.state.status === "pending") {
                    setLoadingState(host, "queued");
                } else {
                    setLoadingState(host, "loading");
                }
                schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);

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

                markPreviewGenerationStart(current);
                schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);
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

        if (host.dataset.previewFallbackVideoUrl && !host.dataset.mediaUrl) {
            state.suppressPreviewLease = true;
            state.ready = true;
            setLoadingState(host, "ready");
            if (!state.previewFallbackActive && !state.previewFallbackLoading) {
                activatePreviewFallback(host, state);
            }
            return;
        }

        if (state.ready || state.leasing || state.suppressPreviewLease) {
            return;
        }

        state.leasing = true;
        state.previewGenerationStartedAt = performance.now();
        setLoadingState(host, "queued");
        schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);

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

                markPreviewGenerationStart(current);
                if (payload.state && payload.state.status === "pending") {
                    setLoadingState(host, "queued");
                } else {
                    setLoadingState(host, "loading");
                }

                schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);
                pollStatus(host, current);
            })
            .catch(function () {
                var current = activeHosts.get(host);
                if (current) {
                    current.leasing = false;
                    current.failed = true;
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

        Array.from(activeHosts.entries()).forEach(function (entry) {
            var host = entry[0];
            var state = entry[1];
            if (state.pollTimer) {
                window.clearTimeout(state.pollTimer);
                state.pollTimer = null;
            }
            stopPreviewFallback(host, true);
        });

        if (previewFallbackSweepTimer) {
            window.clearTimeout(previewFallbackSweepTimer);
            previewFallbackSweepTimer = null;
        }

        postJson("/api/video-previews/release", {
            page_session_id: pageSessionId,
            job_ids: jobIds
        }, useBeacon);

        clearHeartbeat();
    }

    function observeHosts(root) {
        (root || document).querySelectorAll(".video_preview_host[data-media-url], .video_preview_host[data-preview-fallback-video-url]").forEach(function (host) {
            bindPreviewLifecycle(host);

            if (!activeHosts.has(host)) {
                activeHosts.set(host, {});
            }

            leaseHost(host);
        });
        syncPreviewSoundMode();
    }

    function armPageSettledState() {
        if (pageSettled || pageSettledTimer) {
            return;
        }

        pageSettledTimer = window.setTimeout(function () {
            pageSettledTimer = null;
            pageSettled = true;
            schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);
        }, PAGE_SETTLED_DELAY_MS);
    }

    document.addEventListener("visibilitychange", function () {
        if (document.visibilityState === "hidden") {
            releaseAll(true);
            return;
        }

        observeHosts(document);
        ensureHeartbeat();
        schedulePendingRenderCheck();
        schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);
        syncPreviewSoundMode();
    });

    window.addEventListener("pagehide", function () {
        releaseAll(true);
    });

    window.addEventListener("scroll", function () {
        schedulePendingRenderCheck();
        schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);
        syncPreviewSoundMode();
    }, { passive: true });
    window.addEventListener("resize", function () {
        schedulePendingRenderCheck();
        schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);
        syncPreviewSoundMode();
    });

    if (document.readyState === "complete") {
        armPageSettledState();
    } else {
        window.addEventListener("load", armPageSettledState, { once: true });
    }

    window.observeVideoPreviewHosts = observeHosts;
    window.syncVideoPreviewSoundMode = syncPreviewSoundMode;
    bindPreviewSoundToggle();
    observeHosts(document);
})();
// @license-end
