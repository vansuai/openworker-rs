import { describe, expect, it } from "vitest";
import { coerceTodoArray } from "./todoUtils";

describe("coerceTodoArray", () => {
  it("accepts a direct array", () => {
    expect(coerceTodoArray([{ content: "a", status: "pending" }])).toHaveLength(1);
  });

  it("unwraps MiniMax item wrapper", () => {
    expect(
      coerceTodoArray({ item: [{ content: "a", status: "pending" }] }),
    ).toHaveLength(1);
  });

  it("returns empty for non-array input", () => {
    expect(coerceTodoArray(null)).toEqual([]);
    expect(coerceTodoArray({})).toEqual([]);
  });
});
