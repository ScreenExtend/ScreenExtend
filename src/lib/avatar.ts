import { commands } from "@/lib/bindings";

export const AVATAR_OUTPUT_SIZE = 256;

export const blobToDataUrl = (blob: Blob): Promise<string> =>
  new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result as string);
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(blob);
  });

export const loadAvatar = async (): Promise<string | null> => {
  try {
    const bytes = await commands.getAvatar();
    if (!bytes || bytes.length === 0) return null;
    return await blobToDataUrl(
      new Blob([new Uint8Array(bytes)], { type: "image/png" }),
    );
  } catch (e) {
    console.error("getAvatar failed", e);
    return null;
  }
};

export const saveAvatar = async (bytes: Uint8Array): Promise<boolean> => {
  try {
    return await commands.setAvatar(Array.from(bytes));
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
