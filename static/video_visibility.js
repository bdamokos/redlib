// @license http://www.gnu.org/licenses/agpl-3.0.html AGPL-3.0
(function () {
    var visibilityObserver;

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
        var observer = getVisibilityObserver();
        if (!observer) {
            return;
        }

        var container = root || document;
        var videos = container.querySelectorAll("video.post_media_video");
        videos.forEach(function (videoEl) {
            observer.observe(videoEl);
        });
    }

    window.observeVideoVisibility = observeVideoVisibility;
    observeVideoVisibility(document);
})();
// @license-end
