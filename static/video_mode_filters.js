const VIDEO_MODE_APPLY_DEBOUNCE_MS = 250;
const VIDEO_MODE_PARAM_PREFIX = "video_";

function syncVideoFilterMode(group, target) {
	const checkboxes = Array.from(group.querySelectorAll(`input[type="checkbox"][name="${target}"]`));
	const modeInput = group.querySelector(`[data-video-filter-mode="${target}"]`);
	if (!modeInput) {
		return;
	}

	modeInput.value = checkboxes.some((checkbox) => checkbox.checked) ? "" : "none";
}

function facetPayloadFromGroup(group, checkboxName) {
	const checkboxes = Array.from(group.querySelectorAll(`input[type="checkbox"][name="${checkboxName}"]`));
	const checked = checkboxes.filter((c) => c.checked).map((c) => c.value.trim()).filter(Boolean);
	const unchecked = checkboxes.filter((c) => !c.checked).map((c) => c.value.trim()).filter(Boolean);

	if (checked.length === 0) {
		return { mode: "none", values: [] };
	}
	if (unchecked.length < checked.length) {
		return { mode: "exclude", values: unchecked };
	}
	return { mode: "include", values: checked };
}

function readAgeFromForm(form) {
	const minInput = form.querySelector('input[name="video_age_min_hours"]');
	const maxInput = form.querySelector('input[name="video_age_max_hours"]');
	const ageMinHours = minInput ? Number.parseInt(minInput.value || "0", 10) || 0 : 0;
	let ageMaxHours = maxInput ? Number.parseInt(maxInput.value || "0", 10) : 0;
	if (Number.isNaN(ageMaxHours)) {
		ageMaxHours = 0;
	}
	return { ageMinHours, ageMaxHours };
}

function buildApplyBody(form) {
	const subMode = form.querySelector('[data-video-filter-mode="video_sub"]');
	const userMode = form.querySelector('[data-video-filter-mode="video_user"]');
	const subGroup = subMode ? subMode.closest(".video_mode_filter_group") : null;
	const userGroup = userMode ? userMode.closest(".video_mode_filter_group") : null;

	const subreddits = subGroup ? facetPayloadFromGroup(subGroup, "video_sub") : { mode: "include", values: [] };
	const users = userGroup ? facetPayloadFromGroup(userGroup, "video_user") : { mode: "include", values: [] };

	const { ageMinHours, ageMaxHours } = readAgeFromForm(form);
	const bound = form.querySelector('input[name="video_age_max_hours"]');
	const maxBound = bound ? Number.parseInt(bound.getAttribute("max") || "0", 10) : 0;
	const useMax = Number.isFinite(maxBound) && maxBound > 0 && ageMaxHours >= maxBound;

	return {
		video_mode: {
			subreddits,
			users,
			age_min_hours: ageMinHours,
			age_max_hours: useMax ? null : ageMaxHours,
		},
	};
}

function stripVideoParamsFromLocation() {
	const url = new URL(window.location.href);
	const keys = [];
	url.searchParams.forEach((_, key) => {
		if (key === "video_sub" || key === "video_user" || key === "video_sub_mode" || key === "video_user_mode" || key.startsWith(VIDEO_MODE_PARAM_PREFIX)) {
			keys.push(key);
		}
	});
	for (const key of keys) {
		url.searchParams.delete(key);
	}
	const search = url.searchParams.toString();
	const next = `${url.pathname}${search ? `?${search}` : ""}${url.hash}`;
	window.history.replaceState({}, "", next);
}

document.addEventListener("click", (event) => {
	const button = event.target.closest("[data-video-filter-toggle]");
	if (!button) {
		return;
	}

	const group = button.closest(".video_mode_filter_group");
	const target = button.getAttribute("data-video-filter-target");
	const toggle = button.getAttribute("data-video-filter-toggle");
	if (!group || !target || !toggle) {
		return;
	}

	const shouldCheck = toggle === "check";
	group.querySelectorAll(`input[type="checkbox"][name="${target}"]`).forEach((checkbox) => {
		checkbox.checked = shouldCheck;
	});
	syncVideoFilterMode(group, target);
	const form = button.closest(".video_mode_filter_form");
	if (form) {
		window.scheduleVideoModeApply?.(form);
	}
});

let applyTimer = null;
let applyAbort = null;

const runApply = async (form) => {
	if (applyAbort) {
		applyAbort.abort();
	}
	applyAbort = new AbortController();

	try {
		const response = await fetch("/api/video-mode/apply", {
			method: "POST",
			credentials: "same-origin",
			headers: {
				"Content-Type": "application/json",
				"Cache-Control": "no-store",
			},
			body: JSON.stringify(buildApplyBody(form)),
			signal: applyAbort.signal,
		});

		if (response.status === 413) {
			window.console?.warn?.("video_mode_filters: cookie payload too large");
			return;
		}
		if (!response.ok) {
			window.console?.warn?.("video_mode_filters: apply failed", response.status);
			return;
		}

		stripVideoParamsFromLocation();
		window.location.reload();
	} catch (error) {
		if (String(error).includes("abort")) {
			return;
		}
		window.console?.warn?.("video_mode_filters: apply error", error);
	}
};

window.scheduleVideoModeApply = (form) => {
	if (applyTimer) {
		window.clearTimeout(applyTimer);
	}
	applyTimer = window.setTimeout(() => {
		applyTimer = null;
		void runApply(form);
	}, VIDEO_MODE_APPLY_DEBOUNCE_MS);
};

function wireVideoModeFilterForms(root = document) {
	const scope = root.querySelectorAll ? root : document;
	const forms = scope.querySelectorAll ? scope.querySelectorAll(".video_mode_filter_form:not([data-video-mode-wired])") : [];

	for (const form of forms) {
		form.dataset.videoModeWired = "true";

		form.addEventListener("submit", (event) => {
			event.preventDefault();
			window.scheduleVideoModeApply(form);
		});

		form.addEventListener("change", (event) => {
			const checkbox = event.target.closest('.video_mode_filter_option input[type="checkbox"]');
			if (checkbox) {
				const group = checkbox.closest(".video_mode_filter_group");
				if (group) {
					syncVideoFilterMode(group, checkbox.name);
				}
			}
			window.scheduleVideoModeApply(form);
		});

		form.addEventListener(
			"input",
			(event) => {
				const target = event.target;
				if (target && target.matches && target.matches('input[type="range"][name="video_age_min_hours"], input[type="range"][name="video_age_max_hours"]')) {
					window.scheduleVideoModeApply(form);
				}
			},
			true,
		);
	}
}

document.addEventListener("DOMContentLoaded", () => {
	wireVideoModeFilterForms(document);
	const previousRefresh = window.refreshDynamicContent;
	window.refreshDynamicContent = (root) => {
		previousRefresh?.(root);
		wireVideoModeFilterForms(root || document);
	};
});
