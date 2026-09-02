/** Coerce model todo args into an array (direct list, `items` alias, or `{"item":[...]}` wrapper). */
export function coerceTodoArray(raw: unknown): unknown[] {
  if (Array.isArray(raw)) return raw;
  if (raw && typeof raw === "object") {
    const o = raw as Record<string, unknown>;
    for (const k of ["item", "items", "todos"]) {
      if (Array.isArray(o[k])) return o[k] as unknown[];
    }
  }
  return [];
}
