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

    if (event.target.closest("a")) {
        return;
    }

    preview.classList.add("expanded");
    preview.classList.remove("truncated");
});
