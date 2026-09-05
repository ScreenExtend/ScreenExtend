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
