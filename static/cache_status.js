document.addEventListener("DOMContentLoaded", function () {
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
