// Shared offline/empty empty-state markup (single source of truth for the
// text + markup used by index.astro, services.astro, and service-detail.astro).
import { escapeHtml } from "./api";

/**
 * Offline empty-state card: primary message + "start russel-ctrl" hint +
 * Open Settings CTA. Used when the control plane is unreachable.
 */
export function renderOfflineState(message: string = "Control plane offline."): string {
	return `<div class="card-panel empty-state" style="text-align:center;padding:2rem;color:var(--text-muted)"><p>${escapeHtml(message)}</p><p style="font-size:0.82rem;color:var(--text-muted);margin-top:0.3rem">Start <code>russel-ctrl</code> (default :7878) or enable Demo mode in Settings.</p><a href="/settings" class="btn btn-secondary btn-sm" style="margin-top:0.75rem">Open Settings</a></div>`;
}

/**
 * Empty-fleet empty-state card: primary message + "Deploy your first service"
 * CTA. Used when the control plane is reachable but has no services.
 */
export function renderEmptyState(message: string = "No services configured."): string {
	return `<div class="card-panel empty-state" style="text-align:center;padding:2rem;color:var(--text-muted)"><p>${escapeHtml(message)}</p><a href="/deploy" class="btn btn-primary" style="margin-top:0.75rem">Deploy your first service</a></div>`;
}
