export interface JsonBracketPair {
  openLine: number;
  closeLine: number;
}

export interface JsonStringValue {
  path: string;
  line: number;
  startColumn: number;
  endColumn: number;
}

export interface JsonStructure {
  pairs: JsonBracketPair[];
  strings: JsonStringValue[];
}

// The characters of a masked string value, quotes excluded. `startColumn`/`endColumn` are
// 1-based and half-open the way monaco's Range is, so an empty string yields an empty span
// and is dropped.
export interface MaskedSpan {
  path: string;
  line: number;
  startColumn: number;
  endColumn: number;
}

const ARRAY_INDEX = /\[\d+\]/g;

// `devices[].token` matches `devices[0].token` and `devices[7].token`. Written as a
// separate function because it is the half of the masking that is worth testing: it
// decides which characters get covered, and covering one column too many is how a masked
// value turns into a line that renders as nothing at all.
export const isMaskedPath = (path: string, patterns: string[]): boolean =>
  patterns.includes(path) || patterns.includes(path.replace(ARRAY_INDEX, "[]"));

export function maskedSpans(structure: JsonStructure, patterns: string[]): MaskedSpan[] {
  if (patterns.length === 0) return [];
  return structure.strings
    .filter(value => isMaskedPath(value.path, patterns))
    .map(value => ({
      path: value.path,
      line: value.line,
      // scanJson reports the span including both quotes; the mask covers the contents.
      startColumn: value.startColumn + 1,
      endColumn: value.endColumn - 1,
    }))
    .filter(span => span.endColumn > span.startColumn);
}

interface Frame {
  kind: "object" | "array";
  key: string | null;
  index: number;
}

function pathOf(stack: Frame[]): string {
  let path = "";
  for (const frame of stack) {
    if (frame.kind === "array") {
      path += `[${frame.index}]`;
    } else {
      path += path ? `.${frame.key ?? ""}` : (frame.key ?? "");
    }
  }
  return path;
}

let cachedText: string | null = null;
let cachedStructure: JsonStructure = { pairs: [], strings: [] };

export function scanJson(text: string): JsonStructure {
  if (text === cachedText) return cachedStructure;
  const structure = scanJsonUncached(text);
  cachedText = text;
  cachedStructure = structure;
  return structure;
}

function scanJsonUncached(text: string): JsonStructure {
  const pairs: JsonBracketPair[] = [];
  const strings: JsonStringValue[] = [];
  const stack: Frame[] = [];
  const openLines: number[] = [];

  let i = 0;
  let line = 1;
  let lineStart = 0;

  while (i < text.length) {
    const c = text[i];

    if (c === "\n") {
      line++;
      i++;
      lineStart = i;
      continue;
    }

    if (c === "{" || c === "[") {
      openLines.push(line);
      stack.push({ kind: c === "{" ? "object" : "array", key: null, index: 0 });
      i++;
      continue;
    }

    if (c === "}" || c === "]") {
      const openLine = openLines.pop();
      if (openLine !== undefined && line > openLine) pairs.push({ openLine, closeLine: line });
      stack.pop();
      i++;
      continue;
    }

    if (c === ",") {
      const top = stack[stack.length - 1];
      if (top?.kind === "array") top.index++;
      else if (top) top.key = null;
      i++;
      continue;
    }

    if (c === '"') {
      const startColumn = i - lineStart + 1;
      const contentStart = i + 1;
      i++;
      let closed = false;
      while (i < text.length) {
        const ch = text[i];
        if (ch === "\\") {
          i += 2;
          continue;
        }
        if (ch === "\n") break;
        i++;
        if (ch === '"') {
          closed = true;
          break;
        }
      }
      if (i > text.length) i = text.length;
      const endColumn = i - lineStart + 1;
      const top = stack[stack.length - 1];
      if (top?.kind === "object" && top.key === null) {
        top.key = text.slice(contentStart, closed ? i - 1 : i);
      } else {
        strings.push({ path: pathOf(stack), line, startColumn, endColumn });
      }
      continue;
    }

    i++;
  }

  return { pairs, strings };
}
