import { commands } from "@/lib/bindings";

export const AVATAR_OUTPUT_SIZE = 256;

const bytesToBase64 = (bytes: Uint8Array): string => {
  // chunked: `String.fromCharCode.apply` on a 50 KB argument list overflows the call stack
  // in the macOS 10.15 WebKit
  let binary = "";
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    binary += String.fromCharCode.apply(null, Array.from(bytes.subarray(i, i + CHUNK)));
  }
  return btoa(binary);
};

export const loadAvatar = async (): Promise<string | null> => {
  try {
    const encoded = await commands.getAvatar();
    if (!encoded) return null;
    return `data:image/png;base64,${encoded}`;
  } catch (e) {
    console.error("getAvatar failed", e);
    return null;
  }
};

export const saveAvatar = async (bytes: Uint8Array): Promise<boolean> => {
  try {
    return await commands.setAvatar(bytesToBase64(bytes));
  } catch (e) {
    console.error("setAvatar failed", e);
    return false;
  }
};

export const clearAvatar = async (): Promise<boolean> => {
  try {
    return await commands.removeAvatar();
  } catch (e) {
    console.error("removeAvatar failed", e);
    return false;
  }
};
