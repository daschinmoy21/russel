// Shared port-cell helpers (single source of truth for index.astro,
// services.astro, and ServicesTable.astro).
import { escapeHtml, isApiBaseLoopback } from "./api";
import type { ServiceVM } from "./api";

/** Resolve the published host port for a service (host_port wins, else ports). */
export function getHostPort(svc: ServiceVM): number | null {
	if (typeof svc.host_port === "number") return svc.host_port;
	const m = svc.ports?.match(/^\d+/);
	return m ? parseInt(m[0], 10) : null;
}

/**
 * Render the port cell for client-side tables (index/services hydrates).
 * Loopback API base links to `localhost:{port}`; remote bases show a static
 * `port N` label because the host port lives on the control-plane host (#283).
 */
export function renderPortCell(svc: ServiceVM): string {
	const hp = getHostPort(svc);
	if (!hp) return `<span class="mono-text">\u2014</span>`;
	const portsLabel = escapeHtml(svc.ports || "\u2014");
	if (isApiBaseLoopback()) {
		return `<div class="port-cell"><span class="mono-text">${portsLabel}</span><a class="port-link" href="http://localhost:${hp}" target="_blank" rel="noopener noreferrer">localhost:${hp}</a></div>`;
	}
	return `<div class="port-cell"><span class="mono-text">${portsLabel}</span><span class="port-link port-link-remote" title="Host port is on the remote control-plane host, not this browser">port ${hp}</span></div>`;
}
