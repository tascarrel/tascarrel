import { useEffect, useState } from "react";

import { hostApi } from "../../api/client.ts";
import type { network } from "../../api/generated/index.ts";

type HttpRouteAccess = {
  url?: string;
  error?: Error;
  pending?: boolean;
};

export async function createHttpRouteTicket(
  prefix: network.HostnamePrefix,
  returnTo = "/",
): Promise<string> {
  const result = await hostApi.execute("auth_CreateHttpRouteTicket", {
    hostnamePrefix: prefix,
    returnTo,
  });
  return result.url;
}

export function useHttpRouteTicket(
  prefix: network.HostnamePrefix | undefined,
  returnTo = "/",
): HttpRouteAccess {
  const [state, setState] = useState<HttpRouteAccess>({});
  useEffect(() => {
    let active = true;
    setState(prefix ? { pending: true } : {});
    if (!prefix) {
      return () => {
        active = false;
      };
    }
    void createHttpRouteTicket(prefix, returnTo)
      .then((url) => {
        if (active) setState({ url });
      })
      .catch((cause) => {
        if (active) setState({ error: cause instanceof Error ? cause : new Error(String(cause)) });
      });
    return () => {
      active = false;
    };
  }, [prefix, returnTo]);
  return state;
}
