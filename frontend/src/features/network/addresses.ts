import type { network } from "../../api/generated/index.ts";
import { CONTROL_API_PATH } from "../../api/paths.ts";

export function httpRouteUrl(prefix: network.HostnamePrefix): string {
  const url = new URL(CONTROL_API_PATH, window.location.origin);
  return prefixedOrigin(url, prefix);
}

export function nestedHttpRouteUrl(prefix: network.HostnamePrefix): string {
  const url = new URL(window.location.origin);
  return prefixedOrigin(url, prefix);
}

function prefixedOrigin(url: URL, prefix: network.HostnamePrefix): string {
  url.hostname = `${prefix}.${url.hostname}`;
  url.pathname = "/";
  url.search = "";
  url.hash = "";
  return url.toString();
}
