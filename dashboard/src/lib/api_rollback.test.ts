import { afterEach, describe, expect, test } from "bun:test";
import { api } from "./api";

const originalFetch = globalThis.fetch;

afterEach(() => {
	globalThis.fetch = originalFetch;
});

function mockStream(events: unknown[]): void {
	globalThis.fetch = (async (_input, init) => {
		expect(init?.signal).toBeUndefined();
		return new Response(events.map((event) => JSON.stringify(event)).join("\n") + "\n", {
			status: 200,
			headers: { "Content-Type": "application/x-ndjson" },
		});
	}) as typeof fetch;
}

const complete = (status: string, message: string) => ({
	type: "Complete",
	payload: {
		service_id: "svc",
		vm_id: "svc",
		status,
		elapsed_ms: 100,
		message,
	},
});

describe("rollbackService", () => {
	test("failed Complete reports rollback failure", async () => {
		mockStream([
			{ type: "Progress", payload: { phase: "build", description: "building" } },
			complete("failed", "build failed"),
		]);
		expect(await api.rollbackService("svc", 1)).toEqual({
			success: false,
			message: "Rollback failed: build failed",
			supported: true,
		});
	});

	test("Error event fails even after deployed Complete", async () => {
		mockStream([{ type: "Error", payload: "stream failed" }, complete("deployed", "ok")]);
		expect(await api.rollbackService("svc", 1)).toEqual({
			success: false,
			message: "Rollback failed: stream failed",
			supported: true,
		});
	});

	test("missing Complete fails", async () => {
		mockStream([{ type: "Progress", payload: { phase: "build", description: "building" } }]);
		expect((await api.rollbackService("svc", 1)).success).toBe(false);
	});

	test("deployed Complete succeeds", async () => {
		mockStream([complete("deployed", "rolled back")]);
		expect(await api.rollbackService("svc", 1)).toEqual({
			success: true,
			message: "rolled back",
			supported: true,
		});
	});

	test("stream read failure reports rollback failure", async () => {
		globalThis.fetch = (async () =>
			new Response(
				new ReadableStream({
					start(controller) {
						controller.error(new Error("stream disconnected"));
					},
				}),
				{ status: 200 },
			)) as typeof fetch;
		expect(await api.rollbackService("svc", 1)).toEqual({
			success: false,
			message: "Rollback request failed: stream disconnected",
			supported: true,
		});
	});
});
