import type { network } from "../../api/generated/index.ts";
import { CONTROL_API_PATH } from "../../api/paths.ts";

export function httpRouteUrl(prefix: network.HostnamePrefix): string {
  const url = new URL(CONTROL_API_PATH, window.location.origin);
  if (CONTROL_API_PATH.startsWith("/.tascarrel/")) {
    url.hostname = url.hostname.slice(url.hostname.indexOf(".") + 1);
  }
  return prefixedOrigin(url, prefix);
}

function prefixedOrigin(url: URL, prefix: network.HostnamePrefix): string {
  url.hostname = `${prefix}.${url.hostname}`;
  url.pathname = "/";
  url.search = "";
  url.hash = "";
  return url.toString();
}
