(() => {
	"use strict";

	const labels = {
		snapcode: "Snapchat Snapcode detected",
		social_promo: "Social contact promotion detected",
		vote_bait: "Upvote solicitation detected",
		urgency_bait: "Time-pressure promotion detected",
	};
	let timer = 0;
	let requestRunning = false;

	function posts() {
		return Array.from(document.querySelectorAll(".post[data-post-id]"));
	}

	function addBanner(post, signal) {
		if (post.querySelector(":scope > .post_promo_warning")) return;
		const banner = document.createElement("div");
		banner.className = `post_promo_warning post_promo_warning_${signal}`;
		banner.setAttribute("role", "status");
		banner.textContent = labels[signal] || "Promotional pattern detected";
		const header = post.querySelector(":scope > .post_header, :scope > .post_title");
		if (header) header.insertAdjacentElement("afterend", banner);
		else post.prepend(banner);
	}

	async function hydrate() {
		if (requestRunning) return;
		const candidates = posts().filter((post) => !post.querySelector(":scope > .post_promo_warning"));
		if (!candidates.length) return;
		requestRunning = true;
		try {
			for (let offset = 0; offset < candidates.length; offset += 75) {
				const batch = candidates.slice(offset, offset + 75);
				const ids = batch.map((post) => post.dataset.postId).filter(Boolean);
				if (!ids.length) continue;
				const response = await fetch(`/api/promo-status?ids=${encodeURIComponent(ids.join(","))}`, {
					credentials: "same-origin",
					cache: "no-store",
				});
				if (!response.ok) continue;
				const payload = await response.json();
				for (const post of batch) {
					const result = payload.signals && payload.signals[post.dataset.postId];
					if (result && result.signal) addBanner(post, result.signal);
				}
			}
		} catch (_) {
			// Detection is advisory and must never interfere with rendering.
		} finally {
			requestRunning = false;
		}
	}

	function schedule(delay = 100) {
		window.clearTimeout(timer);
		timer = window.setTimeout(hydrate, delay);
	}

	document.addEventListener("DOMContentLoaded", () => {
		hydrate();
		window.setTimeout(hydrate, 450);
		window.setTimeout(hydrate, 1600);
		document.addEventListener("load", (event) => {
			if (event.target instanceof HTMLImageElement) schedule(120);
		}, true);
		new MutationObserver(() => schedule()).observe(document.body, { childList: true, subtree: true });
	});
})();
