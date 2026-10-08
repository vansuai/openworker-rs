import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { NewAutomationForm } from "./ScheduledView";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function stubFetch(connectors: any[], channels: any[] = []) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      const u = String(url);
      if (u.includes("/v1/connectors")) {
        return { ok: true, json: async () => ({ connectors }) } as Response;
      }
      if (u.includes("/v1/cloud/status")) {
        return { ok: true, json: async () => ({ signed_in: true, account: "me" }) } as Response;
      }
      if (u.includes("/v1/channels/recent")) {
        return { ok: true, json: async () => ({ channels }) } as Response;
      }
      return { ok: true, json: async () => ({}) } as Response;
    }),
  );
}

const SLACK = {
  name: "slack",
  title: "Slack",
  blurb: "Two-way Slack messaging.",
  channels: true,
  connected: true,
  available: true,
  brand_color: "#611f69",
  logo: "slack",
};
const GMAIL = {
  name: "gmail",
  title: "Gmail",
  blurb: "Mail.",
  channels: false,
  connected: false,
  available: true,
  brand_color: "#ea4335",
  logo: "gmail",
};

function fillBasics() {
  fireEvent.change(screen.getByPlaceholderText(/Title/), { target: { value: "Notes" } });
  fireEvent.change(screen.getByPlaceholderText(/What should it do/), {
    target: { value: "Write the notes." },
  });
}

describe("NewAutomationForm — search grant and connections", () => {
  it("omits permissions when neither search nor channel consent is set", async () => {
    stubFetch([SLACK, GMAIL]);
    const onCreate = vi.fn();
    render(<NewAutomationForm busy={false} onCancel={() => {}} onCreate={onCreate} />);
    fillBasics();
    fireEvent.click(screen.getByTestId("na-create"));
    expect(onCreate).toHaveBeenCalledWith(
      expect.objectContaining({ title: "Notes", instructions: "Write the notes." }),
    );
    expect(onCreate.mock.calls[0][0].permissions).toBeUndefined();
  });

  it("sends a name-only web_search grant when the search box is checked", () => {
    stubFetch([SLACK]);
    const onCreate = vi.fn();
    render(<NewAutomationForm busy={false} onCancel={() => {}} onCreate={onCreate} />);
    fillBasics();
    fireEvent.click(screen.getByTestId("na-search-consent"));
    fireEvent.click(screen.getByTestId("na-create"));
    expect(onCreate.mock.calls[0][0].permissions).toEqual([
      { tool: "web_search", access: "write" },
    ]);
  });

  it("adds a connected Slack row and mints send_message when the channel is consented", async () => {
    stubFetch([SLACK, GMAIL]);
    const onCreate = vi.fn();
    render(<NewAutomationForm busy={false} onCancel={() => {}} onCreate={onCreate} />);
    await waitFor(() => expect(screen.getByTestId("na-add-conn")).toBeTruthy());
    fireEvent.change(screen.getByTestId("na-add-conn"), { target: { value: "slack" } });
    expect(screen.getByTestId("na-conn-slack").textContent).toContain("Connected");
    expect(screen.queryByTestId("na-connect-slack")).toBeNull();

    fireEvent.change(screen.getByPlaceholderText(/slack:C0123/), {
      target: { value: "slack:T1/C1" },
    });
    fireEvent.click(screen.getByTestId("na-post-consent"));
    fireEvent.click(screen.getByTestId("na-search-consent"));
    fillBasics();
    fireEvent.click(screen.getByTestId("na-create"));
    expect(onCreate.mock.calls[0][0].permissions).toEqual([
      { tool: "web_search", access: "write" },
      { tool: "send_message", target: "slack:T1/C1", access: "write" },
    ]);
  });

  it("blocks create until an added connection is connected", async () => {
    stubFetch([GMAIL]);
    render(<NewAutomationForm busy={false} onCancel={() => {}} onCreate={vi.fn()} />);
    await waitFor(() => expect(screen.getByTestId("na-add-conn")).toBeTruthy());
    fireEvent.change(screen.getByTestId("na-add-conn"), { target: { value: "gmail" } });
    fillBasics();
    expect(screen.getByTestId("na-connect-gmail")).toBeTruthy();
    expect((screen.getByTestId("na-create") as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByTestId("na-create-hint").textContent).toContain("Gmail");
  });
});
