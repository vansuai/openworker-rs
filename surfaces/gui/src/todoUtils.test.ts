import { describe, expect, it } from "vitest";
import { coerceTodoArray, coerceTodoItems } from "./todoUtils";

describe("coerceTodoArray", () => {
  it("accepts a direct array", () => {
    expect(coerceTodoArray([{ content: "a", status: "pending" }])).toHaveLength(1);
  });

  it("unwraps MiniMax item wrapper", () => {
    expect(
      coerceTodoArray({ item: [{ content: "a", status: "pending" }] }),
    ).toHaveLength(1);
  });

  // Each replayed turn adds another {"item": …} level, so histories pile up to depth 6.
  it("unwraps nested item wrappers", () => {
    for (let depth = 1; depth <= 6; depth++) {
      let value: unknown = [{ content: "a", status: "pending" }];
      for (let i = 0; i < depth; i++) value = { item: value };
      expect(coerceTodoArray(value)).toHaveLength(1);
    }
  });

  it("returns empty for non-array input", () => {
    expect(coerceTodoArray(null)).toEqual([]);
    expect(coerceTodoArray({})).toEqual([]);
    expect(coerceTodoArray("")).toEqual([]);
  });
});

describe("coerceTodoItems", () => {
  it("reads the todos array", () => {
    expect(coerceTodoItems({ todos: [{ content: "a", status: "pending" }] })).toHaveLength(1);
  });

  it("reads a wrapped todos array", () => {
    expect(
      coerceTodoItems({ todos: { item: [{ content: "a", status: "done" }] } }),
    ).toHaveLength(1);
  });

  // MiniMax flattens a one-item list onto the top level and leaves todos as "".
  it("reads a flattened single item", () => {
    const items = coerceTodoItems({
      activeForm: "Writing the briefing",
      content: "Curate the briefing",
      item: "",
      status: "in_progress",
      todos: "",
    });
    expect(items).toEqual([{ content: "Curate the briefing", status: "in_progress" }]);
  });

  it("returns empty when there is nothing to read", () => {
    expect(coerceTodoItems({ todos: "", status: "pending" })).toEqual([]);
    expect(coerceTodoItems(null)).toEqual([]);
  });
});
