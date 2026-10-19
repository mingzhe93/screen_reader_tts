// Image attachments for the Assistant composer: pick, paste or drop a picture, shrink it so
// the long edge is at most 1024 px, and return it as a data URL the backend sends to the model.

export const MAX_ATTACHED_IMAGES = 4;
export const MAX_IMAGE_EDGE = 1024;
const ACCEPTED_TYPES = ["image/png", "image/jpeg", "image/webp"];

/** Value for an `<input type="file" accept>`. */
export const IMAGE_ACCEPT = ACCEPTED_TYPES.join(",");

export type PreparedImages = {
  /** Data URLs of the images that were accepted, in input order. */
  images: string[];
  /** Readable problems, one per file that was refused. */
  errors: string[];
};

/** The image files in a list of files (from a picker, a paste or a drop). */
export function imageFilesOf(files: ArrayLike<File> | null | undefined): File[] {
  return Array.from(files ?? []).filter((file) => file.type.startsWith("image/"));
}

function describe(file: File): string {
  return file.name ? `"${file.name}"` : "The pasted image";
}

/** Scales (never up) so the long edge is at most `MAX_IMAGE_EDGE`. */
function targetSize(width: number, height: number): { width: number; height: number } {
  const longEdge = Math.max(width, height);
  if (longEdge <= MAX_IMAGE_EDGE) return { width, height };
  const scale = MAX_IMAGE_EDGE / longEdge;
  return { width: Math.max(1, Math.round(width * scale)), height: Math.max(1, Math.round(height * scale)) };
}

/**
 * Decodes one PNG, JPEG or WebP file and returns it downscaled as a data URL.
 * PNG stays PNG; everything else becomes JPEG at quality 0.9. Throws an Error with a message
 * fit to show the user.
 */
export async function prepareImage(file: File): Promise<string> {
  if (!ACCEPTED_TYPES.includes(file.type)) {
    throw new Error(`${describe(file)} is not a PNG, JPEG or WebP image.`);
  }
  let bitmap: ImageBitmap;
  try {
    bitmap = await createImageBitmap(file);
  } catch {
    throw new Error(`${describe(file)} could not be read as an image.`);
  }
  try {
    const { width, height } = targetSize(bitmap.width, bitmap.height);
    const canvas = document.createElement("canvas");
    canvas.width = width;
    canvas.height = height;
    const context = canvas.getContext("2d");
    if (!context) throw new Error("This window cannot process images.");
    const asPng = file.type === "image/png";
    if (!asPng) {
      // JPEG has no transparency; without this a transparent WebP would turn black.
      context.fillStyle = "#ffffff";
      context.fillRect(0, 0, width, height);
    }
    context.drawImage(bitmap, 0, 0, width, height);
    return asPng ? canvas.toDataURL("image/png") : canvas.toDataURL("image/jpeg", 0.9);
  } finally {
    bitmap.close();
  }
}

/**
 * Prepares as many of `files` as fit beside the `alreadyAttached` images.
 * Files are handled one after another to keep memory use low.
 */
export async function prepareImages(files: File[], alreadyAttached: number): Promise<PreparedImages> {
  const result: PreparedImages = { images: [], errors: [] };
  let room = MAX_ATTACHED_IMAGES - alreadyAttached;
  for (const file of files) {
    if (room <= 0) {
      result.errors.push(`You can attach up to ${MAX_ATTACHED_IMAGES} images per message.`);
      break;
    }
    try {
      result.images.push(await prepareImage(file));
      room -= 1;
    } catch (error) {
      result.errors.push(error instanceof Error ? error.message : String(error));
    }
  }
  return result;
}
