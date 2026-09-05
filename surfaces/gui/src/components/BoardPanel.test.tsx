import { describe, expect, it } from "vitest";
import { boardSummary } from "./BoardPanel";
import type { Board } from "../api";

describe("boardSummary", () => {
  it("summarizes active counts", () => {
    const board: Board = {
      space: "acme",
      name: "acme",
      items: [
        {
          id: 1,
          title: "a",
          description: "",
          criteria: "",
          state: "blocked",
          assignee: "w1",
          creator: "lead",
          refs: [],
          links: [],
        },
        {
          id: 2,
          title: "b",
          description: "",
          criteria: "",
          state: "review",
          assignee: "w2",
          creator: "lead",
          refs: [],
          links: [],
        },
        {
          id: 3,
          title: "c",
          description: "",
          criteria: "",
          state: "open",
          assignee: "",
          creator: "lead",
          refs: [],
          links: [],
        },
      ],
    };
    const text = boardSummary(board);
    expect(text).toContain("blocked");
    expect(text).toContain("review");
    expect(text).toContain("open");
  });
});
