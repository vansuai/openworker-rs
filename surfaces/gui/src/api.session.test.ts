import { afterEach, expect, it, vi } from "vitest";
import { Session } from "./api";

afterEach(() => {
  vi.unstubAllGlobals();
});

/** Minimal WebSocket double: state is driven manually by the test (no real handshake). */
class FakeWebSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  readyState = FakeWebSocket.CONNECTING;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onopen: (() => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  send = vi.fn();
  close = vi.fn();

  constructor(
    public readonly url: string,
    public readonly protocols?: string | string[],
  ) {}
}

function stubSocketEnv() {
  vi.stubGlobal("__COWORKER_API_TOKEN__", "");
  vi.stubGlobal("__COWORKER_WS__", "ws://127.0.0.1:62024");
  vi.stubGlobal("WebSocket", FakeWebSocket);
}

it("close() while CONNECTING defers the close until the handshake opens", () => {
  stubSocketEnv();
  const session = new Session("s1", "/w", "cowork", { onEvent: vi.fn() });
  const ws = (session as unknown as { ws: FakeWebSocket }).ws;

  session.close();
  expect(ws.close).not.toHaveBeenCalled();

  // The deferred handler closes the socket once the handshake completes.
  ws.onopen?.();
  expect(ws.close).toHaveBeenCalledOnce();
  expect(ws.onopen).toBeNull();
});

it("close() while OPEN closes immediately", () => {
  stubSocketEnv();
  const session = new Session("s1", "/w", "cowork", { onEvent: vi.fn() });
  const ws = (session as unknown as { ws: FakeWebSocket }).ws;
  ws.readyState = FakeWebSocket.OPEN;

  session.close();
  expect(ws.close).toHaveBeenCalledOnce();
});

it("close() while CONNECTING never calls ws.close() if the handshake fails on its own", () => {
  stubSocketEnv();
  const session = new Session("s1", "/w", "cowork", { onEvent: vi.fn() });
  const ws = (session as unknown as { ws: FakeWebSocket }).ws;

  session.close();
  // The server rejects the handshake: the socket terminates by itself (onerror→onclose).
  ws.onerror?.();
  ws.onclose?.();
  expect(ws.close).not.toHaveBeenCalled();
});

it("constructor normalizes a \"/\" workspace to empty in the URL", () => {
  stubSocketEnv();
  const session = new Session("s1", "/", "cowork", { onEvent: vi.fn() });
  const ws = (session as unknown as { ws: FakeWebSocket }).ws;
  expect(ws.url).toContain("workspace=&agent=cowork");
});

it("constructor keeps a real workspace path intact", () => {
  stubSocketEnv();
  const session = new Session("s1", "/Users/me/project", "cowork", { onEvent: vi.fn() });
  const ws = (session as unknown as { ws: FakeWebSocket }).ws;
  expect(ws.url).toContain(`workspace=${encodeURIComponent("/Users/me/project")}&agent=cowork`);
});
