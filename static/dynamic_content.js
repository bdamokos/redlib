(() => {
    const initialize = (root = document) => {
        window.initializeFeedStatus?.(root);
        window.initializeSubscriptionForms?.(root);
        window.loadAuthorLinks?.(root);
        window.markTruncatedPreviews?.();
        window.initializeHlsVideos?.(root);
        window.initializeDashVideos?.(root);
        window.observeVideoPreviewHosts?.(root);
        window.observeVideoVisibility?.(root);
    };

    window.refreshDynamicContent = initialize;
})();
