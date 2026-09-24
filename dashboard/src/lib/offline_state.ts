// Shared connection and empty-state markup used by the dashboard pages.
import {
	classifyApiTopology,
	escapeHtml,
	type ApiTopologyInfo,
} from "./api";

/** Pure topology copy used by offline cards and tests. */
export function offlineTopologyMessage(
	apiBase: string,
	browserOrigin?: string,
): { topology: ApiTopologyInfo; message: string } {
	const topology = classifyApiTopology(apiBase, browserOrigin);
	let message: string;
	switch (topology.kind) {
		case "relative-loopback":
			message =
				`No control plane is running on this browser host (${topology.host}). For a VPS, open the SSH tunnel with ./contrib/install.sh connect <user@host> and retry. For local ctrl, start the user unit.`;
			break;
		case "relative-same-origin":
			message =
				`The same-origin reverse proxy should map /api/ to russel-ctrl (${topology.host}). Check the proxy. Do not start russel-ctrl on the laptop.`;
			break;
		case "absolute-loopback":
			message =
				`Only a local control plane or SSH tunnel can serve ${topology.host}. Start the local user unit or open the tunnel, then retry.`;
			break;
		case "absolute-remote-https":
			message =
				`The browser cannot reach remote API host ${topology.host} directly. Check the reverse proxy. Do not start russel-ctrl on the laptop.`;
			break;
		case "absolute-remote-http":
			message =
				`The remote API host ${topology.host} uses cleartext HTTP. Check the HTTPS reverse proxy. Do not start russel-ctrl on the laptop.`;
			break;
		case "invalid":
		default:
			message =
				"The configured API is not reachable. Check the API base in Settings.";
	}
	return { topology, message };
}

function settingsCta(): string {
	return `<a href="/settings" class="btn btn-secondary btn-sm" style="margin-top:0.75rem">Open Settings</a>`;
}

/** Offline card with topology-specific guidance and a Demo mode CTA. */
export function renderOfflineState(
	message = "Control plane offline.",
	apiBase = "/api",
	browserOrigin?: string,
): string {
	const guidance = offlineTopologyMessage(apiBase, browserOrigin);
	return `<div class="card-panel empty-state" style="text-align:center;padding:2rem;color:var(--text-muted)"><p>${escapeHtml(message)}</p><p style="font-size:0.82rem;color:var(--text-muted);margin-top:0.3rem">${escapeHtml(guidance.message)}</p><p style="font-size:0.82rem;color:var(--text-muted);margin-top:0.3rem">Open Settings to update the connection or enable Demo mode.</p>${settingsCta()}</div>`;
}

/** Unauthorized card. It intentionally contains no offline or tunnel advice. */
export function renderUnauthorizedState(
	message = "API token missing or wrong.",
): string {
	return `<div class="card-panel empty-state" style="text-align:center;padding:2rem;color:var(--text-muted)"><p>${escapeHtml(message)}</p><p style="font-size:0.82rem;color:var(--text-muted);margin-top:0.3rem">Update the bearer token in Settings, or enable Demo mode there.</p>${settingsCta()}</div>`;
}

/** Render either connection failure card while keeping page consumers small. */
export function renderConnectionState(
	connection: "offline" | "unauthorized",
	message: string,
	apiBase = "/api",
	browserOrigin?: string,
): string {
	return connection === "unauthorized"
		? renderUnauthorizedState(message)
		: renderOfflineState(message, apiBase, browserOrigin);
}

/** Empty-fleet card for a reachable control plane. */
export function renderEmptyState(message = "No services configured."): string {
	return `<div class="card-panel empty-state" style="text-align:center;padding:2rem;color:var(--text-muted)"><p>${escapeHtml(message)}</p><a href="/deploy" class="btn btn-primary" style="margin-top:0.75rem">Deploy your first service</a></div>`;
}
