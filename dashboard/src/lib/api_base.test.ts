/**
 * Unit tests for API base validation and URL joining.
 * Run: bun test (from dashboard/)
 */
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
	apiBaseValidationError,
	getApiBase,
	getApiToken,
	joinApiUrl,
	setApiBase,
	setApiToken,
	validateApiBase,
} from "./api";

describe("joinApiUrl", () => {
	test("root empty base → absolute path only", () => {
		expect(joinApiUrl("", "/vms")).toBe("/vms");
		expect(joinApiUrl("", "vms")).toBe("/vms");
	});

	test("root slash base does not produce protocol-relative //vms", () => {
		expect(joinApiUrl("/", "/vms")).toBe("/vms");
		expect(joinApiUrl("/", "deploy")).toBe("/deploy");
	});

	test("relative /api base", () => {
		expect(joinApiUrl("/api", "/vms")).toBe("/api/vms");
		expect(joinApiUrl("/api/", "/vms")).toBe("/api/vms");
	});

	test("absolute https base", () => {
		expect(joinApiUrl("https://ctrl.example.com", "/vms")).toBe(
			"https://ctrl.example.com/vms",
		);
		expect(joinApiUrl("https://ctrl.example.com/api", "/deploy")).toBe(
			"https://ctrl.example.com/api/deploy",
		);
	});
});

describe("validateApiBase", () => {
	test("relative paths ok", () => {
		expect(validateApiBase("/api")).toBe("/api");
		expect(validateApiBase("/api/")).toBe("/api");
		expect(validateApiBase("/v1/russel")).toBe("/v1/russel");
	});

	test("root normalizes to empty string", () => {
		expect(validateApiBase("/")).toBe("");
	});

	test("https ok", () => {
		expect(validateApiBase("https://example.com")).toBe("https://example.com");
		expect(validateApiBase("https://example.com/api/")).toBe(
			"https://example.com/api",
		);
		expect(validateApiBase("http://127.0.0.1:7878")).toBe(
			"http://127.0.0.1:7878",
		);
	});

	test("reject protocol-relative //", () => {
		expect(validateApiBase("//evil.example/path")).toBeNull();
	});

	test("reject javascript: and other schemes", () => {
		expect(validateApiBase("javascript:alert(1)")).toBeNull();
		expect(validateApiBase("data:text/html,hi")).toBeNull();
		expect(validateApiBase("ftp://files.example/api")).toBeNull();
	});

	test("reject credentials", () => {
		expect(validateApiBase("https://user:pass@example.com/api")).toBeNull();
		expect(validateApiBase("http://token@127.0.0.1:7878")).toBeNull();
	});

	test("reject query and fragment on relative", () => {
		expect(validateApiBase("/api?x=1")).toBeNull();
		expect(validateApiBase("/api#frag")).toBeNull();
		expect(validateApiBase("/api?x=1#y")).toBeNull();
	});

	test("reject query and fragment on absolute", () => {
		expect(validateApiBase("https://example.com/api?x=1")).toBeNull();
		expect(validateApiBase("https://example.com/api#section")).toBeNull();
		expect(validateApiBase("http://127.0.0.1:7878/?q=1")).toBeNull();
	});

	test("reject empty / whitespace", () => {
		expect(validateApiBase("")).toBeNull();
		expect(validateApiBase("   ")).toBeNull();
	});

	test("reject bare host without scheme", () => {
		expect(validateApiBase("example.com/api")).toBeNull();
		expect(validateApiBase("127.0.0.1:7878")).toBeNull();
	});

	test("root join never yields //vms", () => {
		const base = validateApiBase("/");
		expect(base).toBe("");
		expect(joinApiUrl(base!, "/vms")).toBe("/vms");
		// Even if a caller still has legacy stored "/"
		expect(joinApiUrl("/", "/vms")).toBe("/vms");
	});
});

describe("apiBaseValidationError", () => {
	test("mentions query/fragment", () => {
		expect(apiBaseValidationError("/api?x=1")).toMatch(/query|fragment/i);
		expect(apiBaseValidationError("https://ex.com?x=1")).toMatch(
			/query|fragment/i,
		);
	});

	test("mentions credentials", () => {
		expect(apiBaseValidationError("https://u:p@ex.com")).toMatch(
			/credentials/i,
		);
	});

	test("mentions scheme", () => {
		expect(apiBaseValidationError("javascript:void(0)")).toMatch(/scheme/i);
	});
});

describe("getApiBase / setApiBase", () => {
	const store = new Map<string, string>();
	const sessionStore = new Map<string, string>();
	const memoryStorage = {
		getItem(key: string) {
			return store.has(key) ? store.get(key)! : null;
		},
		setItem(key: string, value: string) {
			store.set(key, String(value));
		},
		removeItem(key: string) {
			store.delete(key);
		},
		clear() {
			store.clear();
		},
		key(i: number) {
			return [...store.keys()][i] ?? null;
		},
		get length() {
			return store.size;
		},
	};
	const memorySessionStorage = {
		getItem(key: string) {
			return sessionStore.has(key) ? sessionStore.get(key)! : null;
		},
		setItem(key: string, value: string) {
			sessionStore.set(key, String(value));
		},
		removeItem(key: string) {
			sessionStore.delete(key);
		},
		clear() {
			sessionStore.clear();
		},
	};

	beforeEach(() => {
		store.clear();
		sessionStore.clear();
		// get/setApiBase gate on `window`; bun test has no DOM by default.
		(globalThis as any).window = globalThis;
		(globalThis as any).localStorage = memoryStorage;
		(globalThis as any).sessionStorage = memorySessionStorage;
	});
	afterEach(() => {
		store.clear();
		sessionStore.clear();
		delete (globalThis as any).localStorage;
		delete (globalThis as any).sessionStorage;
		// leave window alone if other suites need it; only remove our stub if we set it
		if ((globalThis as any).window === globalThis) {
			delete (globalThis as any).window;
		}
	});

	test("default when unset is /api", () => {
		expect(getApiBase()).toBe("/api");
	});

	test("set/get relative and absolute bases", () => {
		setApiBase("/v1/russel");
		expect(getApiBase()).toBe("/v1/russel");
		expect(memoryStorage.getItem("RUSSEL_API_URL")).toBe("/v1/russel");

		setApiBase("https://ctrl.example.com/api/");
		expect(getApiBase()).toBe("https://ctrl.example.com/api");
	});

	test("set root normalizes to empty string and get returns it", () => {
		setApiBase("/");
		expect(memoryStorage.getItem("RUSSEL_API_URL")).toBe("");
		expect(getApiBase()).toBe("");
		expect(joinApiUrl(getApiBase(), "/vms")).toBe("/vms");
	});

	test("set rejects query/fragment/credentials and leaves storage unchanged", () => {
		setApiBase("/api");
		expect(() => setApiBase("/api?x=1")).toThrow(/query|fragment/i);
		expect(() => setApiBase("https://ex.com#frag")).toThrow(/query|fragment/i);
		expect(() => setApiBase("https://u:p@ex.com")).toThrow(/credentials/i);
		expect(getApiBase()).toBe("/api");
	});

	test("invalid stored value falls back to /api", () => {
		memoryStorage.setItem("RUSSEL_API_URL", "javascript:alert(1)");
		expect(getApiBase()).toBe("/api");
	});

	test("token is session-scoped and migrates once from legacy localStorage", () => {
		store.set("RUSSEL_API_TOKEN", "legacy-token");
		expect(getApiToken()).toBe("legacy-token");
		expect(memorySessionStorage.getItem("RUSSEL_API_TOKEN")).toBe("legacy-token");
		expect(memoryStorage.getItem("RUSSEL_API_TOKEN")).toBeNull();

		setApiToken("new-token");
		expect(getApiToken()).toBe("new-token");
		expect(memoryStorage.getItem("RUSSEL_API_TOKEN")).toBeNull();
		expect(memorySessionStorage.getItem("RUSSEL_API_TOKEN")).toBe("new-token");
	});
});
