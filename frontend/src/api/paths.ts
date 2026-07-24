const DEFAULT_API_ROOT = "/api/v1";

const apiRoot = (import.meta.env.VITE_TASCARREL_API_ROOT || DEFAULT_API_ROOT).replace(/\/+$/, "");

export const CONTROL_API_PATH = `${apiRoot}/control`;
export const CHAT_ATTACHMENT_UPLOAD_API_PATH = `${apiRoot}/chat/upload-attachment`;
export const CHAT_ATTACHMENT_API_PATH = `${apiRoot}/chat/attachment`;
export const RAW_FILES_API_PATH = `${apiRoot}/files/raw`;
