// ---- Real API types (mirroring crates/core/src/api.rs) ----

export interface PortMapping {
	host: number;
	guest: number;
}

export interface DeployRequest {
	repo_url: string;
	config_path: string;
	/** Optional check; must equal the Russelfile service.name. */
	vm_id?: string;
}

export interface DeployTiming {
	resolve_ms: number;
	build_ms: number;
	create_ms: number;
	start_ms: number;
	network_ms: number;
	ready_ms: number;
}

export interface DeployResponse {
	service_id: string;
	vm_id: string;
	status: string;
	store_path?: string;
	microvm_config_path?: string;
	port?: PortMapping;
	elapsed_ms: number;
	message: string;
	timing?: DeployTiming;
	vm_ip?: string;
	runtime?: "microvm" | "container";
	route_host?: string;
	backend_port?: number;
}

export type DeployEvent =
	| { type: "Progress"; payload: { phase: string; description: string } }
	| { type: "Complete"; payload: DeployResponse }
	| { type: "Error"; payload: string };

export interface ServiceSummary {
	service_id: string;
	runtime?: "microvm" | "container";
	status: string;
}

export interface VmsResponse {
	vms: string[];
	services: ServiceSummary[];
}

export interface StatusResponse {
	service_id: string;
	status: string;
	vm_state: string;
	uptime_seconds: number;
	runtime?: "microvm" | "container";
	host_port?: number;
	guest_port?: number;
	route_host?: string | null;
}

export interface LogsResponse {
	output: string;
}

export interface DeploymentRecord {
	version: number;
	generation_id?: string;
	status: string; // active | previous | superseded | failed | rolled_back
	runtime?: "microvm" | "container";
	deployed_at: string;
	store_path?: string;
	repo_url?: string;
	config_path?: string;
	host_port?: number;
	guest_port?: number;
	message?: string;
	rollback_ready?: boolean;
}

export interface DeploymentsResponse {
	service_id: string;
	active_version?: number;
	deployments: DeploymentRecord[];
}

// ---- UI model (flattened for components) ----

export interface ServiceVM {
	id: string;
	name?: string;
	runtime: "microvm" | "container";
	status: string;
	vm_state?: string;
	uptime_seconds?: number;
	host_port?: number;
	guest_port?: number;
	ports?: string; // "host:guest" display string
	route_host?: string | null;
}

export interface FleetStatus {
	online: boolean;
	total_vms: number;
	running_vms: number;
	stopped_vms: number;
	failed_vms: number;
	api_latency_ms: number;
	max_uptime_seconds: number;
}

/** True only when the workload process is live — not generation/journal "active". */
export function isServiceRunning(svc: {
	status?: string;
	vm_state?: string;
}): boolean {
	if (svc.vm_state === "running") return true;
	if (
		svc.vm_state === "none" ||
		svc.vm_state === "stopped" ||
		svc.vm_state === "failed" ||
		svc.vm_state === "orphaned" ||
		svc.vm_state === "pending"
	) {
		return false;
	}
	// Fallback when vm_state is missing (list summary only).
	return svc.status === "deployed" || svc.status === "running";
}

export function isServiceFailed(svc: {
	status?: string;
	vm_state?: string;
}): boolean {
	return svc.vm_state === "failed" || svc.status === "failed";
}

// ---- Connection state ----

export type ConnectionState = "live" | "demo" | "offline" | "unauthorized";

export type ConnectionResultKind =
	| "success"
	| "unauthorized"
	| "http_error"
	| "network"
	| "cleartext";

export type ConnectionResult =
	| { kind: "success"; response: Response }
	| { kind: "unauthorized"; status: 401; response: Response }
	| { kind: "http_error"; status: number; response: Response }
	| { kind: "network"; error: unknown }
	| { kind: "cleartext"; message: string; error: unknown };

/**
 * Classify a fetch response or a fetch rejection without consulting browser
 * state. HTTP 401 is deliberately separate from transport failure.
 */
export function classifyConnectionResult(value: unknown): ConnectionResult {
	if (
		value !== null &&
		typeof value === "object" &&
		"ok" in value &&
		"status" in value &&
		typeof (value as { ok?: unknown }).ok === "boolean" &&
		typeof (value as { status?: unknown }).status === "number"
	) {
		const response = value as Response;
		if (response.ok) return { kind: "success", response };
		if (response.status === 401) {
			return { kind: "unauthorized", status: 401, response };
		}
		return { kind: "http_error", status: response.status, response };
	}

	if (isCleartextPolicyError(value)) {
		return {
			kind: "cleartext",
			message: errorMessage(value),
			error: value,
		};
	}
	return { kind: "network", error: value };
}

function errorMessage(error: unknown): string {
	return error instanceof Error
		? error.message
		: typeof error === "string"
			? error
			: "network unreachable";
}

/** Error name used by the cleartext Bearer policy below. */
export class CleartextTokenError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "CleartextTokenError";
	}
}

export function isCleartextPolicyError(error: unknown): boolean {
	if (error instanceof CleartextTokenError) return true;
	if (!error || typeof error !== "object") return false;
	const candidate = error as { name?: unknown; message?: unknown };
	return (
		candidate.name === "CleartextTokenError" ||
		(typeof candidate.message === "string" &&
			/refusing to send api token over cleartext http/i.test(candidate.message))
	);
}

export type ApiTopology =
	| "relative-loopback"
	| "relative-same-origin"
	| "absolute-loopback"
	| "absolute-remote-https"
	| "absolute-remote-http"
	| "invalid";

export interface ApiTopologyInfo {
	kind: ApiTopology;
	host: string;
}

/** Purely classify an API base against the browser origin that will call it. */
export function classifyApiTopology(
	apiBase: string,
	browserOrigin = "http://127.0.0.1:4321",
): ApiTopologyInfo {
	const rawBase = apiBase.trim();
	if (!/^https?:\/\//i.test(rawBase)) {
		try {
			const origin = new URL(browserOrigin);
			return {
				kind: isLoopbackHost(origin.hostname)
					? "relative-loopback"
					: "relative-same-origin",
				host: origin.host || origin.hostname,
			};
		} catch {
			return { kind: "invalid", host: rawBase || "the configured API" };
		}
	}

	try {
		const url = new URL(rawBase);
		if (isLoopbackHost(url.hostname)) {
			return {
				kind: "absolute-loopback",
				host: url.host || url.hostname,
			};
		}
		return {
			kind: url.protocol === "https:" ? "absolute-remote-https" : "absolute-remote-http",
			host: url.host || url.hostname,
		};
	} catch {
		return { kind: "invalid", host: rawBase || "the configured API" };
	}
}

/** Pure, user-facing guidance for a failed connection attempt. */
export function connectionResultMessage(
	result: ConnectionResult,
	apiBase = "/api",
	browserOrigin = "http://127.0.0.1:4321",
): string {
	switch (result.kind) {
		case "success":
			return "Connected.";
		case "unauthorized":
			return "API token missing or wrong. Open Settings to update it.";
		case "http_error":
			return `Connection failed: HTTP ${result.status}.`;
		case "cleartext":
			return result.message;
		case "network": {
			const topology = classifyApiTopology(apiBase, browserOrigin);
			switch (topology.kind) {
				case "relative-loopback":
					return `No control plane is reachable on this browser host (${topology.host}). For a VPS, open the SSH tunnel with ./contrib/install.sh connect <user@host> and retry. For local ctrl, start the user unit.`;
				case "relative-same-origin":
					return `The same-origin reverse proxy for /api/ is not reachable (${topology.host}). Check the proxy. Do not start russel-ctrl on the laptop.`;
				case "absolute-loopback":
					return `Nothing is listening on local API host ${topology.host}. A local ctrl or SSH tunnel must be up before retrying.`;
				case "absolute-remote-https":
					return `The remote HTTPS API host ${topology.host} is not reachable from this dashboard. Check the reverse proxy. Do not start russel-ctrl on the laptop.`;
				case "absolute-remote-http":
					return `The remote API host ${topology.host} uses cleartext HTTP. Check the HTTPS reverse proxy. Do not start russel-ctrl on the laptop.`;
				case "invalid":
				default:
					return "The configured API is not reachable. Check the API base in Settings.";
			}
		}
	}
}

/** Header may also show pre-probe state for static shells (#316). */
export type HeaderConnectionState = ConnectionState | "loading";

export function getConnectionLabel(c: ConnectionState): string {
	switch (c) {
		case "live":
			return "Live";
		case "demo":
			return "Demo";
		case "offline":
			return "Offline";
		case "unauthorized":
			return "Unauthorized";
	}
}

/**
 * Resolve the connection badge for static shells.
 * Omitted connection defaults to `loading` (not LIVE) so prerender never claims
 * a verified control-plane link before the browser probe completes.
 */
export function resolveHeaderConnection(
	connection?: HeaderConnectionState,
	isDemo = false,
): HeaderConnectionState {
	if (connection) return connection;
	if (isDemo) return "demo";
	return "loading";
}

export function headerBadgeMeta(
	state: HeaderConnectionState,
	apiBase = "/api",
	browserOrigin = "http://127.0.0.1:4321",
): {
	label: string;
	className: string;
	title: string;
} {
	switch (state) {
		case "live":
			return {
				label: "LIVE",
				className: "badge-online",
				title: "Connected to russel-ctrl",
			};
		case "demo":
			return {
				label: "DEMO",
				className: "badge-demo",
				title: "Demo mode — mock data (Settings)",
			};
		case "offline":
			return {
				label: "OFFLINE",
				className: "badge-offline",
				title:
					classifyApiTopology(apiBase, browserOrigin).kind === "relative-loopback"
						? "Control plane unreachable — tunnel? Open the SSH tunnel or start the local user unit"
						: "Control plane unreachable — check the API topology or enable Demo mode",
			};
		case "unauthorized":
			return {
				label: "UNAUTHORIZED",
				className: "badge-warning",
				title: "API token missing or wrong — open Settings",
			};
		case "loading":
			return {
				label: "…",
				className: "badge-muted",
				title: "Checking control-plane connection…",
			};
	}
}

/** Zeroed fleet snapshot used by prerendered overview shell (no build-time fetch). */
export const STATIC_FLEET_STATUS: FleetStatus = {
	online: false,
	total_vms: 0,
	running_vms: 0,
	stopped_vms: 0,
	failed_vms: 0,
	api_latency_ms: 0,
	max_uptime_seconds: 0,
};

export type OverviewMetricsCard = {
	title: string;
	value: string;
	subtext: string;
};

/**
 * Pure metrics for overview hydrate states (live/demo/offline/unauthorized/loading shell).
 * Used by client hydrate and unit tests so shell contracts stay locked.
 */
export function overviewMetricsForConnection(
	connection: ConnectionState | "loading",
	status: FleetStatus,
	services: ServiceVM[],
): OverviewMetricsCard[] | null {
	if (connection === "loading") {
		return [
			{
				title: "Fleet Health",
				value: "—",
				subtext: "Loading…",
			},
			{
				title: "Longest Uptime",
				value: "—",
				subtext: "Across all services",
			},
			{
				title: "API Latency",
				value: "—",
				subtext: "Loading…",
			},
			{
				title: "Services",
				value: "—",
				subtext: "Loading…",
			},
		];
	}
	if (connection === "offline") {
		return [
			{
				title: "Fleet Health",
				value: "0 / 0",
				subtext: "Control plane offline",
			},
			{
				title: "Longest Uptime",
				value: "—",
				subtext: "Across all services",
			},
			{
				title: "API Latency",
				value: "Offline",
				subtext: "Unreachable",
			},
			{
				title: "Services",
				value: "0",
				subtext: "0 running",
			},
		];
	}
	if (connection === "unauthorized") {
		return [
			{
				title: "Fleet Health",
				value: "—",
				subtext: "API token missing or wrong",
			},
			{
				title: "Longest Uptime",
				value: "—",
				subtext: "Authorization required",
			},
			{
				title: "API Latency",
				value: "—",
				subtext: "Open Settings",
			},
			{
				title: "Services",
				value: "—",
				subtext: "Authorization required",
			},
		];
	}
	// live | demo
	const active = services.filter((s) => isServiceRunning(s)).length;
	const uptime =
		status.max_uptime_seconds > 0
			? formatUptime(status.max_uptime_seconds)
			: "—";
	const latency = status.online ? `${status.api_latency_ms} ms` : "Offline";
	return [
		{
			title: "Fleet Health",
			value: `${status.running_vms} / ${status.total_vms}`,
			subtext: status.online
				? "All systems operational"
				: "API unreachable",
		},
		{
			title: "Longest Uptime",
			value: uptime,
			subtext: "Across all services",
		},
		{
			title: "API Latency",
			value: latency,
			subtext: status.online ? "Control plane reachable" : "Unreachable",
		},
		{
			title: "Services",
			value: String(services.length),
			subtext: `${active} running`,
		},
	];
}

/** Empty-state copy under Active Fleet Status after hydrate. */
export function overviewServicesEmptyCopy(
	connection: ConnectionState,
): { primary: string; kind: "offline" | "unauthorized" | "empty" } {
	if (connection === "offline") {
		return { primary: "Control plane offline.", kind: "offline" };
	}
	if (connection === "unauthorized") {
		return { primary: "API token missing or wrong.", kind: "unauthorized" };
	}
	return { primary: "No services configured.", kind: "empty" };
}

// ---- Demo mode / settings ----

const TOKEN_KEY = "RUSSEL_API_TOKEN";

/** Escape text for safe interpolation into HTML (text or quoted attributes). */
export function escapeHtml(s: string): string {
	return s
		.replace(/&/g, "&amp;")
		.replace(/</g, "&lt;")
		.replace(/>/g, "&gt;")
		.replace(/"/g, "&quot;")
		.replace(/'/g, "&#39;");
}

let warnedPublicToken = false;

/**
 * Never bake secrets into the client bundle via PUBLIC_* env vars.
 * If someone still sets PUBLIC_RUSSEL_API_TOKEN at build time, ignore it
 * and warn once — tokens must be entered at runtime (Settings UI).
 */
function warnPublicTokenOnce(): void {
	if (warnedPublicToken) return;
	warnedPublicToken = true;
	try {
		const baked = (import.meta as any).env?.PUBLIC_RUSSEL_API_TOKEN;
		if (baked) {
			console.warn(
				"[russel] PUBLIC_RUSSEL_API_TOKEN is set but ignored. " +
					"Do not bake API tokens into the client bundle; enter the token in Settings instead.",
			);
		}
	} catch {
		/* import.meta.env may be unavailable in some contexts */
	}
}

export function isDemoMode(): boolean {
	if (typeof window === "undefined") return false;
	return localStorage.getItem("RUSSEL_DEMO_MODE") === "1";
}

export function setDemoMode(on: boolean): void {
	if (typeof window === "undefined") return;
	localStorage.setItem("RUSSEL_DEMO_MODE", on ? "1" : "0");
}

/**
 * Join an API base with a path without producing protocol-relative URLs.
 * Root base (`""` or `"/"`) yields a same-origin absolute path (e.g. `/vms`).
 */
export function joinApiUrl(base: string, path: string): string {
	const p = path.startsWith("/") ? path : `/${path}`;
	if (!base || base === "/") return p;
	return `${base.replace(/\/+$/, "")}${p}`;
}

/**
 * Validate and normalize an API base URL.
 * Returns the normalized base on success, or `null` when invalid.
 *
 * Allowed:
 * - Relative path starting with `/` (e.g. `/api`) — not protocol-relative `//`
 * - Root `/` normalizes to `""` (same-origin root; join via `joinApiUrl`)
 * - Absolute `http://` or `https://` URL without credentials in userinfo
 *
 * Rejected: empty, `javascript:`, `data:`, credentials (`user:pass@`),
 * query (`?`) / fragment (`#`), bare host without scheme, whitespace garbage.
 */
export function validateApiBase(url: string): string | null {
	const raw = url.trim();
	if (!raw) return null;

	// Relative path: must start with single `/`, not `//` (protocol-relative)
	if (raw.startsWith("/")) {
		if (raw.startsWith("//")) return null;
		if (raw.includes("?") || raw.includes("#")) return null;
		if (/[\s<>"'`\\]/.test(raw)) return null;
		// Root → empty string so joinApiUrl("/", "/vms") never becomes "//vms"
		if (raw === "/") return "";
		return raw.replace(/\/+$/, "");
	}

	// Reject known-dangerous schemes before URL parse (parse may accept them)
	const schemeMatch = raw.match(/^([a-zA-Z][a-zA-Z0-9+.-]*):/);
	if (schemeMatch) {
		const scheme = schemeMatch[1].toLowerCase();
		if (scheme !== "http" && scheme !== "https") return null;
	}

	let parsed: URL;
	try {
		parsed = new URL(raw);
	} catch {
		return null;
	}

	const protocol = parsed.protocol.toLowerCase();
	if (protocol !== "http:" && protocol !== "https:") return null;

	// Reject credentials in userinfo (user:pass@host)
	if (parsed.username || parsed.password) return null;
	if (!parsed.hostname) return null;
	// Reject query and fragment on API base
	if (parsed.search || parsed.hash) return null;

	let path = parsed.pathname || "";
	if (path !== "/" && path.endsWith("/")) {
		path = path.replace(/\/+$/, "");
	}
	if (path === "/") path = "";
	return `${parsed.origin}${path}`;
}

/** Human-readable reason when `validateApiBase` returns null. */
export function apiBaseValidationError(url: string): string {
	const raw = url.trim();
	if (!raw) return "API base URL is required.";
	if (raw.startsWith("//")) {
		return "Protocol-relative URLs are not allowed; use http(s):// or a path starting with /.";
	}
	if (raw.startsWith("/")) {
		if (raw.includes("?") || raw.includes("#")) {
			return "API base path must not include a query string or fragment.";
		}
		if (/[\s<>"'`\\]/.test(raw)) {
			return "API base path contains invalid characters.";
		}
		return "Invalid API base path.";
	}
	const schemeMatch = raw.match(/^([a-zA-Z][a-zA-Z0-9+.-]*):/);
	if (schemeMatch) {
		const scheme = schemeMatch[1].toLowerCase();
		if (scheme !== "http" && scheme !== "https") {
			return `Unsupported URL scheme "${schemeMatch[1]}:". Use http://, https://, or a relative path starting with /.`;
		}
	}
	try {
		const parsed = new URL(raw);
		if (parsed.username || parsed.password) {
			return "API base URL must not include credentials (user:pass@). Use the token field instead.";
		}
		if (!parsed.hostname) return "API base URL is missing a hostname.";
		if (parsed.search || parsed.hash) {
			return "API base URL must not include a query string or fragment.";
		}
	} catch {
		/* fall through */
	}
	return "Invalid API base URL. Use a path like /api or an http(s):// URL.";
}

export function getApiBase(): string {
	if (typeof window !== "undefined") {
		// null = unset; "" = same-origin root (valid normalized base)
		const stored = localStorage.getItem("RUSSEL_API_URL");
		if (stored !== null) {
			if (stored === "") return "";
			const validated = validateApiBase(stored);
			// null = invalid; "" = valid root — do not use truthiness
			if (validated !== null) return validated;
			// Invalid stored value — fall back rather than using garbage
			return "/api";
		}
		return "/api";
	}
	return (import.meta as any).env?.PUBLIC_RUSSEL_API || "/api";
}

/**
 * Bearer token for the control plane.
 * Stored in sessionStorage (tab-scoped, cleared when the tab closes) so XSS
 * in a later session cannot read a long-lived localStorage secret.
 * Never reads PUBLIC_RUSSEL_API_TOKEN — that would ship the secret in the JS bundle.
 */
export function getApiToken(): string | null {
	warnPublicTokenOnce();
	if (typeof window === "undefined") return null;

	let token = sessionStorage.getItem(TOKEN_KEY);
	if (token) {
		// Remove any stale pre-session copy even when a session token already exists.
		localStorage.removeItem(TOKEN_KEY);
		return token;
	}
	if (!token) {
		// One-time migrate from pre-#198 localStorage storage
		const legacy = localStorage.getItem(TOKEN_KEY);
		if (legacy) {
			sessionStorage.setItem(TOKEN_KEY, legacy);
			localStorage.removeItem(TOKEN_KEY);
			token = legacy;
		}
	}
	return token || null;
}

/**
 * Persist API base after validation. Throws Error with a user-facing message
 * when the URL is invalid so Settings can show the toast without a separate check.
 */
export function setApiBase(url: string): void {
	if (typeof window === "undefined") return;
	const normalized = validateApiBase(url);
	// "" is a valid same-origin root — only null is rejection
	if (normalized === null) {
		throw new Error(apiBaseValidationError(url));
	}
	localStorage.setItem("RUSSEL_API_URL", normalized);
}

export function setApiToken(token: string): void {
	if (typeof window === "undefined") return;
	if (token) {
		sessionStorage.setItem(TOKEN_KEY, token);
	} else {
		sessionStorage.removeItem(TOKEN_KEY);
	}
	// Drop any legacy long-lived copy
	localStorage.removeItem(TOKEN_KEY);
}

// ---- Cleartext Bearer policy (#189) ----

/** True for localhost / 127.0.0.0/8 / ::1. */
export function isLoopbackHost(host: string): boolean {
	const h = host.replace(/^\[|\]$/g, "").toLowerCase();
	if (h === "localhost" || h === "::1") return true;
	if (h.startsWith("127.")) {
		const parts = h.split(".");
		return (
			parts.length === 4 &&
			parts.every((p) => {
				const n = Number(p);
				return Number.isInteger(n) && n >= 0 && n <= 255;
			})
		);
	}
	return false;
}

/**
 * Refuse to attach a Bearer token when the API base is plain `http://` to a
 * non-loopback host. Relative bases (`/api`) resolve against `window.location`.
 * Escape hatch: `localStorage.RUSSEL_INSECURE_CLEARTEXT = "1"`.
 */
export function assertCleartextTokenOk(
	apiBase: string,
	token: string | null,
): void {
	if (!token || !token.trim()) return;
	if (
		typeof window !== "undefined" &&
		localStorage.getItem("RUSSEL_INSECURE_CLEARTEXT") === "1"
	) {
		return;
	}
	let url: URL;
	try {
		if (/^https?:\/\//i.test(apiBase)) {
			url = new URL(apiBase);
		} else if (typeof window !== "undefined") {
			url = new URL(apiBase || "/", window.location.href);
		} else {
			return; // SSR / no origin — nothing to send yet
		}
	} catch {
		return;
	}
	if (url.protocol !== "http:") return;
	const host = url.hostname;
	if (isLoopbackHost(host)) return;
	throw new CleartextTokenError(
		`Refusing to send API token over cleartext HTTP to non-loopback host "${host}". ` +
			`Use HTTPS (reverse proxy in front of russel-ctrl; see docs/security-tls.md), ` +
			`a loopback API URL, or set localStorage RUSSEL_INSECURE_CLEARTEXT=1.`,
	);
}

// ---- Helpers ----

/** `word` for a count of 1, otherwise `word` + "s" ("1 microVM", "0 microVMs"). */
export function plural(count: number, word: string): string {
	return count === 1 ? word : `${word}s`;
}

export function formatUptime(seconds?: number): string {
	if (!seconds || seconds <= 0) return "—";
	const d = Math.floor(seconds / 86400);
	const h = Math.floor((seconds % 86400) / 3600);
	const m = Math.floor((seconds % 3600) / 60);
	if (d > 0) return `${d}d ${h}h`;
	if (h > 0) return `${h}h ${m}m`;
	return `${m}m ${seconds % 60}s`;
}

/** Align with CLI: `"deployed"`, or `"unchanged"` when the service already runs this source. */
export function isDeployStatusSuccess(status: string): boolean {
	return status === "deployed" || status === "unchanged";
}

/**
 * Parse one NDJSON line into a DeployEvent.
 * Accepts tagged events (`{type,payload}`) and a bare DeployResponse shape.
 */
export function parseDeployEventLine(line: string): DeployEvent | null {
	const trimmed = line.trim();
	if (!trimmed) return null;
	try {
		const parsed = JSON.parse(trimmed);
		if (parsed?.type === "Progress") {
			const p = parsed.payload ?? parsed;
			return {
				type: "Progress",
				payload: {
					phase: String(p.phase ?? "unknown"),
					description: String(p.description ?? p.message ?? ""),
				},
			};
		}
		if (parsed?.type === "Complete") {
			const p = parsed.payload ?? parsed;
			return { type: "Complete", payload: p as DeployResponse };
		}
		if (parsed?.type === "Error") {
			const p = parsed.payload;
			const msg =
				typeof p === "string"
					? p
					: p?.message || parsed.message || trimmed;
			return { type: "Error", payload: String(msg) };
		}
		// Bare DeployResponse (no type tag)
		if (parsed?.service_id && parsed?.status) {
			return { type: "Complete", payload: parsed as DeployResponse };
		}
		if (parsed?.phase) {
			return {
				type: "Progress",
				payload: {
					phase: String(parsed.phase),
					description: String(parsed.description || parsed.message || ""),
				},
			};
		}
		return {
			type: "Progress",
			payload: { phase: "raw", description: trimmed },
		};
	} catch {
		return {
			type: "Progress",
			payload: { phase: "raw", description: trimmed },
		};
	}
}

/**
 * Stream a response body as NDJSON: call onLine for each complete line
 * (including a final trailing line without newline).
 */
export async function consumeNdjsonStream(
	body: ReadableStream<Uint8Array>,
	onLine: (line: string) => void,
): Promise<void> {
	const reader = body.getReader();
	const decoder = new TextDecoder();
	let buffer = "";
	try {
		while (true) {
			const { done, value } = await reader.read();
			if (done) break;
			buffer += decoder.decode(value, { stream: true });
			let nl: number;
			while ((nl = buffer.indexOf("\n")) >= 0) {
				const line = buffer.slice(0, nl);
				buffer = buffer.slice(nl + 1);
				if (line.trim()) onLine(line);
			}
		}
		buffer += decoder.decode();
		if (buffer.trim()) onLine(buffer);
	} finally {
		reader.releaseLock();
	}
}

/** Result of consuming a deploy/update NDJSON event stream. */
export interface NdjsonDeployOutcome {
	success: boolean;
	sawError: boolean;
	complete: DeployResponse | null;
	errorMessage: string | null;
}

/**
 * Feed NDJSON lines through parseDeployEventLine, invoke onEvent, and decide
 * success only when Complete has a success status and no Error events.
 */
export function reduceDeployEvents(
	lines: Iterable<string>,
	onEvent?: (event: DeployEvent) => void,
): NdjsonDeployOutcome {
	let sawError = false;
	let complete: DeployResponse | null = null;
	let errorMessage: string | null = null;

	for (const line of lines) {
		const event = parseDeployEventLine(line);
		if (!event) continue;
		onEvent?.(event);
		if (event.type === "Error") {
			sawError = true;
			errorMessage = event.payload;
		} else if (event.type === "Complete") {
			complete = event.payload;
		}
	}

	const success =
		!sawError &&
		complete != null &&
		isDeployStatusSuccess(complete.status);

	return { success, sawError, complete, errorMessage };
}

/**
 * Resolve whether the configured API base points at a loopback host.
 * Relative bases (`/api`) resolve against `window.location` (the operator browser).
 */
export function isApiBaseLoopback(apiBase: string = getApiBase()): boolean {
	try {
		let url: URL;
		if (/^https?:\/\//i.test(apiBase)) {
			url = new URL(apiBase);
		} else if (typeof window !== "undefined") {
			url = new URL(apiBase || "/", window.location.href);
		} else {
			return false;
		}
		return isLoopbackHost(url.hostname);
	} catch {
		return false;
	}
}

/**
 * Probe a host port via no-cors fetch against 127.0.0.1.
 * Only runs when the API base host is loopback — otherwise the service port
 * lives on a remote machine, not the operator browser's localhost (#283).
 * Returns `{ up: null, ms: null, skipped: true }` when probing is not applicable.
 */
export async function probeEndpointPort(
	hostPort: number,
): Promise<{ up: boolean | null; ms: number | null; skipped?: boolean }> {
	if (!isApiBaseLoopback()) {
		return { up: null, ms: null, skipped: true };
	}
	const url = `http://127.0.0.1:${hostPort}/`;
	const t0 = performance.now();
	try {
		await fetch(url, {
			mode: "no-cors",
			cache: "no-store",
			signal: AbortSignal.timeout(2000),
		});
		return { up: true, ms: Math.round(performance.now() - t0) };
	} catch {
		return { up: false, ms: null };
	}
}

function portDisplay(host?: number, guest?: number): string {
	if (host != null && guest != null) return `${host}:${guest}`;
	if (host != null) return `${host}`;
	if (guest != null) return `:${guest}`;
	return "—";
}

// ---- Mock data (demo mode only) ----

const MOCK_DEPLOYMENTS: DeploymentRecord[] = [
	{
		version: 3,
		generation_id: "gen-c3d2e1",
		status: "active",
		runtime: "container",
		deployed_at: new Date(Date.now() - 3600_000).toISOString(),
		store_path: "/var/lib/russel/vms/vm-gw-89a1",
		repo_url: "https://github.com/org/api-gateway",
		config_path: "russel.toml",
		host_port: 8080,
		guest_port: 80,
		message: "Zero-downtime cutover: container gen promoted via ingress swap",
	},
	{
		version: 2,
		generation_id: "gen-b2a1f0",
		status: "previous",
		runtime: "microvm",
		deployed_at: new Date(Date.now() - 86_400_000).toISOString(),
		store_path: "/var/lib/russel/vms/vm-gw-89a1-v2",
		repo_url: "https://github.com/org/api-gateway",
		config_path: "russel.toml",
		host_port: 8080,
		guest_port: 80,
		message: "Dual-live microVM generation (previous)",
		rollback_ready: true,
	},
	{
		version: 1,
		generation_id: "gen-a0b9c8",
		status: "superseded",
		runtime: "container",
		deployed_at: new Date(Date.now() - 604_800_000).toISOString(),
		store_path: "/var/lib/russel/vms/vm-gw-89a1-v1",
		repo_url: "https://github.com/org/api-gateway",
		config_path: "russel.toml",
		host_port: 8080,
		guest_port: 80,
		message: "Initial container deploy",
	},
];

const MOCK_SERVICES: ServiceVM[] = [
	{
		id: "vm-gw-89a1",
		name: "api-gateway",
		runtime: "microvm",
		status: "deployed",
		vm_state: "running",
		uptime_seconds: 367200,
		host_port: 8080,
		guest_port: 80,
		ports: "8080:80",
	},
	{
		id: "vm-auth-44b2",
		name: "auth-service",
		runtime: "container",
		status: "deployed",
		vm_state: "running",
		uptime_seconds: 172800,
		host_port: 8081,
		guest_port: 8080,
		ports: "8081:8080",
	},
	{
		id: "vm-db-11c9",
		name: "postgres-primary",
		runtime: "container",
		status: "deployed",
		vm_state: "running",
		uptime_seconds: 864000,
		host_port: 5432,
		guest_port: 5432,
		ports: "5432:5432",
	},
	{
		id: "vm-wrk-99x5",
		name: "async-worker-pool",
		runtime: "microvm",
		status: "stopped",
		vm_state: "stopped",
		uptime_seconds: 0,
		ports: "—",
	},
	{
		id: "vm-rd-33d7",
		name: "redis-cache",
		runtime: "container",
		status: "deployed",
		vm_state: "running",
		uptime_seconds: 259200,
		host_port: 6379,
		guest_port: 6379,
		ports: "6379:6379",
	},
];

// ---- Client ----

export class RusselClient {
	private getHeaders(): Record<string, string> {
		const headers: Record<string, string> = {
			"Content-Type": "application/json",
		};
		const token = getApiToken();
		// #189: refuse cleartext Bearer to non-loopback (relative /api uses page origin).
		assertCleartextTokenOk(getApiBase(), token);
		if (token) headers["Authorization"] = `Bearer ${token}`;
		return headers;
	}

	/** Run an API request through the shared response/error classifier. */
	private async request(
		path: string,
		init: RequestInit = {},
	): Promise<ConnectionResult> {
		try {
			const res = await fetch(joinApiUrl(getApiBase(), path), {
				...init,
				headers: {
					...this.getHeaders(),
					...(init.headers as Record<string, string> | undefined),
				},
			});
			return classifyConnectionResult(res);
		} catch (error) {
			return classifyConnectionResult(error);
		}
	}

	// Shared concurrency-4 batched status fetcher for getServices + getFleetStatus
	private async fetchStatuses(
		ids: string[],
	): Promise<{ statuses: Map<string, StatusResponse>; unauthorized: boolean }> {
		const map = new Map<string, StatusResponse>();
		let unauthorized = false;
		const concurrency = 4;
		for (let i = 0; i < ids.length; i += concurrency) {
			const batch = ids.slice(i, i + concurrency);
			const results = await Promise.allSettled(
				batch.map(async (id) => {
					const result = await this.request(
						`/vm/${encodeURIComponent(id)}/status`,
						{ signal: AbortSignal.timeout(3000) },
					);
					if (result.kind === "unauthorized") unauthorized = true;
					if (result.kind !== "success") return null;
					try {
						return (await result.response.json()) as StatusResponse;
					} catch {
						return null;
					}
				}),
			);
			for (const r of results) {
				if (r.status === "fulfilled" && r.value) {
					map.set(r.value.service_id, r.value);
				}
			}
		}
		return { statuses: map, unauthorized };
	}

	async getServices(): Promise<{
		services: ServiceVM[];
		connection: ConnectionState;
		isDemo: boolean;
	}> {
		if (isDemoMode())
			return { services: MOCK_SERVICES, connection: "demo", isDemo: true };
		const result = await this.request("/vms", {
			signal: AbortSignal.timeout(3000),
		});
		if (result.kind !== "success") {
			return {
				services: [],
				connection: result.kind === "unauthorized" ? "unauthorized" : "offline",
				isDemo: false,
			};
		}
		try {
			const data: VmsResponse = await result.response.json();
			const summaries = data.services || [];
			const statusResult = await this.fetchStatuses(
				summaries.map((s) => s.service_id),
			);
			if (statusResult.unauthorized) {
				return { services: [], connection: "unauthorized", isDemo: false };
			}
			const statuses = statusResult.statuses;
			const services: ServiceVM[] = summaries.map((s) => {
				const st = statuses.get(s.service_id);
				if (st) {
					return {
						id: s.service_id,
						runtime: st.runtime || s.runtime || "microvm",
						status: st.status,
						vm_state: st.vm_state,
						uptime_seconds: st.uptime_seconds,
						host_port: st.host_port,
						guest_port: st.guest_port,
						ports: portDisplay(st.host_port, st.guest_port),
						route_host: st.route_host ?? null,
					};
				}
				return {
					id: s.service_id,
					runtime: s.runtime || "microvm",
					status: s.status,
					ports: "—",
				};
			});
			return { services, connection: "live", isDemo: false };
		} catch {
			return { services: [], connection: "offline", isDemo: false };
		}
	}

	async getFleetStatus(): Promise<{
		status: FleetStatus;
		connection: ConnectionState;
		isDemo: boolean;
	}> {
		if (isDemoMode()) {
			return {
				status: {
					online: true,
					total_vms: MOCK_SERVICES.length,
					running_vms: MOCK_SERVICES.filter((s) => isServiceRunning(s)).length,
					stopped_vms: MOCK_SERVICES.filter(
						(s) => !isServiceRunning(s) && !isServiceFailed(s),
					).length,
					failed_vms: 0,
					api_latency_ms: 0,
					max_uptime_seconds: Math.max(
						...MOCK_SERVICES.map((s) => s.uptime_seconds || 0),
					),
				},
				connection: "demo",
				isDemo: true,
			};
		}

		const start = performance.now();
		const result = await this.request("/vms", {
			signal: AbortSignal.timeout(3000),
		});
		if (result.kind !== "success") {
			return {
				status: {
					online: false,
					total_vms: 0,
					running_vms: 0,
					stopped_vms: 0,
					failed_vms: 0,
					api_latency_ms: 0,
					max_uptime_seconds: 0,
				},
				connection: result.kind === "unauthorized" ? "unauthorized" : "offline",
				isDemo: false,
			};
		}
		try {
			const data: VmsResponse = await result.response.json();
			const latency = Math.round(performance.now() - start);

			const summaries = data.services || [];
			let running = 0;
			let stopped = 0;
			let failed = 0;
			let maxUptime = 0;

			// Fetch per-service status in parallel with concurrency ~4
			const statusResult = await this.fetchStatuses(
				summaries.map((s) => s.service_id),
			);
			if (statusResult.unauthorized) {
				return {
					status: {
						online: false,
						total_vms: 0,
						running_vms: 0,
						stopped_vms: 0,
						failed_vms: 0,
						api_latency_ms: 0,
						max_uptime_seconds: 0,
					},
					connection: "unauthorized",
					isDemo: false,
				};
			}
			const statuses = statusResult.statuses;

			for (const s of statuses.values()) {
				if (isServiceRunning(s)) running++;
				else if (isServiceFailed(s)) failed++;
				else stopped++;
				if (isServiceRunning(s) && s.uptime_seconds > maxUptime) {
					maxUptime = s.uptime_seconds;
				}
			}

			// Fallback: use summary statuses if no per-service statuses fetched
			if (statuses.size === 0) {
				for (const s of summaries) {
					if (isServiceRunning(s)) running++;
					else if (isServiceFailed(s)) failed++;
					else stopped++;
				}
			}

			return {
				status: {
					online: true,
					total_vms: summaries.length,
					running_vms: running,
					stopped_vms: stopped,
					failed_vms: failed,
					api_latency_ms: latency,
					max_uptime_seconds: maxUptime,
				},
				connection: "live",
				isDemo: false,
			};
		} catch {
			return {
				status: {
					online: false,
					total_vms: 0,
					running_vms: 0,
					stopped_vms: 0,
					failed_vms: 0,
					api_latency_ms: 0,
					max_uptime_seconds: 0,
				},
				connection: "offline",
				isDemo: false,
			};
		}
	}

	/**
	 * Cheap connection probe for the top bar: one `GET /vms`, no per-service
	 * status fan-out. `total` is set only when the fleet is reachable.
	 */
	async probeConnection(): Promise<{
		connection: ConnectionState;
		total?: number;
	}> {
		if (isDemoMode()) {
			return { connection: "demo", total: MOCK_SERVICES.length };
		}
		const result = await this.request("/vms", {
			signal: AbortSignal.timeout(3000),
		});
		if (result.kind !== "success") {
			return {
				connection: result.kind === "unauthorized" ? "unauthorized" : "offline",
			};
		}
		try {
			const data: VmsResponse = await result.response.json();
			return { connection: "live", total: (data.services || []).length };
		} catch {
			return { connection: "offline" };
		}
	}

	async getServiceDetail(id: string): Promise<{
		service: ServiceVM | null;
		connection: ConnectionState;
		isDemo: boolean;
	}> {
		if (isDemoMode()) {
			const found =
				MOCK_SERVICES.find((s) => s.id === id || s.name === id) ||
				MOCK_SERVICES[0];
			return { service: found, connection: "demo", isDemo: true };
		}
		const result = await this.request(
			`/vm/${encodeURIComponent(id)}/status`,
			{ signal: AbortSignal.timeout(3000) },
		);
		if (result.kind !== "success") {
			return {
				service: null,
				connection: result.kind === "unauthorized" ? "unauthorized" : "offline",
				isDemo: false,
			};
		}
		try {
			const data: StatusResponse = await result.response.json();
			const svc: ServiceVM = {
				id: data.service_id,
				runtime: data.runtime || "microvm",
				status: data.status,
				vm_state: data.vm_state,
				uptime_seconds: data.uptime_seconds,
				host_port: data.host_port,
				guest_port: data.guest_port,
				ports: portDisplay(data.host_port, data.guest_port),
				route_host: data.route_host ?? null,
			};
			return { service: svc, connection: "live", isDemo: false };
		} catch {
			return { service: null, connection: "offline", isDemo: false };
		}
	}

	async getDeployments(id: string): Promise<{
		data: DeploymentsResponse | null;
		connection: ConnectionState;
		supported: boolean;
	}> {
		if (isDemoMode()) {
			// Return mock history for any known mock service id
			const known = MOCK_SERVICES.some((s) => s.id === id || s.name === id);
			if (known) {
				return {
					data: {
						service_id: id,
						active_version: 3,
						deployments: MOCK_DEPLOYMENTS,
					},
					connection: "demo",
					supported: true,
				};
			}
			return { data: null, connection: "demo", supported: false };
		}
		const result = await this.request(
			`/vm/${encodeURIComponent(id)}/deployments`,
			{ signal: AbortSignal.timeout(5000) },
		);
		if (result.kind === "http_error" && result.status === 404) {
				return { data: null, connection: "live", supported: false };
		}
		if (result.kind !== "success") {
			return {
				data: null,
				connection: result.kind === "unauthorized" ? "unauthorized" : "offline",
				supported: false,
			};
		}
		try {
			const data: DeploymentsResponse = await result.response.json();
			return { data, connection: "live", supported: true };
		} catch {
			return { data: null, connection: "offline", supported: false };
		}
	}

	async rollbackService(
		id: string,
		version?: number,
	): Promise<{ success: boolean; message: string; supported: boolean }> {
		if (isDemoMode()) {
			return {
				success: true,
				message: `[Demo] Rolled back service ${id} to version ${version || "previous"}.`,
				supported: true,
			};
		}
		try {
			const body = version != null ? JSON.stringify({ version }) : "{}";
			const res = await fetch(
				joinApiUrl(getApiBase(), `/vm/${encodeURIComponent(id)}/rollback`),
				{
					method: "POST",
					headers: this.getHeaders(),
					body,
					signal: AbortSignal.timeout(10_000),
				},
			);
			if (res.status === 404) {
				return { success: false, message: "", supported: false };
			}
			if (!res.ok) {
				const text = await res.text().catch(() => "");
				return {
					success: false,
					message: `Rollback failed: HTTP ${res.status}${text ? ` — ${text}` : ""}`,
					supported: true,
				};
			}
			let msg = `Service ${id} rollback initiated.`;
			const json = await res.json().catch(() => null);
			if (json?.message) msg = json.message;
			return { success: true, message: msg, supported: true };
		} catch (e: any) {
			return {
				success: false,
				message: `Rollback request failed: ${e.message || "network error"}`,
				supported: true,
			};
		}
	}

	async getServiceLogs(id: string): Promise<string> {
		if (isDemoMode()) {
			const ts = new Date().toISOString();
			return `[${ts}] [INFO] Starting service ${id}...
[${ts}] [INFO] Attached guest tap interface tap0 (192.168.127.2/24)
[${ts}] [INFO] Cloud-Hypervisor booted kernel vmlinux-6.6 in 142ms
[${ts}] [INFO] Init process spawned PID 1 (russel-guest-init)
[${ts}] [INFO] Service listening on 0.0.0.0:8080 (forwarded from host 8080)
[${ts}] [DEBUG] Health check GET /health HTTP/1.1 -> 200 OK (0.8ms)
[${ts}] [INFO] Memory RSS: 142 MB, CPU usage: 1.4%
[${ts}] [INFO] Received 1420 HTTP requests in last 60s (0 errors)`;
		}
		try {
			const res = await fetch(
				joinApiUrl(getApiBase(), `/vm/${encodeURIComponent(id)}/logs`),
				{
					headers: this.getHeaders(),
					signal: AbortSignal.timeout(5000),
				},
			);
			if (!res.ok) throw new Error(`HTTP ${res.status}`);
			const text = await res.text();
			try {
				const json: LogsResponse = JSON.parse(text);
				return json.output;
			} catch {
				return text; // text fallback
			}
		} catch {
			return `[Error] Could not fetch logs for ${id}.`;
		}
	}

	async stopService(
		id: string,
	): Promise<{ success: boolean; message: string }> {
		if (isDemoMode()) {
			return {
				success: true,
				message: `[Demo] Service ${id} stop signal sent.`,
			};
		}
		try {
			const res = await fetch(
				joinApiUrl(getApiBase(), `/vm/${encodeURIComponent(id)}/stop`),
				{
					method: "POST",
					headers: this.getHeaders(),
				},
			);
			if (!res.ok) {
				const body = await res.text().catch(() => "");
				return {
					success: false,
					message: `Stop failed: HTTP ${res.status}${body ? ` — ${body}` : ""}`,
				};
			}
			return { success: true, message: `Service ${id} stopped.` };
		} catch (e: any) {
			return {
				success: false,
				message: `Stop request failed: ${e.message || "network error"}`,
			};
		}
	}

	async updateService(
		id: string,
		onEvent?: (event: DeployEvent) => void,
	): Promise<{ success: boolean; message: string }> {
		if (isDemoMode()) {
			onEvent?.({
				type: "Progress",
				payload: {
					phase: "demo",
					description: `[Demo] Service ${id} update initiated.`,
				},
			});
			onEvent?.({
				type: "Complete",
				payload: {
					service_id: id,
					vm_id: id,
					status: "deployed",
					elapsed_ms: 0,
					message: `[Demo] Service ${id} update initiated.`,
				},
			});
			return {
				success: true,
				message: `[Demo] Service ${id} update initiated.`,
			};
		}
		try {
			const res = await fetch(
				joinApiUrl(getApiBase(), `/vm/${encodeURIComponent(id)}/update`),
				{
					method: "POST",
					headers: this.getHeaders(),
					body: "{}",
				},
			);
			if (!res.ok) {
				const body = await res.text().catch(() => "");
				const msg = `Update failed: HTTP ${res.status}${body ? ` — ${body}` : ""}`;
				onEvent?.({ type: "Error", payload: msg });
				return {
					success: false,
					message: msg,
				};
			}
			// /update streams NDJSON like /deploy — consume incrementally
			if (!res.body) {
				const msg = `Update failed: empty response body for service ${id}.`;
				onEvent?.({ type: "Error", payload: msg });
				return {
					success: false,
					message: msg,
				};
			}
			const state: {
				sawError: boolean;
				complete: DeployResponse | null;
				errorMessage: string | null;
			} = { sawError: false, complete: null, errorMessage: null };

			await consumeNdjsonStream(res.body, (line) => {
				const event = parseDeployEventLine(line);
				if (!event) return;
				// Forward every parsed event to the caller (parity with deployService).
				onEvent?.(event);
				if (event.type === "Error") {
					state.sawError = true;
					state.errorMessage = event.payload;
				} else if (event.type === "Complete") {
					state.complete = event.payload;
				}
			});

			const success =
				!state.sawError &&
				state.complete != null &&
				isDeployStatusSuccess(state.complete.status);

			if (success) {
				return {
					success: true,
					message: state.complete?.message || `Service ${id} updated.`,
				};
			}
			if (state.errorMessage) {
				return {
					success: false,
					message: `Update failed: ${state.errorMessage}`,
				};
			}
			if (state.complete) {
				const msg = `Update finished with status ${state.complete.status}.`;
				// Complete already forwarded; surface a terminal Error for silent UIs.
				onEvent?.({ type: "Error", payload: msg });
				return {
					success: false,
					message: msg,
				};
			}
			const closedMsg =
				"Update failed: control plane closed stream before Complete.";
			onEvent?.({ type: "Error", payload: closedMsg });
			return {
				success: false,
				message: closedMsg,
			};
		} catch (e: any) {
			const msg = `Update request failed: ${e.message || "network error"}`;
			onEvent?.({ type: "Error", payload: msg });
			return {
				success: false,
				message: msg,
			};
		}
	}

	async destroyService(
		id: string,
	): Promise<{ success: boolean; message: string }> {
		if (isDemoMode()) {
			return { success: true, message: `[Demo] Service ${id} destroyed.` };
		}
		try {
			const res = await fetch(
				joinApiUrl(getApiBase(), `/vm/${encodeURIComponent(id)}`),
				{
					method: "DELETE",
					headers: this.getHeaders(),
				},
			);
			if (!res.ok) {
				const body = await res.text().catch(() => "");
				return {
					success: false,
					message: `Destroy failed: HTTP ${res.status}${body ? ` — ${body}` : ""}`,
				};
			}
			return { success: true, message: `Service ${id} destroyed.` };
		} catch (e: any) {
			return {
				success: false,
				message: `Destroy request failed: ${e.message || "network error"}`,
			};
		}
	}

	async deployService(
		payload: DeployRequest,
		onEvent?: (event: DeployEvent) => void,
	): Promise<{ success: boolean }> {
		if (isDemoMode()) {
			const demoEvents: DeployEvent[] = [
				{
					type: "Progress",
					payload: { phase: "resolve", description: "Resolving repository..." },
				},
				{
					type: "Progress",
					payload: {
						phase: "build",
						description: "Building image / downloading rootfs...",
					},
				},
				{
					type: "Progress",
					payload: {
						phase: "create",
						description: "Configuring network bridge & tap interface...",
					},
				},
				{
					type: "Progress",
					payload: {
						phase: "start",
						description: "Launching Cloud-Hypervisor instance...",
					},
				},
				{
					type: "Complete",
					payload: {
						service_id: payload.vm_id || "demo-svc",
						vm_id: payload.vm_id || "demo-vm",
						status: "deployed",
						elapsed_ms: 2400,
						message: "Deployment complete!",
						runtime: "microvm",
					},
				},
			];
			for (const ev of demoEvents) {
				onEvent?.(ev);
				await new Promise((r) => setTimeout(r, 400));
			}
			return { success: true };
		}

		try {
			const res = await fetch(joinApiUrl(getApiBase(), "/deploy"), {
				method: "POST",
				headers: this.getHeaders(),
				body: JSON.stringify(payload),
			});
			if (!res.ok) {
				const body = await res.text().catch(() => "");
				onEvent?.({ type: "Error", payload: `HTTP ${res.status}: ${body}` });
				return { success: false };
			}
			if (!res.body) {
				onEvent?.({ type: "Error", payload: "Empty response body from /deploy" });
				return { success: false };
			}

			// Stream NDJSON: fire onEvent per complete line; success only on Complete(deployed) without Error
			const state: {
				sawError: boolean;
				complete: DeployResponse | null;
			} = { sawError: false, complete: null };

			await consumeNdjsonStream(res.body, (line) => {
				const event = parseDeployEventLine(line);
				if (!event) return;
				onEvent?.(event);
				if (event.type === "Error") {
					state.sawError = true;
				} else if (event.type === "Complete") {
					state.complete = event.payload;
				}
			});

			const success =
				!state.sawError &&
				state.complete != null &&
				isDeployStatusSuccess(state.complete.status);

			if (!success && !state.sawError && state.complete == null) {
				onEvent?.({
					type: "Error",
					payload: "Control plane closed stream before Complete",
				});
			} else if (
				!success &&
				!state.sawError &&
				state.complete != null &&
				!isDeployStatusSuccess(state.complete.status)
			) {
				onEvent?.({
					type: "Error",
					payload: `Deploy finished with status ${state.complete.status}`,
				});
			}

			return { success };
		} catch (e: any) {
			onEvent?.({ type: "Error", payload: e.message || "network error" });
			return { success: false };
		}
	}
}

export const api = new RusselClient();
