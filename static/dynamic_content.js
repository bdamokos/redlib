(() => {
    const initialize = (root = document) => {
        window.initializeFeedStatus?.(root);
        window.initializeSubscriptionForms?.(root);
        window.loadAuthorLinks?.(root);
        window.markTruncatedPreviews?.();
        window.refreshPostTimes?.(root);
        window.initializeHlsVideos?.(root);
        window.initializeDashVideos?.(root);
        window.initializeNetworkImages?.(root);
        window.observeVideoPreviewHosts?.(root);
        window.observeVideoVisibility?.(root);
        window.resizeSinglePostMediaToViewport?.(root);
    };

    window.refreshDynamicContent = initialize;
})();
