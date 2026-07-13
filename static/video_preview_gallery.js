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
    var FILTERED_AUTHOR_SELECTOR = "[data-temporary-author-filtered='true'], [data-permanent-author-filtered='true']";

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

    function hostUsesDirectPreviewFallback(host) {
        return Boolean(host && host.dataset.previewFallbackVideoUrl && !host.dataset.mediaUrl);
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

    function fullyVisibleFallbackHosts() {
        var viewportTop = scrollPaddingTop();
        var viewportBottom = window.innerHeight;
        var viewportRight = window.innerWidth;
        return Array.from(activeHosts.keys()).filter(function (host) {
            var fallback = previewFallbackVideo(host);
            if (!fallback || fallback.hidden) {
                return false;
            }

            var rect = fallback.getBoundingClientRect();
            return rect.top >= viewportTop && rect.bottom <= viewportBottom && rect.left >= 0 && rect.right <= viewportRight;
        });
    }

    function currentPreviewSoundHost() {
        var hosts = fullyVisibleFallbackHosts();
        hosts.sort(function (left, right) {
            return previewFallbackVideo(left).getBoundingClientRect().top - previewFallbackVideo(right).getBoundingClientRect().top;
        });

        return hosts[0] || null;
    }

    function browserAllowsPreviewSound() {
        return browserAllowsPreviewPlayback();
    }

    function pageIsVisible() {
        if (document.visibilityState !== "visible") {
            return false;
        }

        return true;
    }

    function mobilePreviewFocusIsUnreliable() {
        if (navigator.maxTouchPoints && navigator.maxTouchPoints > 0) {
            return true;
        }

        return Boolean(window.matchMedia && window.matchMedia("(hover: none), (pointer: coarse)").matches);
    }

    function browserAllowsForegroundPreviewPlayback() {
        if (!pageIsVisible()) {
            return false;
        }

        return typeof document.hasFocus !== "function" || document.hasFocus();
    }

    function browserAllowsPreviewPlayback() {
        return pageIsVisible() && (mobilePreviewFocusIsUnreliable() || browserAllowsForegroundPreviewPlayback());
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
        var soundHost = previewSoundEnabled && browserAllowsPreviewSound() ? currentPreviewSoundHost() : null;

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

        seekVideoToPreviewFallbackTime(host, video);
        hideAnimatedPreview(host);

        var player = video._dashjs_player;
        var playPromise = player && typeof player.play === "function" ? player.play() : video.play();
        if (playPromise && typeof playPromise.catch === "function") {
            playPromise.catch(function () {});
        }
    }

    function activePreviewFallbackTime(host) {
        var layer = previewLayer(host);
        var fallback = previewFallbackVideo(host);
        if (!layer || !fallback || fallback.hidden || layer.dataset.previewFallbackActive !== "true") {
            return 0;
        }

        var currentTime = Number(fallback.currentTime);
        if (!Number.isFinite(currentTime) || currentTime <= 0.05) {
            return 0;
        }

        return clampMediaTime(fallback, currentTime);
    }

    function clampMediaTime(video, currentTime) {
        var clampedTime = Number(currentTime);
        if (!Number.isFinite(clampedTime) || clampedTime <= 0) {
            return 0;
        }

        var duration = Number(video.duration);
        if (Number.isFinite(duration) && duration > 0) {
            clampedTime = Math.min(clampedTime, Math.max(0, duration - 0.05));
        }

        return clampedTime;
    }

    function seekVideoToTime(video, currentTime) {
        var seekTime = clampMediaTime(video, currentTime);
        if (seekTime <= 0) {
            return;
        }

        var player = video._dashjs_player;
        try {
            if (player && typeof player.seek === "function") {
                player.seek(seekTime);
                return;
            }

            var videoTime = Number(video.currentTime);
            if (!Number.isFinite(videoTime) || Math.abs(videoTime - seekTime) > 0.25) {
                video.currentTime = seekTime;
            }
        } catch (error) {}
    }

    function seekVideoToPreviewFallbackTime(host, video) {
        var startTime = activePreviewFallbackTime(host);
        if (startTime <= 0) {
            return;
        }

        if (video.readyState >= 1) {
            seekVideoToTime(video, startTime);
            return;
        }

        var deferredSeek = function () {
            video.removeEventListener("loadedmetadata", deferredSeek);
            video.removeEventListener("durationchange", deferredSeek);
            seekVideoToTime(video, startTime);
        };
        video.addEventListener("loadedmetadata", deferredSeek);
        video.addEventListener("durationchange", deferredSeek);
        seekVideoToTime(video, startTime);
    }

    function desktopPreviewFallbackAllowed() {
        if (!window.matchMedia || !window.matchMedia(DESKTOP_PREVIEW_FALLBACK_QUERY).matches) {
            return false;
        }

        var connection = navigator.connection || navigator.mozConnection || navigator.webkitConnection;
        return !(connection && connection.saveData);
    }

    function rememberPreviewFallbackResumePoint(host, fallback) {
        var state = activeHosts.get(host);
        var src = fallback ? fallback.getAttribute("src") : "";
        if (!state || !src) {
            return;
        }

        var currentTime = Number(fallback.currentTime);
        if (!Number.isFinite(currentTime) || currentTime <= 0) {
            return;
        }

        state.previewFallbackResumeSrc = src;
        state.previewFallbackResumeTime = currentTime;
    }

    function restorePreviewFallbackResumePoint(host, fallback) {
        var state = activeHosts.get(host);
        var src = fallback ? fallback.getAttribute("src") : "";
        var resumeTime = state ? Number(state.previewFallbackResumeTime) : 0;
        if (!state || !src || state.previewFallbackResumeSrc !== src || !Number.isFinite(resumeTime) || resumeTime <= 0) {
            return;
        }

        var duration = Number(fallback.duration);
        if (Number.isFinite(duration) && duration > 0) {
            resumeTime = Math.min(resumeTime, Math.max(0, duration - 0.05));
        }

        try {
            if (Math.abs(Number(fallback.currentTime) - resumeTime) > 0.25) {
                fallback.currentTime = resumeTime;
            }
            delete state.previewFallbackResumeSrc;
            delete state.previewFallbackResumeTime;
        } catch (error) {}
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
            rememberPreviewFallbackResumePoint(host, fallback);
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
        restorePreviewFallbackResumePoint(host, fallback);
        setLoadingState(host, "ready");
        syncAnimatedPreviewVisibility(host);

        if (!browserAllowsPreviewPlayback() || !shouldRenderHost(host)) {
            stopPreviewFallback(host, false);
            return;
        }

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

    function hostIsUnavailable(host) {
        var post = host.closest(".post");
        var target = post || host;
        return Boolean(target.closest(FILTERED_AUTHOR_SELECTOR) || target.hidden || target.getClientRects().length === 0);
    }

    function shouldRenderHost(host) {
        if (hostIsUnavailable(host)) {
            return false;
        }

        var post = host.closest(".post");
        var target = post || host;
        var rect = target.getBoundingClientRect();
        return rect.bottom > scrollPaddingTop() && rect.top < window.innerHeight;
    }

    function releaseHost(host, jobIds) {
        var state = activeHosts.get(host);
        if (!state) {
            stopPreviewFallback(host, true);
            return;
        }

        if (state.pollTimer) {
            window.clearTimeout(state.pollTimer);
            state.pollTimer = null;
        }
        if (state.jobId) {
            jobIds.push(state.jobId);
        }
        stopPreviewFallback(host, true);
        activeHosts.delete(host);
    }

    function releasePostPreviewHosts(post) {
        var jobIds = [];
        post.querySelectorAll(".video_preview_host[data-media-url], .video_preview_host[data-preview-fallback-video-url]").forEach(function (host) {
            releaseHost(host, jobIds);
        });

        if (previewFallbackSweepTimer && !hasWaitingPreviewFallbackHosts()) {
            window.clearTimeout(previewFallbackSweepTimer);
            previewFallbackSweepTimer = null;
        }

        if (jobIds.length > 0) {
            postJson("/api/video-previews/release", {
                page_session_id: pageSessionId,
                job_ids: jobIds
            });
        }

        if (activeHosts.size === 0) {
            clearHeartbeat();
        }
        syncPreviewSoundMode();
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
        var directFallback = hostUsesDirectPreviewFallback(host);
        return Boolean(
            state &&
            host.dataset.previewFallbackVideoUrl &&
            (directFallback || !state.ready) &&
            !state.failed &&
            !state.rendered &&
            !state.previewFallbackActive &&
            !state.previewFallbackLoading &&
            (directFallback || !state.suppressPreviewLease) &&
            video &&
            !video.autoplay
        );
    }

    function hasWaitingPreviewFallbackHosts() {
        if (!browserAllowsPreviewPlayback()) {
            return false;
        }

        var waiting = false;
        activeHosts.forEach(function (state, host) {
            if (hostSupportsPreviewFallback(host, state) && shouldRenderHost(host)) {
                waiting = true;
            }
        });
        return waiting;
    }

    function eligiblePreviewFallbackHosts(directOnly) {
        var now = performance.now();
        var candidates = [];

        activeHosts.forEach(function (state, host) {
            if (directOnly && !hostUsesDirectPreviewFallback(host)) {
                return;
            }
            if (!hostSupportsPreviewFallback(host, state)) {
                return;
            }
            if (!shouldRenderHost(host)) {
                return;
            }
            if (!hostUsesDirectPreviewFallback(host) && (!state.previewGenerationStartedAt || now - state.previewGenerationStartedAt < PREVIEW_FALLBACK_DELAY_MS)) {
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
        if (!layer || !fallback || !nextPath || state.previewFallbackLoading || state.previewFallbackActive || !browserAllowsPreviewPlayback() || !shouldRenderHost(host)) {
            return;
        }

        previewFallbackLoadingHost = host;
        state.previewFallbackLoading = true;
        state.previewFallbackFailed = false;
        fallback.defaultMuted = true;
        fallback.muted = true;
        fallback.autoplay = true;
        fallback.playsInline = true;
        fallback.hidden = true;

        if (fallback.getAttribute("src") !== nextPath) {
            fallback.setAttribute("src", nextPath);
            fallback.load();
        } else {
            onPreviewFallbackReady(host);
        }
    }

    function activateVisibleDirectPreviewFallbacks() {
        syncPreviewFallbackPlayback();
        if (!browserAllowsPreviewPlayback()) {
            return;
        }

        activeHosts.forEach(function (state, host) {
            if (!hostUsesDirectPreviewFallback(host) || !hostSupportsPreviewFallback(host, state) || !shouldRenderHost(host)) {
                return;
            }

            activatePreviewFallback(host, state);
        });
    }

    function syncPreviewFallbackPlayback(forcePlaybackAllowed) {
        var playbackAllowed = typeof forcePlaybackAllowed === "boolean" ? forcePlaybackAllowed : browserAllowsPreviewPlayback();

        activeHosts.forEach(function (state, host) {
            if (!state || (!state.previewFallbackActive && !state.previewFallbackLoading)) {
                return;
            }
            if (playbackAllowed && shouldRenderHost(host)) {
                return;
            }

            stopPreviewFallback(host, false);
        });
    }

    function processPreviewFallbackQueue() {
        if (!pageSettled || !browserAllowsPreviewPlayback()) {
            if (hasWaitingPreviewFallbackHosts()) {
                schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);
            }
            return;
        }

        if (previewFallbackLoadingHost) {
            return;
        }

        var directOnly = !desktopPreviewFallbackAllowed();
        var candidates = eligiblePreviewFallbackHosts(directOnly);
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

        if (hostUsesDirectPreviewFallback(host)) {
            state.suppressPreviewLease = true;
            state.ready = true;
            markPreviewGenerationStart(state);
            setLoadingState(host, "ready");
            if (browserAllowsPreviewPlayback() && shouldRenderHost(host) && !state.previewFallbackActive && !state.previewFallbackLoading) {
                activatePreviewFallback(host, state);
            } else if (browserAllowsPreviewPlayback() && !state.previewFallbackActive && !state.previewFallbackLoading) {
                schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);
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
            if (hostIsUnavailable(host)) {
                return;
            }

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

    function resumeForegroundPreviewWork() {
        observeHosts(document);
        ensureHeartbeat();
        schedulePendingRenderCheck();
        activateVisibleDirectPreviewFallbacks();
        schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);
        syncPreviewSoundMode();
    }

    function pauseBackgroundPreviewWork() {
        syncPreviewFallbackPlayback();
        syncPreviewSoundMode();
    }

    document.addEventListener("visibilitychange", function () {
        if (document.visibilityState === "hidden") {
            releaseAll(true);
            return;
        }

        resumeForegroundPreviewWork();
    });

    window.addEventListener("pagehide", function () {
        releaseAll(true);
    });

    window.addEventListener("scroll", function () {
        schedulePendingRenderCheck();
        activateVisibleDirectPreviewFallbacks();
        schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);
        syncPreviewSoundMode();
    }, { passive: true });
    window.addEventListener("resize", function () {
        schedulePendingRenderCheck();
        activateVisibleDirectPreviewFallbacks();
        schedulePreviewFallbackSweep(PREVIEW_FALLBACK_POLL_MS);
        syncPreviewSoundMode();
    });
    window.addEventListener("focus", resumeForegroundPreviewWork);
    window.addEventListener("blur", pauseBackgroundPreviewWork);
    document.addEventListener("redlib:post-media-unloaded", function (event) {
        var post = event.detail && event.detail.post instanceof Element ? event.detail.post : event.target.closest(".post");
        if (post) {
            releasePostPreviewHosts(post);
        }
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
