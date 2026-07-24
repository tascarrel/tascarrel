import type { chats, workspaces } from "./generated/index.ts";
import {
  CHAT_ATTACHMENT_API_PATH,
  CHAT_ATTACHMENT_UPLOAD_API_PATH,
} from "./paths.ts";

const CHAT_ATTACHMENT_UPLOAD_PROOF = "tascarrel-chat-attachment";

export function chatAttachmentUrl(
  workspace: workspaces.WorkspaceName,
  attachmentId: chats.ChatAttachmentId,
): string {
  const url = new URL(CHAT_ATTACHMENT_API_PATH, window.location.href);
  url.searchParams.set("workspace", String(workspace));
  url.searchParams.set("attachmentId", String(attachmentId));
  return url.href;
}

export async function uploadChatAttachment(
  workspace: workspaces.WorkspaceName,
  file: File,
): Promise<chats.ChatPromptAttachment> {
  const url = new URL(CHAT_ATTACHMENT_UPLOAD_API_PATH, window.location.href);
  url.searchParams.set("workspace", String(workspace));
  url.searchParams.set("name", file.name);
  const response = await fetch(url, {
    method: "POST",
    headers: {
      "Content-Type": file.type || inferredMediaType(file.name),
      "X-Tascarrel-Request": CHAT_ATTACHMENT_UPLOAD_PROOF,
    },
    body: file,
  });
  if (!response.ok) {
    const body = (await response.text()).trim();
    let message = body;
    try {
      const parsed = JSON.parse(body) as { message?: unknown };
      if (typeof parsed.message === "string") message = parsed.message;
    } catch {
      // Non-JSON gateway errors are already suitable for display.
    }
    throw new Error(message || `Attachment upload failed with status ${response.status}`);
  }
  return (await response.json()) as chats.ChatPromptAttachment;
}

function inferredMediaType(name: string): string {
  const extension = name.split(".").pop()?.toLowerCase();
  switch (extension) {
    case "avif":
      return "image/avif";
    case "bmp":
      return "image/bmp";
    case "gif":
      return "image/gif";
    case "jpeg":
    case "jpg":
      return "image/jpeg";
    case "png":
      return "image/png";
    case "webp":
      return "image/webp";
    case "md":
    case "markdown":
      return "text/markdown";
    case "txt":
    case "log":
      return "text/plain";
    case "pdf":
      return "application/pdf";
    case "json":
      return "application/json";
    case "csv":
      return "text/csv";
    default:
      return "application/octet-stream";
  }
}
