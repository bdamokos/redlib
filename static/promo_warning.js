(() => {
	"use strict";

	let timer = 0;
	let requestRunning = false;

	function posts() {
		return Array.from(document.querySelectorAll(".post[data-post-id]"));
	}

	function authorFromPost(post) {
		const link = post.querySelector(".post_author[href^='/user/'], .post_author[href^='/u/']");
		if (!link) return "";
		return (link.textContent || "").trim().replace(/^u\//i, "").toLowerCase();
	}

	function eliminatePost(post) {
		const next = post.nextElementSibling;
		const previous = post.previousElementSibling;
		post.remove();
		if (next && next.matches(".sep")) next.remove();
		else if (previous && previous.matches(".sep")) previous.remove();
	}

	function eliminateAuthor(author, exemptPostIds, candidates) {
		const normalized = (author || "").trim().toLowerCase();
		if (!normalized) return;
		for (const post of candidates) {
			if (exemptPostIds.has(post.dataset.postId || "")) continue;
			if (authorFromPost(post) === normalized) eliminatePost(post);
		}
	}

	async function hydrate() {
		if (requestRunning) return;
		const candidates = posts();
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
				const exemptPostIds = new Set(payload.redgifs_exempt_post_ids || []);
				for (const post of batch) {
					const result = payload.signals && payload.signals[post.dataset.postId];
					if (result && result.signal) eliminatePost(post);
				}
				for (const author of Object.keys(payload.auto_filters || {})) eliminateAuthor(author, exemptPostIds, batch);
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
