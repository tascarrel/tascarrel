const DEFAULT_API_ROOT = "/api/v1";

declare global {
  interface Window {
    __TASCARREL_API_ROOT__?: string;
  }
}

const apiRoot = (
  window.__TASCARREL_API_ROOT__
  || DEFAULT_API_ROOT
).replace(/\/+$/, "");

export const CONTROL_API_PATH = `${apiRoot}/control`;
export const AUTH_SESSION_API_PATH = `${apiRoot}/auth/session`;
export const CHAT_ATTACHMENT_UPLOAD_API_PATH = `${apiRoot}/chat/upload-attachment`;
export const CHAT_ATTACHMENT_API_PATH = `${apiRoot}/chat/attachment`;
export const RAW_FILES_API_PATH = `${apiRoot}/files/raw`;
