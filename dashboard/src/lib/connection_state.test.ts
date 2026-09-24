import { describe, expect, test } from "bun:test";
import {
	classifyApiTopology,
	classifyConnectionResult,
	CleartextTokenError,
	connectionResultMessage,
	headerBadgeMeta,
	overviewMetricsForConnection,
	overviewServicesEmptyCopy,
	STATIC_FLEET_STATUS,
} from "./api";
import {
	offlineTopologyMessage,
	renderOfflineState,
	renderUnauthorizedState,
} from "./offline_state";

function response(status: number, ok = status >= 200 && status < 300): Response {
	return { status, ok } as Response;
}

describe("classifyConnectionResult", () => {
	test("success response", () => {
		expect(classifyConnectionResult(response(200)).kind).toBe("success");
	});

	test("401 is unauthorized, not network", () => {
		const result = classifyConnectionResult(response(401));
		expect(result.kind).toBe("unauthorized");
		if (result.kind === "unauthorized") expect(result.status).toBe(401);
	});

	test("other HTTP status is http_error", () => {
		const result = classifyConnectionResult(response(503));
		expect(result.kind).toBe("http_error");
		if (result.kind === "http_error") expect(result.status).toBe(503);
	});

	test("loopback network failure gets tunnel guidance", () => {
		const result = classifyConnectionResult(new TypeError("Failed to fetch"));
		expect(result.kind).toBe("network");
		expect(
			connectionResultMessage(result, "/api", "http://127.0.0.1:4321"),
		).toMatch(/install\.sh connect/);
	});

	test("remote HTTPS network failure gets reverse-proxy guidance", () => {
		const result = classifyConnectionResult(new TypeError("Failed to fetch"));
		expect(
			connectionResultMessage(result, "https://203.0.113.10/api", "https://198.51.100.10"),
		).toMatch(/reverse proxy/i);
		expect(
			connectionResultMessage(result, "https://203.0.113.10/api", "https://198.51.100.10"),
		).toMatch(/do not start russel-ctrl on the laptop/i);
	});

	test("cleartext policy refusal is cleartext", () => {
		const result = classifyConnectionResult(
			new CleartextTokenError("Refusing to send API token over cleartext HTTP"),
		);
		expect(result.kind).toBe("cleartext");
		if (result.kind === "cleartext") {
			expect(result.message).toMatch(/cleartext HTTP/i);
		}
	});
});

describe("API topology guidance", () => {
	test("relative /api on localhost recommends the SSH tunnel", () => {
		const result = offlineTopologyMessage("/api", "http://localhost:4321");
		expect(result.topology.kind).toBe("relative-loopback");
		expect(result.message).toMatch(/install\.sh connect/);
	});

	test("relative /api on production origin recommends the proxy", () => {
		const result = offlineTopologyMessage("/api", "https://198.51.100.10");
		expect(result.topology.kind).toBe("relative-same-origin");
		expect(result.message).toMatch(/same-origin reverse proxy/i);
		expect(result.message).toMatch(/do not start russel-ctrl on the laptop/i);
	});

	test("absolute loopback is local-only", () => {
		expect(classifyApiTopology("http://127.0.0.1:7878").kind).toBe(
			"absolute-loopback",
		);
	});

	test("remote HTTPS points at the reverse proxy", () => {
		const result = offlineTopologyMessage(
			"https://203.0.113.10/api",
			"https://198.51.100.10",
		);
		expect(result.topology.kind).toBe("absolute-remote-https");
		expect(result.message).toMatch(/remote API host 203\.0\.113\.10/);
		expect(result.message).toMatch(/reverse proxy/i);
	});

	test("offline and unauthorized HTML escape messages and keep the Demo CTA", () => {
		const offline = renderOfflineState(
			"<img src=x onerror=alert(1)>",
			"/api",
			"http://127.0.0.1:4321",
		);
		expect(offline).toContain("&lt;img src=x onerror=alert(1)&gt;");
		expect(offline).not.toContain("<img src=x");
		expect(offline).toContain('href="/settings"');
		expect(offline).toMatch(/Demo mode/);

		const unauthorized = renderUnauthorizedState("<wrong token>");
		expect(unauthorized).toContain("&lt;wrong token&gt;");
		expect(unauthorized).toContain('href="/settings"');
		expect(unauthorized).toMatch(/Demo mode/);
		expect(unauthorized).not.toMatch(/start russel-ctrl|open the tunnel/i);
	});
});

describe("shared unauthorized presentation", () => {
	test("unauthorized badge is not OFFLINE", () => {
		const badge = headerBadgeMeta("unauthorized");
		expect(badge.label).toBe("UNAUTHORIZED");
		expect(badge.label).not.toBe("OFFLINE");
		expect(badge.title).toMatch(/token missing or wrong/i);
	});

	test("loopback /api offline badge mentions a tunnel", () => {
		expect(
			headerBadgeMeta("offline", "/api", "http://localhost:4321").title,
		).toMatch(/tunnel\?/i);
	});

	test("unauthorized overview copy is not offline or unreachable", () => {
		const cards = overviewMetricsForConnection(
			"unauthorized",
			STATIC_FLEET_STATUS,
			[],
		);
		expect(cards.every((card) => !/offline|unreachable/i.test(`${card.value} ${card.subtext}`))).toBe(
			true,
		);
		expect(overviewServicesEmptyCopy("unauthorized")).toEqual({
			primary: "API token missing or wrong.",
			kind: "unauthorized",
		});
	});
});
