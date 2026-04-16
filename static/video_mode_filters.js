function syncVideoFilterMode(group, target) {
	const checkboxes = Array.from(group.querySelectorAll(`input[type="checkbox"][name="${target}"]`));
	const modeInput = group.querySelector(`[data-video-filter-mode="${target}"]`);
	if (!modeInput) {
		return;
	}

	modeInput.value = checkboxes.some((checkbox) => checkbox.checked) ? "" : "none";
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
});

document.addEventListener("change", (event) => {
	const checkbox = event.target.closest('.video_mode_filter_option input[type="checkbox"]');
	if (!checkbox) {
		return;
	}

	const group = checkbox.closest(".video_mode_filter_group");
	if (!group) {
		return;
	}

	syncVideoFilterMode(group, checkbox.name);
});
