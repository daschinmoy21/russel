/**
 * Unit tests for NDJSON deploy stream parsing (pure helpers in api.ts).
 * Run: bun test (from dashboard/)
 */
import { describe, expect, test } from "bun:test";
import {
	consumeNdjsonStream,
	isDeployStatusSuccess,
	parseDeployEventLine,
	reduceDeployEvents,
	type DeployResponse,
	type StatusResponse,
} from "./api";

const deployed: DeployResponse = {
	service_id: "svc-a",
	vm_id: "vm-a",
	status: "deployed",
	elapsed_ms: 100,
	message: "ok",
};

const failedComplete: DeployResponse = {
	service_id: "svc-a",
	vm_id: "vm-a",
	status: "failed",
	elapsed_ms: 50,
	message: "boom",
};

function streamFromChunks(chunks: string[]): ReadableStream<Uint8Array> {
	const encoder = new TextEncoder();
	let i = 0;
	return new ReadableStream({
		pull(controller) {
			if (i < chunks.length) {
				controller.enqueue(encoder.encode(chunks[i++]));
			} else {
				controller.close();
			}
		},
	});
}

describe("isDeployStatusSuccess", () => {
	test("only exact deployed is success", () => {
		expect(isDeployStatusSuccess("deployed")).toBe(true);
		expect(isDeployStatusSuccess("failed")).toBe(false);
		expect(isDeployStatusSuccess("Deployed")).toBe(false);
		expect(isDeployStatusSuccess("")).toBe(false);
	});
});

describe("parseDeployEventLine", () => {
	test("empty / whitespace → null", () => {
		expect(parseDeployEventLine("")).toBeNull();
		expect(parseDeployEventLine("   ")).toBeNull();
	});

	test("Progress event", () => {
		const ev = parseDeployEventLine(
			JSON.stringify({
				type: "Progress",
				payload: { phase: "build", description: "Building..." },
			}),
		);
		expect(ev).toEqual({
			type: "Progress",
			payload: { phase: "build", description: "Building..." },
		});
	});

	test("Complete event", () => {
		const ev = parseDeployEventLine(
			JSON.stringify({ type: "Complete", payload: deployed }),
		);
		expect(ev?.type).toBe("Complete");
		if (ev?.type === "Complete") {
			expect(ev.payload.status).toBe("deployed");
			expect(ev.payload.service_id).toBe("svc-a");
		}
	});

	test("Error event (string payload)", () => {
		const ev = parseDeployEventLine(
			JSON.stringify({ type: "Error", payload: "nope" }),
		);
		expect(ev).toEqual({ type: "Error", payload: "nope" });
	});

	test("bare DeployResponse shape → Complete", () => {
		const ev = parseDeployEventLine(JSON.stringify(deployed));
		expect(ev?.type).toBe("Complete");
		if (ev?.type === "Complete") {
			expect(ev.payload.status).toBe("deployed");
		}
	});

	test("non-JSON → raw Progress", () => {
		const ev = parseDeployEventLine("not json at all");
		expect(ev).toEqual({
			type: "Progress",
			payload: { phase: "raw", description: "not json at all" },
		});
	});
});

describe("reduceDeployEvents", () => {
	test("Complete deployed → success true", () => {
		const lines = [
			JSON.stringify({
				type: "Progress",
				payload: { phase: "start", description: "go" },
			}),
			JSON.stringify({ type: "Complete", payload: deployed }),
		];
		const out = reduceDeployEvents(lines);
		expect(out.success).toBe(true);
		expect(out.sawError).toBe(false);
		expect(out.complete?.status).toBe("deployed");
	});

	test("Complete non-deployed → success false", () => {
		const lines = [JSON.stringify({ type: "Complete", payload: failedComplete })];
		const out = reduceDeployEvents(lines);
		expect(out.success).toBe(false);
		expect(out.sawError).toBe(false);
		expect(out.complete?.status).toBe("failed");
	});

	test("Error → success false", () => {
		const lines = [
			JSON.stringify({ type: "Error", payload: "build failed" }),
			JSON.stringify({ type: "Complete", payload: deployed }),
		];
		const out = reduceDeployEvents(lines);
		expect(out.success).toBe(false);
		expect(out.sawError).toBe(true);
		expect(out.errorMessage).toBe("build failed");
	});

	test("bare DeployResponse shape → success when deployed", () => {
		const out = reduceDeployEvents([JSON.stringify(deployed)]);
		expect(out.success).toBe(true);
		expect(out.complete?.service_id).toBe("svc-a");
	});

	test("invokes onEvent for each parsed event", () => {
		const seen: string[] = [];
		reduceDeployEvents(
			[
				JSON.stringify({
					type: "Progress",
					payload: { phase: "a", description: "1" },
				}),
				JSON.stringify({ type: "Complete", payload: deployed }),
			],
			(ev) => seen.push(ev.type),
		);
		expect(seen).toEqual(["Progress", "Complete"]);
	});
});

describe("consumeNdjsonStream", () => {
	test("fragmented lines across chunks", async () => {
		const completeLine = JSON.stringify({
			type: "Complete",
			payload: deployed,
		});
		// Split mid-line then add newline + trailing partial without newline
		const mid = Math.floor(completeLine.length / 2);
		const chunks = [
			'{"type":"Progress","payload":{"phase":"x","description":"y"}}\n' +
				completeLine.slice(0, mid),
			completeLine.slice(mid) + "\n",
			'{"type":"Error","payload":"late"}', // trailing, no newline
		];
		const lines: string[] = [];
		await consumeNdjsonStream(streamFromChunks(chunks), (line) => lines.push(line));
		expect(lines).toHaveLength(3);
		const out = reduceDeployEvents(lines);
		// Error after Complete still marks failure
		expect(out.sawError).toBe(true);
		expect(out.success).toBe(false);
	});

	test("trailing line without newline is emitted", async () => {
		const line = JSON.stringify({ type: "Complete", payload: deployed });
		const lines: string[] = [];
		await consumeNdjsonStream(streamFromChunks([line]), (l) => lines.push(l));
		expect(lines).toEqual([line]);
		expect(reduceDeployEvents(lines).success).toBe(true);
	});

	test("empty stream yields no lines", async () => {
		const lines: string[] = [];
		await consumeNdjsonStream(streamFromChunks([]), (l) => lines.push(l));
		expect(lines).toEqual([]);
	});
});

describe("route_host", () => {
	test("Complete carries custom ingress host through", () => {
		const withRoute: DeployResponse = { ...deployed, route_host: "abc.com" };
		const ev = parseDeployEventLine(
			JSON.stringify({ type: "Complete", payload: withRoute }),
		);
		expect(ev?.type).toBe("Complete");
		if (ev?.type === "Complete") {
			expect(ev.payload.route_host).toBe("abc.com");
		}
	});

	test("StatusResponse accepts string, null, and absent route_host", () => {
		const withHost = JSON.parse(
			'{"service_id":"a","status":"deployed","vm_state":"running","uptime_seconds":1,"route_host":"abc.com"}',
		) as StatusResponse;
		expect(withHost.route_host).toBe("abc.com");
		const withNull = JSON.parse(
			'{"service_id":"a","status":"deployed","vm_state":"running","uptime_seconds":1,"route_host":null}',
		) as StatusResponse;
		expect(withNull.route_host).toBeNull();
		const legacy = JSON.parse(
			'{"service_id":"a","status":"deployed","vm_state":"running","uptime_seconds":1}',
		) as StatusResponse;
		expect(legacy.route_host).toBeUndefined();
	});
});
