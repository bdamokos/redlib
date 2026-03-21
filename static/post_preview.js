function markTruncatedPreviews() {
    for (const preview of document.querySelectorAll(".post_preview:not(.expanded)")) {
        preview.classList.toggle("truncated", preview.scrollHeight > preview.clientHeight);
    }
}

globalThis.markTruncatedPreviews = markTruncatedPreviews;
markTruncatedPreviews();
window.addEventListener("load", markTruncatedPreviews);
document.addEventListener(
    "load",
    (event) => {
        if (event.target instanceof HTMLImageElement && event.target.closest(".post_preview")) {
            markTruncatedPreviews();
        }
    },
    true
);

document.addEventListener("click", (event) => {
    const preview = event.target.closest(".post_preview.truncated:not(.expanded)");
    if (!preview) {
        return;
    }

    const isGallery = preview.classList.contains("gallery");
    const clickedLink = event.target.closest("a");

    if (isGallery) {
        const rect = preview.getBoundingClientRect();
        const clickY = event.clientY - rect.top;
        const threshold = rect.height - 48;
        if (clickY >= threshold) {
            event.preventDefault();
            preview.classList.add("expanded");
            preview.classList.remove("truncated");
            return;
        }
    } else {
        if (clickedLink) {
            return;
        }
        preview.classList.add("expanded");
        preview.classList.remove("truncated");
    }
});
