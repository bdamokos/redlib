document.addEventListener("DOMContentLoaded", function () {
	const relativeFormatter = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
	const dateFormatter = new Intl.DateTimeFormat(undefined, {
		dateStyle: "medium",
		timeStyle: "short",
	});
	const relativeTime = (unixSeconds) => {
		const deltaSeconds = unixSeconds - Date.now() / 1000;
		const ranges = [
			["year", 365 * 24 * 60 * 60],
			["month", 30 * 24 * 60 * 60],
			["day", 24 * 60 * 60],
			["hour", 60 * 60],
			["minute", 60],
			["second", 1],
		];
		const [unit, seconds] = ranges.find(([, size]) => Math.abs(deltaSeconds) >= size) || ranges[ranges.length - 1];
		return relativeFormatter.format(Math.round(deltaSeconds / seconds), unit);
	};
	document.querySelectorAll("time[data-local-time]").forEach((element) => {
		const unixSeconds = Number(element.dataset.localTime);
		if (!Number.isFinite(unixSeconds) || unixSeconds <= 0) {
			return;
		}
		const date = new Date(unixSeconds * 1000);
		element.dateTime = date.toISOString();
		element.textContent = `${dateFormatter.format(date)} (${relativeTime(unixSeconds)})`;
		element.title = `${date.toLocaleString()} · Unix ${Math.trunc(unixSeconds)}`;
	});

	const table = document.querySelector("[data-sortable-table]");
	if (!table) {
		return;
	}

	const tbody = table.tBodies[0];
	const headers = Array.from(table.querySelectorAll("thead th"));

	const getCellValue = (row, index, type) => {
		const cell = row.cells[index];
		if (!cell) {
			return null;
		}
		const rawValue = cell.dataset.sortValue ?? cell.textContent.trim();
		if (!rawValue || rawValue === "-") {
			return null;
		}
		if (type === "number") {
			const number = Number(rawValue);
			return Number.isNaN(number) ? null : number;
		}
		return rawValue.toLocaleLowerCase();
	};

	const compareValues = (left, right, direction) => {
		if (left === null && right === null) {
			return 0;
		}
		if (left === null) {
			return 1;
		}
		if (right === null) {
			return -1;
		}
		if (left < right) {
			return direction === "asc" ? -1 : 1;
		}
		if (left > right) {
			return direction === "asc" ? 1 : -1;
		}
		return 0;
	};

	headers.forEach((header, index) => {
		const button = header.querySelector(".sort_button");
		if (!button) {
			return;
		}

		button.addEventListener("click", function () {
			const type = header.dataset.sortType || "text";
			const currentDirection = header.dataset.sortDirection;
			const nextDirection = currentDirection === "asc" ? "desc" : "asc";
			const rows = Array.from(tbody.rows).map((row, originalIndex) => ({ row, originalIndex }));

			rows.sort((left, right) => {
				const leftValue = getCellValue(left.row, index, type);
				const rightValue = getCellValue(right.row, index, type);
				return compareValues(leftValue, rightValue, nextDirection) || (left.originalIndex - right.originalIndex);
			});

			headers.forEach((otherHeader) => {
				delete otherHeader.dataset.sortDirection;
				otherHeader.removeAttribute("aria-sort");
			});

			header.dataset.sortDirection = nextDirection;
			header.setAttribute("aria-sort", nextDirection === "asc" ? "ascending" : "descending");
			rows.forEach(({ row }) => tbody.appendChild(row));
		});
	});
});
