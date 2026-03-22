const AUTO_MODE = "auto";
const FAST_BYTES_PER_MS = 250;

function getImageMode() {
  return document.body?.dataset.imageQuality || AUTO_MODE;
}

function contentImages() {
  return Array.from(document.querySelectorAll("img[data-full-src], .post_media_image img, .gallery img"));
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

function wireAutoImageUpgrade() {
  const mode = getImageMode();
  const allImages = contentImages();

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

  showQuality("Images: downloading resized previews");

  const images = Array.from(document.querySelectorAll("img[data-full-src]"));
  if (images.length < 2) {
    showQuality("Images: downloading resized previews");
    return;
  }

  const [firstImage, ...remainingImages] = images;
  const decide = () => {
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

document.addEventListener("DOMContentLoaded", wireAutoImageUpgrade);
