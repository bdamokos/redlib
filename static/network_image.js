const AUTO_MODE = "auto";
const FAST_BYTES_PER_MS = 250;
const EAGER_IMAGE_COUNT = 2;
const LAZY_PRELOAD_MARGIN = "3000px 0px";
const DEFERRED_SRC_PRELOAD_MARGIN = "900px 0px";

let lazyImageObserver = null;
let deferredSrcObserver = null;
let autoUpgradeDecided = false;

function originalImageUrl(img) {
  const candidate = img.dataset.fullSrc || img.closest("a")?.getAttribute("href");
  if (!candidate) {
    return null;
  }

  try {
    const url = new URL(candidate, window.location.href);
    if (url.origin !== window.location.origin
      || !["/img/", "/preview/pre/", "/preview/external-pre/"].some((prefix) => url.pathname.startsWith(prefix))) {
      return null;
    }
    return url.href;
  } catch (_) {
    return null;
  }
}

function removeConfirmedDeletedImage(img) {
  const post = img.closest(".post");
  const gallery = img.closest(".gallery");

  if (gallery) {
    img.closest("figure")?.remove();
    if (gallery.querySelector("img")) {
      return;
    }
  }

  const hasNonMediaContent = post && (
    Array.from(post.querySelectorAll(".post_body, .post_cached_link")).some((element) => element.textContent.trim())
    || Boolean(post.querySelector(".post_poll, [data-post-live-poll]"))
  );
  if (post && !hasNonMediaContent && !post.closest("#page_fragment[data-post-shell-id]")) {
    post.remove();
    return;
  }

  (gallery || img.closest(".post_media_content"))?.remove();
}

async function verifyFailedImage(img) {
  if (img.dataset.deletedMediaCheck !== "pending") {
    return;
  }

  const originalUrl = originalImageUrl(img);
  if (!originalUrl) {
    img.dataset.deletedMediaCheck = "unavailable";
    return;
  }

  const previousVisibility = img.style.visibility;
  img.style.visibility = "hidden";
  const reveal = () => {
    img.style.visibility = previousVisibility;
  };

  try {
    const response = await fetch(originalUrl, {
      cache: "reload",
      credentials: "same-origin",
      headers: { "Accept": "image/*" },
    });
    const deleted = response.status === 410 && response.headers.get("x-redlib-deleted-media") === "1";
    if (deleted) {
      img.dataset.deletedMediaCheck = "deleted";
      removeConfirmedDeletedImage(img);
      return;
    }

    const isImage = response.ok && (response.headers.get("content-type") || "").startsWith("image/");
    if (!isImage) {
      img.dataset.deletedMediaCheck = "failed";
      reveal();
      return;
    }

    // A resized preview can expire while the original remains valid. Drop the
    // failed srcset and retry with the original, which the verification request
    // has just put in the browser/server cache.
    img.removeAttribute("srcset");
    img.removeAttribute("sizes");
    img.dataset.deletedMediaCheck = "fallback";
    if (img.src !== originalUrl) {
      img.addEventListener("load", reveal, { once: true });
      img.addEventListener("error", reveal, { once: true });
      img.src = originalUrl;
    } else {
      reveal();
    }
  } catch (_) {
    img.dataset.deletedMediaCheck = "failed";
    reveal();
  }
}

function wireDeletedImageCheck(img) {
  if (img.dataset.deletedMediaCheckWired === "true") {
    return;
  }
  if (!originalImageUrl(img)) {
    return;
  }

  img.dataset.deletedMediaCheckWired = "true";
  img.addEventListener("error", () => {
    if (["pending", "deleted", "failed", "fallback"].includes(img.dataset.deletedMediaCheck)) {
      return;
    }
    img.dataset.deletedMediaCheck = "pending";
    void verifyFailedImage(img);
  });

  // Deferred scripts can run after a cached image failure has already fired.
  if (img.complete && img.currentSrc && img.naturalWidth === 0) {
    img.dataset.deletedMediaCheck = "pending";
    void verifyFailedImage(img);
  }
}

function getImageMode() {
  return document.body?.dataset.imageQuality || AUTO_MODE;
}

function contentImages(root = document) {
  return Array.from(root.querySelectorAll("img[data-full-src], img[data-lazy-src], .post_media_image img, .gallery img"));
}

function qualityBadge() {
  let badge = document.getElementById("image_quality_status");

  if (!badge) {
    badge = document.createElement("div");
    badge.id = "image_quality_status";
    badge.className = "image_quality_status";
    const nav = document.querySelector("body > nav");
    if (nav) {
      nav.insertAdjacentElement("afterend", badge);
    } else {
      document.body.appendChild(badge);
    }
  }

  return badge;
}

function showQuality(message) {
  const badge = qualityBadge();
  badge.textContent = message;
  badge.classList.add("visible");
}

function shouldUpgradeAfterPreview(img) {
  const entries = performance.getEntriesByName(img.currentSrc || img.src);
  const entry = entries[entries.length - 1];

  if (!entry) {
    return true;
  }

  const duration = entry.duration || (entry.responseEnd - entry.startTime);
  const transferSize = entry.transferSize || entry.encodedBodySize || entry.decodedBodySize;

  if (!duration || !transferSize) {
    return true;
  }

  return transferSize / duration >= FAST_BYTES_PER_MS;
}

function upgradeDeferredImages(images) {
  images.forEach((img) => {
    const fullSrc = img.dataset.fullSrc;
    if (!fullSrc || img.dataset.upgraded === "true" || img.complete) {
      return;
    }

    img.removeAttribute("srcset");
    img.src = fullSrc;
    img.dataset.upgraded = "true";
  });
}

function setEager(img, highPriority = false) {
  if (img.loading === "lazy" || img.getAttribute("loading") === "lazy") {
    img.loading = "eager";
    img.setAttribute("loading", "eager");
  }

  if (highPriority && "fetchPriority" in img) {
    img.fetchPriority = "high";
  }
}

function activateDeferredSrc(img, highPriority = false) {
  const lazySrc = img.dataset.lazySrc;
  if (lazySrc && !img.hasAttribute("src")) {
    img.src = lazySrc;
    img.dataset.lazyLoaded = "true";
  }

  setEager(img, highPriority);
}

function deferredSrcPreloadObserver() {
  if (deferredSrcObserver || !("IntersectionObserver" in window)) {
    return deferredSrcObserver;
  }

  deferredSrcObserver = new IntersectionObserver((entries) => {
    entries.forEach((entry) => {
      if (!entry.isIntersecting) {
        return;
      }

      const img = entry.target;
      activateDeferredSrc(img);
      deferredSrcObserver.unobserve(img);
    });
  }, {
    rootMargin: DEFERRED_SRC_PRELOAD_MARGIN,
    threshold: 0,
  });

  return deferredSrcObserver;
}

function lazyPreloadObserver() {
  if (lazyImageObserver || !("IntersectionObserver" in window)) {
    return lazyImageObserver;
  }

  lazyImageObserver = new IntersectionObserver((entries) => {
    entries.forEach((entry) => {
      if (!entry.isIntersecting) {
        return;
      }

      const img = entry.target;
      setEager(img);
      lazyImageObserver.unobserve(img);
    });
  }, {
    rootMargin: LAZY_PRELOAD_MARGIN,
    threshold: 0,
  });

  return lazyImageObserver;
}

function primeLazyImages(root = document) {
  const images = contentImages(root);
  const observer = lazyPreloadObserver();
  const deferredObserver = deferredSrcPreloadObserver();

  images.forEach((img, index) => {
    wireDeletedImageCheck(img);

    if (img.dataset.networkImagePrimed === "true") {
      return;
    }

    img.dataset.networkImagePrimed = "true";

    if (img.dataset.lazySrc) {
      if (deferredObserver) {
        deferredObserver.observe(img);
      } else {
        activateDeferredSrc(img, index === 0);
      }
      return;
    }

    if (index < EAGER_IMAGE_COUNT || !observer) {
      setEager(img, index === 0);
      return;
    }

    observer.observe(img);
  });
}

function wireAutoImageUpgrade(root = document) {
  const mode = getImageMode();
  const allImages = contentImages(root);

  if (mode === "best") {
    showQuality("Images: downloading full resolution");
    return;
  }

  if (mode === "display") {
    showQuality("Images: downloading resized previews");
    return;
  }

  if (allImages.length === 0) {
    showQuality("Images: no inline images");
    return;
  }

  if (autoUpgradeDecided) {
    return;
  }

  showQuality("Images: downloading resized previews");

  const images = Array.from(document.querySelectorAll("img[data-full-src]"));
  if (images.length < 2) {
    showQuality("Images: downloading resized previews");
    return;
  }

  const [firstImage, ...remainingImages] = images;
  const decide = () => {
    autoUpgradeDecided = true;
    if (shouldUpgradeAfterPreview(firstImage)) {
      upgradeDeferredImages(remainingImages);
      showQuality("Images: downloading full resolution");
    } else {
      showQuality("Images: downloading resized previews");
    }
  };

  if (firstImage.complete) {
    decide();
    return;
  }

  firstImage.addEventListener("load", decide, { once: true });
}

function initializeNetworkImages(root = document) {
  primeLazyImages(root);
  wireAutoImageUpgrade(root);
}

window.initializeNetworkImages = initializeNetworkImages;
document.addEventListener("DOMContentLoaded", () => initializeNetworkImages(document));
