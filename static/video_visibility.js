// @license http://www.gnu.org/licenses/agpl-3.0.html AGPL-3.0
(function () {
    var visibilityObserver;

    function progressBarForVideo(videoEl) {
        var stage = videoEl ? videoEl.closest(".video_preview_stage") : null;
        return stage ? stage.querySelector(".video_playback_progress") : null;
    }

    function syncVideoProgress(videoEl) {
        var progress = progressBarForVideo(videoEl);
        if (!progress) {
            return;
        }

        var duration = videoEl.duration;
        var hasDuration = Number.isFinite(duration) && duration > 0;
        progress.hidden = !hasDuration;

        var ratio = 0;
        if (hasDuration && Number.isFinite(videoEl.currentTime)) {
            ratio = Math.max(0, Math.min(1, videoEl.currentTime / duration));
        }

        progress.style.setProperty("--video-playback-progress", ratio.toString());
    }

    function scheduleVideoProgress(videoEl) {
        if (!videoEl || videoEl._redlib_progress_frame) {
            return;
        }

        videoEl._redlib_progress_frame = window.requestAnimationFrame(function () {
            videoEl._redlib_progress_frame = 0;
            syncVideoProgress(videoEl);
            if (!videoEl.paused && !videoEl.ended) {
                scheduleVideoProgress(videoEl);
            }
        });
    }

    function bindVideoProgress(videoEl) {
        if (!videoEl || videoEl._redlib_progress_bound) {
            syncVideoProgress(videoEl);
            return;
        }

        videoEl._redlib_progress_bound = true;
        ["durationchange", "loadedmetadata", "timeupdate", "seeked", "emptied", "ended"].forEach(function (eventName) {
            videoEl.addEventListener(eventName, function () {
                syncVideoProgress(videoEl);
            });
        });
        ["play", "playing"].forEach(function (eventName) {
            videoEl.addEventListener(eventName, function () {
                scheduleVideoProgress(videoEl);
            });
        });

        syncVideoProgress(videoEl);
        if (!videoEl.paused && !videoEl.ended) {
            scheduleVideoProgress(videoEl);
        }
    }

    function pauseVideo(videoEl) {
        var player = videoEl._dashjs_player;
        if (player && typeof player.pause === "function") {
            player.pause();
            return;
        }

        if (!videoEl.paused) {
            videoEl.pause();
        }
    }

    function handleVideoVisibility(entries) {
        entries.forEach(function (entry) {
            if (!entry.isIntersecting) {
                pauseVideo(entry.target);
            }
        });
    }

    function getVisibilityObserver() {
        if (!("IntersectionObserver" in window)) {
            return null;
        }

        if (!visibilityObserver) {
            visibilityObserver = new IntersectionObserver(handleVideoVisibility, {
                rootMargin: "100px 0px",
                threshold: 0
            });
        }

        return visibilityObserver;
    }

    function observeVideoVisibility(root) {
        var container = root || document;
        var videos = container.querySelectorAll("video.post_media_video");
        videos.forEach(function (videoEl) {
            bindVideoProgress(videoEl);
        });

        var observer = getVisibilityObserver();
        if (!observer) {
            return;
        }

        videos.forEach(function (videoEl) {
            observer.observe(videoEl);
        });
    }

    window.observeVideoVisibility = observeVideoVisibility;
    window.bindVideoProgress = bindVideoProgress;
    observeVideoVisibility(document);
})();
// @license-end
