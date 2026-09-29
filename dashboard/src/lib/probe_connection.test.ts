/**
 * The top bar polls `probeConnection` on every view, so it must stay one
 * `GET /vms` with no per-service status fan-out.
 * Run: bun test (from dashboard/)
 */
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { api } from "./api";

const realFetch = globalThis.fetch;
let calls: string[] = [];

function stubFetch(respond: (url: string) => Response) {
	globalThis.fetch = (async (input: RequestInfo | URL) => {
		const url = String(input);
		calls.push(url);
		return respond(url);
	}) as typeof fetch;
}

describe("probeConnection", () => {
	beforeEach(() => {
		calls = [];
	});
	afterEach(() => {
		globalThis.fetch = realFetch;
	});

	test("live: one /vms call and the service count", async () => {
		stubFetch(() =>
			Response.json({
				services: [{ service_id: "a" }, { service_id: "b" }, { service_id: "c" }],
			}),
		);
		expect(await api.probeConnection()).toEqual({ connection: "live", total: 3 });
		expect(calls).toEqual(["/api/vms"]);
	});

	test("401 is unauthorized with no count", async () => {
		stubFetch(() => new Response("", { status: 401 }));
		expect(await api.probeConnection()).toEqual({ connection: "unauthorized" });
	});

	test("network failure is offline", async () => {
		globalThis.fetch = (async () => {
			throw new TypeError("Failed to fetch");
		}) as unknown as typeof fetch;
		expect(await api.probeConnection()).toEqual({ connection: "offline" });
	});
});
