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
  const bytes = await commands.getAvatar();
  if (!bytes || bytes.length === 0) return null;
  return blobToDataUrl(new Blob([new Uint8Array(bytes)], { type: "image/png" }));
};

export const saveAvatar = (bytes: Uint8Array): Promise<boolean> =>
  commands.setAvatar(Array.from(bytes));

export const clearAvatar = (): Promise<boolean> => commands.removeAvatar();
