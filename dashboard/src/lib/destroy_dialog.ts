// Promise wrapper around the shared DestroyDialog.astro modal.

/**
 * Show the destroy confirmation for `name`. Resolves true on confirm, false on
 * cancel, Escape, or a click on the backdrop. Falls back to window.confirm()
 * when the page does not render DestroyDialog.
 */
export function confirmDestroy(name: string): Promise<boolean> {
	const modal = document.getElementById("destroyModal");
	const btnConfirm = document.getElementById("btnConfirmDestroy");
	const btnCancel = document.getElementById("btnCancelDestroy");
	if (!modal || !btnConfirm || !btnCancel) {
		return Promise.resolve(
			window.confirm(`Destroy service ${name}? This action cannot be undone.`),
		);
	}
	const label = document.getElementById("destroyServiceName");
	if (label) label.textContent = name;

	return new Promise((resolve) => {
		const close = (ok: boolean) => {
			modal.classList.add("hidden");
			btnConfirm.removeEventListener("click", onConfirm);
			btnCancel.removeEventListener("click", onCancel);
			modal.removeEventListener("click", onBackdrop);
			document.removeEventListener("keydown", onKey);
			resolve(ok);
		};
		const onConfirm = () => close(true);
		const onCancel = () => close(false);
		const onBackdrop = (e: MouseEvent) => {
			if (e.target === modal) close(false);
		};
		const onKey = (e: KeyboardEvent) => {
			if (e.key === "Escape") close(false);
		};
		btnConfirm.addEventListener("click", onConfirm);
		btnCancel.addEventListener("click", onCancel);
		modal.addEventListener("click", onBackdrop);
		document.addEventListener("keydown", onKey);
		modal.classList.remove("hidden");
		btnCancel.focus();
	});
}
