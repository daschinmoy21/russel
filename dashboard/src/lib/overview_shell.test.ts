/**
 * Unit tests for overview static shell + connection badge contract (#316).
 * Run: bun test (from dashboard/)
 */
import { describe, expect, test } from "bun:test";
import {
	STATIC_FLEET_STATUS,
	headerBadgeMeta,
	overviewMetricsForConnection,
	overviewServicesEmptyCopy,
	resolveHeaderConnection,
	type FleetStatus,
	type ServiceVM,
} from "./api";

describe("resolveHeaderConnection / headerBadgeMeta", () => {
	test("omitted connection is loading, not LIVE", () => {
		expect(resolveHeaderConnection()).toBe("loading");
		expect(resolveHeaderConnection(undefined, false)).toBe("loading");
		const badge = headerBadgeMeta(resolveHeaderConnection());
		expect(badge.label).not.toBe("LIVE");
		expect(badge.label).toBe("…");
		expect(badge.className).toBe("badge-muted");
	});

	test("explicit live / offline / demo / loading", () => {
		expect(headerBadgeMeta("live").label).toBe("LIVE");
		expect(headerBadgeMeta("offline").label).toBe("OFFLINE");
		expect(headerBadgeMeta("demo").label).toBe("DEMO");
		expect(headerBadgeMeta("loading").label).toBe("…");
	});

	test("isDemo without connection → demo", () => {
		expect(resolveHeaderConnection(undefined, true)).toBe("demo");
		expect(headerBadgeMeta(resolveHeaderConnection(undefined, true)).label).toBe(
			"DEMO",
		);
	});

	test("explicit connection wins over isDemo", () => {
		expect(resolveHeaderConnection("offline", true)).toBe("offline");
		expect(resolveHeaderConnection("loading", true)).toBe("loading");
	});
});

describe("STATIC_FLEET_STATUS shell", () => {
	test("prerender snapshot is zeroed / offline (no baked fleet)", () => {
		expect(STATIC_FLEET_STATUS.online).toBe(false);
		expect(STATIC_FLEET_STATUS.total_vms).toBe(0);
		expect(STATIC_FLEET_STATUS.running_vms).toBe(0);
		expect(STATIC_FLEET_STATUS.stopped_vms).toBe(0);
		expect(STATIC_FLEET_STATUS.failed_vms).toBe(0);
		expect(STATIC_FLEET_STATUS.api_latency_ms).toBe(0);
		expect(STATIC_FLEET_STATUS.max_uptime_seconds).toBe(0);
	});
});

describe("overviewMetricsForConnection", () => {
	const liveStatus: FleetStatus = {
		online: true,
		total_vms: 2,
		running_vms: 1,
		stopped_vms: 1,
		failed_vms: 0,
		api_latency_ms: 12,
		max_uptime_seconds: 3600,
	};

	const services: ServiceVM[] = [
		{
			id: "a",
			runtime: "microvm",
			status: "deployed",
			vm_state: "running",
		},
		{
			id: "b",
			runtime: "container",
			status: "stopped",
			vm_state: "stopped",
		},
	];

	test("loading shell placeholders", () => {
		const cards = overviewMetricsForConnection(
			"loading",
			STATIC_FLEET_STATUS,
			[],
		)!;
		expect(cards).toHaveLength(4);
		expect(cards.every((c) => c.value === "—" || c.subtext.includes("Loading"))).toBe(
			true,
		);
		expect(cards[0].subtext).toBe("Loading…");
		expect(cards[2].subtext).toBe("Loading…");
		expect(cards[3].subtext).toBe("Loading…");
	});

	test("offline hydrate state", () => {
		const cards = overviewMetricsForConnection(
			"offline",
			STATIC_FLEET_STATUS,
			[],
		)!;
		expect(cards[0].value).toBe("0 / 0");
		expect(cards[0].subtext).toBe("Control plane offline");
		expect(cards[2].value).toBe("Offline");
		expect(cards[3].value).toBe("0");
	});

	test("live fleet hydrate state", () => {
		const cards = overviewMetricsForConnection("live", liveStatus, services)!;
		expect(cards[0].value).toBe("1 / 2");
		expect(cards[0].subtext).toBe("All systems operational");
		expect(cards[2].value).toBe("12 ms");
		expect(cards[3].value).toBe("2");
		expect(cards[3].subtext).toBe("1 running");
	});

	test("live empty-fleet hydrate state", () => {
		const empty: FleetStatus = {
			...liveStatus,
			total_vms: 0,
			running_vms: 0,
			stopped_vms: 0,
		};
		const cards = overviewMetricsForConnection("live", empty, [])!;
		expect(cards[0].value).toBe("0 / 0");
		expect(cards[3].value).toBe("0");
		expect(cards[3].subtext).toBe("0 running");
	});
});

describe("overviewServicesEmptyCopy", () => {
	test("offline vs empty fleet copy", () => {
		expect(overviewServicesEmptyCopy("offline").kind).toBe("offline");
		expect(overviewServicesEmptyCopy("offline").primary).toMatch(/offline/i);
		expect(overviewServicesEmptyCopy("live").kind).toBe("empty");
		expect(overviewServicesEmptyCopy("demo").primary).toMatch(/No services/i);
	});
});
