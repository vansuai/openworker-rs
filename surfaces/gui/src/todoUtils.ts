/** Coerce model todo args into an array (direct list, `items` alias, or `{"item":[…]}` wrapper). */
export function coerceTodoArray(raw: unknown): unknown[] {
  let value = raw;
  for (let depth = 0; depth < 8; depth++) {
    if (Array.isArray(value)) return value;
    if (!value || typeof value !== "object") return [];
    // MiniMax wraps array parameters in the XML repeated-element name, and replaying a
    // wrapped call to the vendor adds another level per turn — so unwrap repeatedly.
    const o = value as Record<string, unknown>;
    const keys = Object.keys(o);
    if (!keys.length || !keys.every((k) => ["item", "items", "todos"].includes(k))) return [];
    const child = keys.map((k) => o[k]).find((v) => v !== undefined);
    if (Array.isArray(child)) return child;
    if (!child || typeof child !== "object") return [];
    value = child;
  }
  return [];
}

/**
 * Todo items from a `todo_write` proposal: the array under `todos`/`items`, or a
 * one-item list MiniMax flattened onto the top level of the arguments (`todos: ""`).
 */
export function coerceTodoItems(args: unknown): unknown[] {
  const a = (args && typeof args === "object" ? args : {}) as Record<string, unknown>;
  for (const key of ["todos", "items"]) {
    const arr = coerceTodoArray(a[key]);
    if (arr.length) return arr;
  }
  const content = typeof a.content === "string" ? a.content.trim() : "";
  if (!content) return [];
  return [{ content, status: typeof a.status === "string" ? a.status : "pending" }];
}
