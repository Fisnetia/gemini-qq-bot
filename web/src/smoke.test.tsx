// Console smoke tests: the whole shell against a stubbed admin API.
// They pin the contract the pages rely on — login, tab ARIA wiring,
// dashboard rendering, the pending-enable group section, and the
// persona editor round trip (the edit must reach the API as a PUT).

import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";

const persona = {
  persona: "# Role: Smoke\n\n冒烟测试用人设卡。",
  is_default: false,
  max_chars: 4000,
};

const payloads: Record<string, unknown> = {
  "/api/v1/settings": {
    aux: {
      mode: "agy",
      enabled: false,
      base_url: "http://host.docker.internal:18088",
      model: "hy3",
      session_header: "X-Aux-Session",
      api_key_set: false,
    },
    runtime: { session_rotation_enabled: true, quota_degradation_enabled: true },
    engine: { model: "gemini-3.8-flash", effort: "low" },
  },
  "/api/v1/settings/engine/models": {
    models: [{ id: "gemini-3.8-flash-high", label: "Gemini 3.8 Flash (High)" }],
  },
  "/api/v1/status": {
    bot_id: "2000000001",
    health: { production_ready: true, blockers: [], aux: { ready: true, error: null } },
    groups: 2,
    lanes: [],
    pending_messages: 0,
    outbox: [],
    calls_total: 42,
    pacing: {
      tier: "T0",
      raw_tier: "T0",
      degradation_enabled: true,
      weekly_pct: 93.5,
      five_hour_pct: 97.0,
      snapshot_age_s: 30,
      stale: false,
      effective_caps: [],
    },
  },
  "/api/v1/quota": {
    gemini: {
      five_hour: { remaining_fraction: 0.97, reset_time: "2026-11-03T05:27:55Z" },
      weekly: { remaining_fraction: 0.935, reset_time: "2026-11-07T03:03:55Z" },
    },
    fetched_at: "2026-11-03T03:27:55Z",
    error: null,
  },
  "/api/v1/groups": {
    groups: [
      {
        group_id: "900000001",
        enabled: true,
        paused: false,
        resident_insert_enabled: true,
        agy_conversation_id: "conv-1",
        has_conversation: true,
        agy_turn_count: 12,
        ambient_pace: "normal",
        conversation_turn_limit: 35,
        name: "测试群一",
        member_count: 42,
        departed_at: null,
      },
      {
        group_id: "900000002",
        enabled: false,
        paused: false,
        resident_insert_enabled: false,
        agy_conversation_id: null,
        has_conversation: false,
        agy_turn_count: 0,
        ambient_pace: "normal",
        conversation_turn_limit: 35,
        name: "新发现的群",
        member_count: 7,
        departed_at: null,
      },
    ],
  },
  "/api/v1/persona": persona,
  "/api/v1/persona/private": { persona: "", is_default: true, max_chars: 4000 },
  "/api/v1/stickers": { stickers: [], total: 0, library_size: 0 },
};

const puts: Array<{ url: string; body: Record<string, unknown> }> = [];

function installFetch() {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? "GET";
      const respond = (obj: unknown, status = 200) =>
        new Response(JSON.stringify(obj), {
          status,
          headers: { "Content-Type": "application/json" },
        });
      if ((init?.headers as Record<string, string>)?.Authorization !== "Bearer test-token") {
        return respond({ error: "unauthorized" }, 401);
      }
      if (method === "PUT" && url === "/api/v1/persona") {
        const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
        puts.push({ url, body });
        persona.persona = String(body.persona);
        return respond({ persona: persona.persona, is_default: false, max_chars: 4000 });
      }
      if (method === "PUT" && url === "/api/v1/persona/private") {
        const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
        puts.push({ url, body });
        return respond({ ok: true });
      }
      if (method === "GET" && url in payloads) {
        return respond(payloads[url]);
      }
      if (method === "GET" && url.startsWith("/api/v1/stickers")) {
        return respond(payloads["/api/v1/stickers"]);
      }
      return respond({ error: "not_found" }, 404);
    }),
  );
}

describe("console shell", () => {
  beforeEach(() => {
    sessionStorage.clear();
    puts.length = 0;
    persona.persona = "# Role: Smoke\n\n冒烟测试用人设卡。";
    installFetch();
  });

  it("logs in, renders every page, and the persona edit reaches the API", async () => {
    render(<App />);

    // Login gate.
    fireEvent.change(screen.getByLabelText("管理令牌（部署时配置的 ADMIN_TOKEN）"), {
      target: { value: "test-token" },
    });
    fireEvent.click(screen.getByRole("button", { name: "进入管理台" }));

    // Dashboard renders from the canned status/quota/settings.
    expect(await screen.findByText("生产就绪")).toBeTruthy();
    expect(await screen.findByText(/93\.5%/)).toBeTruthy();

    // Tab wiring: five tabs, tablist semantics, selected state follows.
    const tabs = screen.getAllByRole("tab");
    expect(tabs.map((t) => t.textContent)).toEqual([
      "总览",
      "群管理",
      "人设卡",
      "表情",
      "设置",
    ]);
    expect(tabs[0].getAttribute("aria-selected")).toBe("true");

    // Groups: the discovered group sits in the pending-enable section.
    fireEvent.click(screen.getByRole("tab", { name: "群管理" }));
    expect(await screen.findByText(/已发现待启用/)).toBeTruthy();
    expect(screen.getByText("新发现的群")).toBeTruthy();
    expect(screen.getByText("测试群一")).toBeTruthy();

    // Persona editor: shows the stored card, saving PUTs the edit.
    fireEvent.click(screen.getByRole("tab", { name: "人设卡" }));
    const area = await screen.findByLabelText("群聊人设内容");
    expect((area as HTMLTextAreaElement).value).toContain("冒烟测试用人设卡");
    fireEvent.change(area, { target: { value: "# Role: Smoke\n\n改过的人设卡。" } });
    fireEvent.click(screen.getByRole("button", { name: "保存群聊人设" }));
    await waitFor(() => expect(puts).toHaveLength(1));
    expect(puts[0].body.persona).toBe("# Role: Smoke\n\n改过的人设卡。");
    expect(await screen.findByText(/已保存。各群在下一轮对话时换上新人设卡/)).toBeTruthy();

    // Private card: starts empty and saves to its own endpoint.
    const privateArea = await screen.findByLabelText("私聊人设内容");
    expect((privateArea as HTMLTextAreaElement).value).toBe("");
    fireEvent.change(privateArea, { target: { value: "私聊人设" } });
    fireEvent.click(screen.getByRole("button", { name: "保存私聊人设" }));
    await waitFor(() => expect(puts).toHaveLength(2));
    expect(puts[1].url).toBe("/api/v1/persona/private");
    expect(puts[1].body.persona).toBe("私聊人设");

    // Settings: agy mode is explained, gateway starts disabled.
    fireEvent.click(screen.getByRole("tab", { name: "设置" }));
    expect(await screen.findByText(/自动落到/)).toBeTruthy();
    const modelSelect = screen.getByRole("combobox", { name: "主模型" }) as HTMLSelectElement;
    expect(modelSelect.value).toBe("gemini-3.8-flash");
    expect(await screen.findByText(/gemini-3\.8-flash-high/)).toBeTruthy();
  });
});
