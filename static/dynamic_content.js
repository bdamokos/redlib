(() => {
    const initialize = (root = document) => {
        window.initializeFeedStatus?.(root);
        window.initializeDiscoveryNavigation?.(root);
        window.initializeSubscriptionForms?.(root);
        window.initializePostFeedback?.(root);
        window.initializePostAuthorActions?.(root);
        window.applyTemporaryAuthorFilters?.(root);
        window.loadAuthorLinks?.(root);
        window.markTruncatedPreviews?.();
        window.refreshPostTimes?.(root);
        window.initializeHlsVideos?.(root);
        window.initializeDashVideos?.(root);
        window.initializeNetworkImages?.(root);
        window.initializeEmbeddingActivity?.(root);
        window.observeVideoPreviewHosts?.(root);
        window.observeVideoVisibility?.(root);
        window.resizeSinglePostMediaToViewport?.(root);
		window.initializePostComponents?.(root);
    };

    window.refreshDynamicContent = initialize;
})();
