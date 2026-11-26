// Turns a malformed `config.json` into the shape the app declares, once, for every consumer.
// A reader treats a value of the wrong container type as absent: it logs one line, uses the
// default and never throws (`devices` stored as an object would otherwise throw at startup).
// The shape comes from `configJsonSchema`, not a second table that could drift. `cli.rs`
// mirrors its container types for the writer; `scripts/test-config-editor.mjs` checks the two.
// Imports only the two plain-data modules, with `.ts` extensions, so bare `node` can load it.
import { defaultConfig, type Config } from "./config-defaults.ts";
import { configJsonSchema } from "./config-schema.ts";

type JsonSchema = Record<string, unknown>;

export interface NormalizedConfig {
  /** The stored config with every declared value in its declared shape. */
  config: Config;
  /** One `[migrate]`-style sentence per value that was not what the config declares. */
  notes: string[];
  /**
   * Top-level keys whose stored value was present and malformed, so the caller can write the
   * corrected value back. An absent key is not listed: rewriting the file for it would churn
   * every config written by an older version.
   */
  repaired: string[];
}

const properties = configJsonSchema.properties as Record<string, JsonSchema>;

/** What a value *is*, in the words `cli.rs` uses for the same sentence. */
export function describe(value: unknown): string {
  if (value === null) return "null";
  if (Array.isArray(value)) return `an array of ${value.length}`;
  switch (typeof value) {
    case "boolean":
      return "a boolean";
    case "number":
      return "a number";
    case "string":
      return "a string";
    case "object": {
      const count = Object.keys(value as object).length;
      return `an object with ${count} key${count === 1 ? "" : "s"}`;
    }
    default:
      return "null";
  }
}

/** What a schema *wants*, in the same words. */
function article(schema: JsonSchema): string {
  switch (schema.type as string) {
    case "string":
      return "a string";
    case "integer":
      return "a whole number";
    case "number":
      return "a number";
    case "boolean":
      return "true or false";
    case "array":
      return "an array";
    case "object":
      return "an object";
    default:
      return "a value";
  }
}

function matchesType(value: unknown, type: string | undefined): boolean {
  switch (type) {
    case "integer":
      return typeof value === "number" && Number.isInteger(value);
    case "number":
      return typeof value === "number" && Number.isFinite(value);
    case "array":
      return Array.isArray(value);
    case "object":
      return typeof value === "object" && value !== null && !Array.isArray(value);
    case undefined:
      return true;
    default:
      return typeof value === type;
  }
}

function clone<T>(value: T): T {
  if (Array.isArray(value)) return value.map(clone) as unknown as T;
  if (value !== null && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value as Record<string, unknown>)) out[k] = clone(v);
    return out as T;
  }
  return value;
}

/**
 * A value of the declared shape to stand in for one that is missing or unusable.
 * `fallback` is the runtime default (`defaultConfig`); rows of `devices[]`/`knownDevices[]`
 * have none, so they fall back to the schema's own `default`.
 */
function defaultFor(schema: JsonSchema, fallback: unknown): unknown {
  if (fallback !== undefined) return clone(fallback);
  if ("default" in schema) return clone(schema.default);
  switch (schema.type as string) {
    case "string":
      return "";
    case "integer":
    case "number":
      return 0;
    case "boolean":
      return false;
    case "array":
      return [];
    case "object": {
      const out: Record<string, unknown> = {};
      const props = (schema.properties as Record<string, JsonSchema> | undefined) ?? {};
      for (const [key, child] of Object.entries(props)) out[key] = defaultFor(child, undefined);
      return out;
    }
    default:
      return null;
  }
}

interface Coerced {
  value: unknown;
  /** Something stored was present and wrong, so the file itself is worth correcting. */
  changed: boolean;
}

function coerce(
  schema: JsonSchema,
  value: unknown,
  path: string,
  fallback: unknown,
  notes: string[]
): Coerced {
  if (!matchesType(value, schema.type as string | undefined)) {
    notes.push(`[migrate] ${path} is not ${article(schema)} (${describe(value)}); ignoring it`);
    return { value: defaultFor(schema, fallback), changed: true };
  }

  const allowed = schema.enum as unknown[] | undefined;
  if (allowed && !allowed.includes(value)) {
    notes.push(
      `[migrate] ${path} is not one of ${allowed.map(v => JSON.stringify(v)).join(", ")} ` +
        `(${JSON.stringify(value)}); ignoring it`
    );
    return { value: defaultFor(schema, fallback), changed: true };
  }

  if (Array.isArray(value) && schema.items) {
    const items = schema.items as JsonSchema;
    const rows: unknown[] = [];
    let changed = false;
    value.forEach((row, i) => {
      // drop only the malformed row, loudly: one bad device must not cost the others their settings
      if (!matchesType(row, items.type as string | undefined)) {
        notes.push(
          `[migrate] ${path}[${i}] is not ${article(items)} (${describe(row)}); dropping it`
        );
        changed = true;
        return;
      }
      const out = coerce(items, row, `${path}[${i}]`, undefined, notes);
      changed = changed || out.changed;
      rows.push(out.value);
    });
    return { value: rows, changed };
  }

  if (matchesType(value, "object") && schema.properties) {
    const props = schema.properties as Record<string, JsonSchema>;
    const record = value as Record<string, unknown>;
    const fallbackRecord = (fallback ?? {}) as Record<string, unknown>;
    // keys the schema does not name pass through: never delete somebody's hand-written key here
    const out: Record<string, unknown> = { ...record };
    let changed = false;
    for (const [key, child] of Object.entries(props)) {
      const childPath = `${path}.${key}`;
      const childFallback = fallbackRecord[key];
      if (!(key in record) || record[key] === undefined) {
        // absent is not malformed: fill it (`undefined` where a number belongs is rejected at
        // the IPC boundary) but do not mark the file as changed
        const filled = defaultFor(child, childFallback);
        if (filled !== undefined) out[key] = filled;
        continue;
      }
      const result = coerce(child, record[key], childPath, childFallback, notes);
      out[key] = result.value;
      changed = changed || result.changed;
    }
    return { value: out, changed };
  }

  return { value, changed: false };
}

/**
 * Read a raw store snapshot as a `Config`.
 *
 * Never throws, whatever is in it, including every top-level key being the wrong type at once:
 * a malformed setting is not one of the things allowed to stop the app (`startup-gate.ts`).
 */
export function normalizeConfig(raw: Record<string, unknown>): NormalizedConfig {
  const notes: string[] = [];
  const repaired: string[] = [];
  const config: Record<string, unknown> = {};

  for (const key of Object.keys(defaultConfig)) {
    const fallback = (defaultConfig as Record<string, unknown>)[key];
    const schema = properties[key];
    const stored = raw[key];
    if (stored === undefined) {
      config[key] = clone(fallback);
      continue;
    }
    if (!schema) {
      // no schema entry (`test-config-editor.mjs` fails on that): keep the stored value anyway
      config[key] = stored;
      continue;
    }
    const result = coerce(schema, stored, key, fallback, notes);
    config[key] = result.value;
    if (result.changed) repaired.push(key);
  }

  return { config: config as Config, notes, repaired };
}
