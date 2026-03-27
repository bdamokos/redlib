(() => {
    const initialize = (root = document) => {
        window.initializeSubscriptionForms?.(root);
        window.loadAuthorLinks?.(root);
        window.markTruncatedPreviews?.();
        window.initializeHlsVideos?.(root);
        window.initializeDashVideos?.(root);
        window.observeVideoPreviewHosts?.(root);
        window.observeVideoVisibility?.(root);
        window.initializeSubmittedDuplicates?.();
    };

    window.refreshDynamicContent = initialize;
})();
