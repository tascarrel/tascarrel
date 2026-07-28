import { Link } from "@tanstack/react-router";
import {
  Check,
  Copy,
  ExternalLink,
  ShieldCheck,
  Trash2,
} from "lucide-react";
import { useEffect, useId, useState, type ReactNode } from "react";

import { hostApi } from "../../api/client.ts";
import type { network, pods, workspaces } from "../../api/generated/index.ts";
import { Badge, type BadgeTone } from "../../components/ui/Badge.tsx";
import { Button } from "../../components/ui/Button.tsx";
import { ConfirmDialog } from "../../components/ui/ConfirmDialog.tsx";
import { SidebarTabs, SidebarTabsPanel } from "../../components/ui/SidebarTabs.tsx";
import { httpRouteUrl } from "./addresses.ts";
import { createHttpRouteTicket } from "./routeAccess.ts";
import { HostPodForwardForm, HttpRouteForm, PodHostForwardForm } from "./NetworkForms.tsx";
import {
  useDnsRequests,
  useHttpRoutes,
  usePodHostForwards,
  usePortForwards,
  useTcpFlows,
} from "./state.ts";

type DeleteTarget =
  | { type: "route"; value: network.HttpRoute }
  | { type: "forward"; value: network.PortForward }
  | { type: "pod-host-forward"; value: network.PodHostForward };

export function NetworkView({
  workspace,
  pods: workspacePods,
  podTitlesById,
}: {
  workspace: workspaces.WorkspaceName;
  pods: readonly pods.Pod[];
  podTitlesById?: ReadonlyMap<pods.PodId, string>;
}) {
  const routeState = useHttpRoutes(workspace);
  const forwardState = usePortForwards(workspace);
  const podHostForwardState = usePodHostForwards(workspace);
  const dnsState = useDnsRequests(workspace);
  const tcpState = useTcpFlows(workspace);
  const routes = routeState.value?.httpRoutes ?? [];
  const forwards = forwardState.value?.portForwards ?? [];
  const podHostForwards = podHostForwardState.value?.podHostForwards ?? [];
  const dnsRequests = dnsState.value?.requests ?? [];
  const tcpFlows = correlateTcpFlows(tcpState.value?.events ?? []);
  const [deleteTarget, setDeleteTarget] = useState<DeleteTarget>();
  const [deleting, setDeleting] = useState(false);
  const [trustTarget, setTrustTarget] = useState<network.HttpRoute>();
  const [settingRouteTrust, setSettingRouteTrust] = useState(false);
  const [copied, setCopied] = useState<string>();
  const [actionError, setActionError] = useState<string>();

  useEffect(() => {
    setDeleteTarget(undefined);
    setTrustTarget(undefined);
    setCopied(undefined);
    setActionError(undefined);
  }, [workspace]);

  useEffect(() => {
    if (!copied) return;
    const timeout = window.setTimeout(() => setCopied(undefined), 2_000);
    return () => window.clearTimeout(timeout);
  }, [copied]);

  const deleteNetworkTarget = async () => {
    if (!deleteTarget || deleting) return;
    setDeleting(true);
    setActionError(undefined);
    try {
      if (deleteTarget.type === "route") {
        await hostApi.execute("network_DeleteHttpRoute", {
          httpRouteId: deleteTarget.value.id,
        });
      } else if (deleteTarget.type === "forward") {
        await hostApi.execute("network_DeletePortForward", {
          portForwardId: deleteTarget.value.id,
        });
      } else {
        await hostApi.execute("network_DeletePodHostForward", {
          podHostForwardId: deleteTarget.value.id,
        });
      }
      setDeleteTarget(undefined);
    } catch (cause) {
      setActionError(errorMessage(cause));
    } finally {
      setDeleting(false);
    }
  };

  const updateRouteTrust = async () => {
    if (!trustTarget || settingRouteTrust) return;
    setSettingRouteTrust(true);
    setActionError(undefined);
    try {
      await hostApi.execute("network_SetHttpRouteTrustedTascarrelFrontend", {
        httpRouteId: trustTarget.id,
        trustedTascarrelFrontend: !trustTarget.trustedTascarrelFrontend,
      });
      setTrustTarget(undefined);
    } catch (cause) {
      setActionError(errorMessage(cause));
    } finally {
      setSettingRouteTrust(false);
    }
  };

  const copyAddress = async (address: string) => {
    setActionError(undefined);
    try {
      await navigator.clipboard.writeText(address);
      setCopied(address);
    } catch (cause) {
      setActionError(`Could not copy the address: ${errorMessage(cause)}`);
    }
  };

  const openRoute = async (route: network.HttpRoute) => {
    const popup = window.open("", "_blank");
    if (popup) popup.opener = null;
    try {
      const url = await createHttpRouteTicket(route.hostnamePrefix);
      if (popup) popup.location.replace(url);
      else window.location.assign(url);
    } catch (cause) {
      popup?.close();
      setActionError(`Could not open the HTTP route: ${errorMessage(cause)}`);
    }
  };

  const visibleError = actionError
    ?? routeState.error?.message
    ?? forwardState.error?.message
    ?? podHostForwardState.error?.message
    ?? dnsState.error?.message
    ?? tcpState.error?.message;
  const noPodDetail = workspacePods.length === 0
    ? "Start the workspace to discover pods and create new network entries. Existing host-owned entries remain available below."
    : undefined;
  const formKey = `${workspace}:${workspacePods.map((pod) => pod.id).join(",")}`;

  return (
    <div className="flex h-full min-h-0 flex-col overflow-hidden bg-canvas text-foreground">
      <span className="sr-only" role="status">
        {copied ? `Copied ${copied}` : ""}
      </span>

      {visibleError ? (
        <p className="border-b border-red-500/20 bg-red-500/5 px-5 py-2.5 text-xs text-red-200" role="alert">
          {visibleError}
        </p>
      ) : null}
      {noPodDetail ? (
        <p className="border-b border-amber-500/20 bg-amber-500/5 px-5 py-2.5 text-xs text-amber-200">
          {noPodDetail}
        </p>
      ) : null}

      <SidebarTabs
        ariaLabel="Network sections"
        defaultValue="http-routes"
        items={[
          {
            value: "http-routes",
            label: "HTTP Routes",
          },
          {
            value: "pod-host",
            label: "Pod → Host",
          },
          {
            value: "host-pod",
            label: "Host → Pod",
          },
          {
            value: "dns-requests",
            label: "DNS Requests",
          },
          {
            value: "tcp-flows",
            label: "TCP Flows",
          },
        ]}
      >
        <SidebarTabsPanel
          contentClassName="max-w-6xl"
          value="http-routes"
        >
          <NetworkPage
            title="HTTP Routes"
            detail="Issue hostnames that proxy browser traffic to services running in workspace pods."
            count={routes.length}
            connection={routeState.connection}
          >
            <NetworkSettingsSection title="Add Route">
              <HttpRouteForm
                key={`http-route:${formKey}`}
                workspace={workspace}
                pods={workspacePods}
              />
            </NetworkSettingsSection>

            <NetworkSettingsSection className="mt-8" title="Existing Routes">
              <NetworkCollection>
                <NetworkList
                  ready={routeState.ready}
                  empty="No HTTP routes have been created for this workspace."
                >
                  {routes.map((route) => {
                    const address = httpRouteUrl(route.hostnamePrefix);
                    return (
                      <li className="px-4 py-3" key={route.id}>
                        <div className="flex flex-wrap items-start justify-between gap-3">
                          <TargetSummary
                            title={route.title}
                            podId={route.podId}
                            podPort={route.podPort}
                            workspace={workspace}
                            podTitlesById={podTitlesById}
                          />
                          <div className="flex items-center gap-1.5">
                            {route.trustedTascarrelFrontend
                              ? <Badge size="xs" tone="success">Tascarrel frontend</Badge>
                              : null}
                            {route.internal ? <Badge size="xs">Internal</Badge> : null}
                            <Badge size="xs" tone="primary">HTTP</Badge>
                          </div>
                        </div>
                        <div className="mt-2 flex min-w-0 flex-wrap items-center gap-2">
                          <code className="min-w-0 flex-1 break-all font-mono text-[10px] leading-5 text-muted">
                            {address}
                          </code>
                          <Button
                            size="icon"
                            className="size-8"
                            variant={route.trustedTascarrelFrontend ? "primary" : "muted"}
                            aria-label={route.trustedTascarrelFrontend
                              ? `Remove Tascarrel frontend trust from ${address}`
                              : `Trust ${address} as the Tascarrel frontend`}
                            aria-pressed={route.trustedTascarrelFrontend}
                            title={route.trustedTascarrelFrontend
                              ? "Remove Tascarrel frontend trust"
                              : "Trust as Tascarrel frontend"}
                            disabled={settingRouteTrust}
                            onClick={() => setTrustTarget(route)}
                          >
                            <ShieldCheck aria-hidden="true" className="size-3.5" />
                          </Button>
                          <AddressActions
                            address={address}
                            copied={copied === address}
                            open
                            onCopy={() => void copyAddress(address)}
                            onOpen={() => void openRoute(route)}
                            onDelete={() => setDeleteTarget({ type: "route", value: route })}
                          />
                        </div>
                      </li>
                    );
                  })}
                </NetworkList>
              </NetworkCollection>
            </NetworkSettingsSection>
          </NetworkPage>
        </SidebarTabsPanel>

        <SidebarTabsPanel
          contentClassName="max-w-6xl"
          value="host-pod"
        >
          <NetworkPage
            title="Host → Pod"
            detail="Expose a service in a selected pod to the host through a dynamic host-loopback port."
            count={forwards.length}
            connection={forwardState.connection}
          >
            <NetworkSettingsSection title="Add Forward">
              <HostPodForwardForm
                key={`host-pod:${formKey}`}
                workspace={workspace}
                pods={workspacePods}
              />
            </NetworkSettingsSection>

            <NetworkSettingsSection className="mt-8" title="Existing Forwards">
              <NetworkCollection>
                <NetworkList
                  ready={forwardState.ready}
                  empty="No host-to-pod forwards have been created for this workspace."
                >
                  {forwards.map((forward) => {
                    const address = `127.0.0.1:${forward.hostPort}`;
                    return (
                      <li className="px-4 py-3" key={forward.id}>
                        <div className="flex flex-wrap items-start justify-between gap-3">
                          <TargetSummary
                            title={forward.title}
                            podId={forward.podId}
                            podPort={forward.podPort}
                            workspace={workspace}
                            podTitlesById={podTitlesById}
                          />
                          <Badge size="xs" tone="success">TCP</Badge>
                        </div>
                        <div className="mt-2 flex min-w-0 flex-wrap items-center gap-2">
                          <code className="min-w-0 flex-1 break-all font-mono text-[10px] leading-5 text-muted">
                            {address}
                          </code>
                          <AddressActions
                            address={address}
                            copied={copied === address}
                            onCopy={() => void copyAddress(address)}
                            onDelete={() => setDeleteTarget({ type: "forward", value: forward })}
                          />
                        </div>
                      </li>
                    );
                  })}
                </NetworkList>
              </NetworkCollection>
            </NetworkSettingsSection>
          </NetworkPage>
        </SidebarTabsPanel>

        <SidebarTabsPanel
          contentClassName="max-w-6xl"
          value="pod-host"
        >
          <NetworkPage
            title="Pod → Host"
            detail="Expose host-loopback services to a selected pod through host.tascarrel.internal."
            count={podHostForwards.length}
            connection={podHostForwardState.connection}
          >
            <NetworkSettingsSection title="Add Forward">
              <PodHostForwardForm
                key={`pod-host:${formKey}`}
                workspace={workspace}
                pods={workspacePods}
              />
            </NetworkSettingsSection>

            <NetworkSettingsSection className="mt-8" title="Existing Forwards">
              <NetworkCollection>
                <NetworkList
                  ready={podHostForwardState.ready}
                  empty="No pod-to-host forwards have been created for this workspace."
                >
                  {podHostForwards.map((forward) => {
                    const mapping = parsePortMapping(String(forward.mapping));
                    if (!mapping) return null;
                    const podAddress = `host.tascarrel.internal:${mapping.podVisiblePort}`;
                    return (
                      <li className="px-4 py-3" key={forward.id}>
                        <div className="flex flex-wrap items-start justify-between gap-3">
                          <TargetSummary
                            title={forward.title}
                            podId={forward.podId}
                            podPort={mapping.podVisiblePort}
                            workspace={workspace}
                            podTitlesById={podTitlesById}
                          />
                          <Badge size="xs" tone="primary">POD → HOST</Badge>
                        </div>
                        <div className="mt-2 flex min-w-0 flex-wrap items-center gap-2">
                          <code className="min-w-0 flex-1 break-all font-mono text-[10px] leading-5 text-muted">
                            {podAddress} → 127.0.0.1:{mapping.hostPort}
                          </code>
                          <AddressActions
                            address={podAddress}
                            copied={copied === podAddress}
                            onCopy={() => void copyAddress(podAddress)}
                            onDelete={() => setDeleteTarget({ type: "pod-host-forward", value: forward })}
                          />
                        </div>
                      </li>
                    );
                  })}
                </NetworkList>
              </NetworkCollection>
            </NetworkSettingsSection>
          </NetworkPage>
        </SidebarTabsPanel>

        <SidebarTabsPanel
          contentClassName="max-w-6xl"
          value="dns-requests"
        >
          <NetworkPage
            title="DNS Requests"
            detail="Inspect DNS queries resolved by the host network service for this workspace."
            count={dnsRequests.length}
            connection={dnsState.connection}
          >
            <NetworkCollection>
              <NetworkList
                ready={dnsState.ready}
                empty="No DNS requests have been observed for this workspace."
              >
                {dnsRequests.toReversed().map((request, index) => (
                  <li className="px-4 py-3" key={`${request.occurredAt}-${request.name}-${index}`}>
                    <div className="flex min-w-0 flex-wrap items-center gap-2">
                      <code className="break-all font-mono text-xs text-foreground">{request.name}</code>
                      <Badge className="normal-case" size="xs" tone={dnsOutcomeTone(request.outcome)}>
                        {request.outcome.status === "Response"
                          ? dnsResponseCode(request.outcome.responseCode)
                          : splitLabel(request.outcome.status)}
                      </Badge>
                    </div>
                    <p className="mt-1 break-all text-[10px] text-subtle">
                      {dnsRecordType(request.recordType)} · {request.transport.type.toUpperCase()} · {" "}
                      <NetworkSource
                        source={request.source}
                        workspace={workspace}
                        podTitlesById={podTitlesById}
                      /> · {formatTimestamp(request.occurredAt)} · {" "}
                      {request.outcome.status === "Response"
                        ? <>{request.outcome.answerCount} answer{request.outcome.answerCount === 1 ? "" : "s"} · {" "}</>
                        : null}
                      {request.durationMs} ms
                    </p>
                    {request.outcome.status === "Response" && request.outcome.resolvedAddresses.length > 0 ? (
                      <p
                        className="mt-1 truncate font-mono text-[10px] text-muted"
                        title={resolvedAddressesLabel(request.outcome)}
                      >
                        {resolvedAddressesLabel(request.outcome)}
                      </p>
                    ) : null}
                  </li>
                ))}
              </NetworkList>
            </NetworkCollection>
          </NetworkPage>
        </SidebarTabsPanel>

        <SidebarTabsPanel
          contentClassName="max-w-6xl"
          value="tcp-flows"
        >
          <NetworkPage
            title="TCP Flows"
            detail="Inspect active and completed TCP connections attributed to this workspace."
            count={tcpFlows.length}
            connection={tcpState.connection}
          >
            <NetworkCollection>
              <NetworkList
                ready={tcpState.ready}
                empty="No TCP flows have been observed for this workspace."
              >
                {tcpFlows.map((flow) => (
                  <li className="px-4 py-3" key={flow.tcpFlowId}>
                    <div className="flex min-w-0 flex-wrap items-center gap-2">
                      <code className="break-all font-mono text-xs text-foreground">
                        {flow.started
                          ? tcpFlowDestination(flow.started)
                          : "Start event no longer retained"}
                      </code>
                      <Badge className="normal-case" size="xs" tone={tcpOutcomeTone(flow.ended?.outcome)}>
                        {flow.ended ? splitLabel(flow.ended.outcome.status) : "Active"}
                      </Badge>
                    </div>
                    <p className="mt-1 break-all text-[10px] text-subtle">
                      {flow.started
                        ? (
                            <>
                              {flow.started.mode.type.toUpperCase()} · {" "}
                              <NetworkSource
                                source={flow.started.source}
                                workspace={workspace}
                                podTitlesById={podTitlesById}
                              /> · {flow.started.sourceAddress} · {formatTimestamp(flow.started.occurredAt)}
                            </>
                          )
                        : `Flow ${shortId(flow.tcpFlowId)}`}
                      {flow.ended ? <> · {flow.ended.durationMs} ms</> : null}
                    </p>
                    {flow.started?.effectiveDestination && flow.started.effectiveDestination !== flow.started.requestedDestination ? (
                      <p className="mt-1 break-all font-mono text-[10px] text-muted">
                        via {flow.started.effectiveDestination}
                      </p>
                    ) : null}
                  </li>
                ))}
              </NetworkList>
            </NetworkCollection>
          </NetworkPage>
        </SidebarTabsPanel>
      </SidebarTabs>

      <ConfirmDialog
        open={Boolean(trustTarget)}
        pending={settingRouteTrust}
        title={trustTarget?.trustedTascarrelFrontend
          ? "Remove Tascarrel Frontend Trust?"
          : "Trust This Tascarrel Frontend?"}
        description={trustTarget?.trustedTascarrelFrontend
          ? "Browsers at this route will immediately lose access to the Tascarrel API."
          : "Browsers at this route's exact origin will receive full Tascarrel API access. This replaces any previously trusted frontend route."}
        confirmLabel={trustTarget?.trustedTascarrelFrontend ? "Remove trust" : "Trust frontend"}
        onOpenChange={(open) => {
          if (!open) setTrustTarget(undefined);
        }}
        onConfirm={() => void updateRouteTrust()}
      />

      <ConfirmDialog
        open={Boolean(deleteTarget)}
        pending={deleting}
        destructive
        title={deleteTarget?.type === "route"
          ? "Delete HTTP Route?"
          : deleteTarget?.type === "pod-host-forward"
            ? "Delete Pod-to-Host Forward?"
            : "Delete Host-to-Pod Forward?"}
        description={deleteTarget?.type === "route"
          ? "The issued hostname will immediately stop routing to this pod."
          : deleteTarget?.type === "pod-host-forward"
            ? "The selected pod will immediately lose access to this host-loopback mapping."
            : "The host listener and all connections using this forward will be closed."}
        confirmLabel={deleteTarget?.type === "route" ? "Delete route" : "Delete forward"}
        onOpenChange={(open) => {
          if (!open) setDeleteTarget(undefined);
        }}
        onConfirm={() => void deleteNetworkTarget()}
      />
    </div>
  );
}

function NetworkPage({
  title,
  detail,
  count,
  connection,
  children,
}: {
  title: string;
  detail: string;
  count: number;
  connection: "idle" | "connecting" | "live" | "reconnecting";
  children: ReactNode;
}) {
  return (
    <section>
      <header className="mb-5">
        <h2 className="text-sm font-semibold text-foreground">{title}</h2>
        <p className="mt-1 text-xs leading-5 text-subtle">{detail}</p>
        <span className="sr-only">
          {connectionLabel(connection)}. {count} {count === 1 ? "entry" : "entries"}.
        </span>
      </header>
      {children}
    </section>
  );
}

function NetworkSettingsSection({
  title,
  className = "",
  children,
}: {
  title: string;
  className?: string;
  children: ReactNode;
}) {
  const titleId = useId();
  return (
    <section className={className} aria-labelledby={titleId}>
      <h3 className="text-xs font-medium text-foreground" id={titleId}>{title}</h3>
      <div className="mt-3">{children}</div>
    </section>
  );
}

function NetworkCollection({ children }: { children: ReactNode }) {
  return (
    <div className="overflow-hidden rounded-xl border border-ui-border bg-surface/30">
      {children}
    </div>
  );
}

function NetworkList({
  ready,
  empty,
  children,
}: {
  ready: boolean;
  empty: string;
  children: ReactNode;
}) {
  const hasChildren = Array.isArray(children) ? children.length > 0 : Boolean(children);
  return hasChildren ? (
    <ul className="m-0 list-none divide-y divide-ui-border p-0">{children}</ul>
  ) : (
    <p className="px-4 py-8 text-center text-xs leading-5 text-subtle">
      {ready ? empty : "Loading network entries…"}
    </p>
  );
}

function TargetSummary({
  title,
  podId,
  podPort,
  workspace,
  podTitlesById,
}: {
  title?: string;
  podId: pods.PodId;
  podPort: number;
  workspace: workspaces.WorkspaceName;
  podTitlesById?: ReadonlyMap<pods.PodId, string>;
}) {
  return (
    <div className="min-w-0">
      <p className="truncate text-xs font-medium text-foreground">
        {title || (
          <PodLink podId={podId} workspace={workspace} podTitlesById={podTitlesById} />
        )}
      </p>
      <p className="mt-1 break-all text-[10px] text-subtle">
        {title ? (
          <><PodLink podId={podId} workspace={workspace} podTitlesById={podTitlesById} /> · {" "}</>
        ) : null}
        pod port {podPort}
      </p>
    </div>
  );
}

function NetworkSource({
  source,
  workspace,
  podTitlesById,
}: {
  source: network.NetworkRequestSource;
  workspace: workspaces.WorkspaceName;
  podTitlesById?: ReadonlyMap<pods.PodId, string>;
}) {
  if (source.type === "Pod") {
    return (
      <PodLink podId={source.content} workspace={workspace} podTitlesById={podTitlesById} />
    );
  }
  return source.type === "ImageBuild" ? "Image build" : "Workspace service";
}

function PodLink({
  podId,
  workspace,
  podTitlesById,
}: {
  podId: pods.PodId;
  workspace: workspaces.WorkspaceName;
  podTitlesById?: ReadonlyMap<pods.PodId, string>;
}) {
  const title = podTitlesById?.get(podId);
  if (!title) return <span>Unavailable pod</span>;
  return (
    <Link
      className="font-medium text-accent-text outline-none underline-offset-2 hover:underline focus-visible:underline"
      to="/workspaces/$workspace/pods/$pod"
      params={{ workspace, pod: podId }}
    >
      {title}
    </Link>
  );
}

function AddressActions({
  address,
  copied,
  open = false,
  onCopy,
  onOpen,
  onDelete,
}: {
  address: string;
  copied: boolean;
  open?: boolean;
  onCopy: () => void;
  onOpen?: () => void;
  onDelete: () => void;
}) {
  return (
    <div className="flex items-center gap-1.5">
      <Button
        size="icon"
        className="size-8"
        aria-label={copied ? `Copied ${address}` : `Copy ${address}`}
        title={copied ? "Copied" : "Copy address"}
        onClick={onCopy}
      >
        {copied
          ? <Check aria-hidden="true" className="size-3.5 text-emerald-300" />
          : <Copy aria-hidden="true" className="size-3.5" />}
      </Button>
      {open ? (
        <Button
          size="icon"
          className="size-8"
          aria-label={`Open ${address}`}
          title="Open route"
          onClick={onOpen}
        >
          <ExternalLink aria-hidden="true" className="size-3.5" />
        </Button>
      ) : null}
      <Button size="icon" className="size-8" variant="danger" aria-label={`Delete ${address}`} title="Delete" onClick={onDelete}>
        <Trash2 aria-hidden="true" className="size-3.5" />
      </Button>
    </div>
  );
}

function parsePortMapping(value: string): Readonly<{
  hostPort: number;
  podVisiblePort: number;
}> | undefined {
  const mapping = value.trim();
  const parts = mapping.split(":");
  if (parts.length < 1 || parts.length > 2 || parts.some((part) => part === "")) return undefined;
  const hostPort = Number(parts[0]);
  const podVisiblePort = Number(parts[1] ?? parts[0]);
  if (![hostPort, podVisiblePort].every((port) => Number.isInteger(port) && port >= 1 && port <= 65535)) {
    return undefined;
  }
  return { hostPort, podVisiblePort };
}

function connectionLabel(connection: "idle" | "connecting" | "live" | "reconnecting"): string {
  if (connection === "live") return "Live";
  if (connection === "idle") return "Paused";
  if (connection === "connecting") return "Connecting…";
  return "Reconnecting…";
}

function shortId(id: string): string {
  const value = String(id);
  return value.length > 12 ? `${value.slice(0, 8)}…${value.slice(-3)}` : value;
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

type TcpFlowRow = {
  tcpFlowId: network.TcpFlowId;
  started?: network.TcpFlowStarted;
  ended?: network.TcpFlowEnded;
};

function correlateTcpFlows(events: readonly network.TcpFlowEvent[]): TcpFlowRow[] {
  const flows = new Map<network.TcpFlowId, TcpFlowRow>();
  const order: network.TcpFlowId[] = [];
  for (const event of events) {
    const id = event.tcpFlowId;
    const current = flows.get(id);
    if (!current) order.push(id);
    flows.set(id, event.type === "Started"
      ? { ...current, tcpFlowId: id, started: event }
      : { ...current, tcpFlowId: id, ended: event });
  }
  return order
    .slice(-512)
    .toReversed()
    .map((id) => flows.get(id))
    .filter((flow): flow is TcpFlowRow => flow !== undefined);
}

function dnsOutcomeTone(outcome: network.DnsRequestOutcome): BadgeTone {
  if (outcome.status === "Response") return outcome.responseCode === 0 ? "muted" : "warning";
  return outcome.status === "TimedOut" || outcome.status === "Overloaded" ? "warning" : "danger";
}

function tcpOutcomeTone(outcome?: network.TcpFlowOutcome): BadgeTone {
  if (!outcome) return "success";
  if (outcome.status === "Closed") return "muted";
  return outcome.status === "TimedOut" || outcome.status === "Unavailable" || outcome.status === "Overloaded"
    ? "warning"
    : "danger";
}

function tcpFlowDestination(flow: network.TcpFlowStarted): string {
  if (!flow.hostname) return flow.requestedDestination;
  const port = flow.requestedDestination.match(/:(\d+)$/)?.[1];
  return port ? `${flow.hostname}:${port}` : flow.hostname;
}

function dnsRecordType(value: number): string {
  return ({ 1: "A", 2: "NS", 5: "CNAME", 6: "SOA", 12: "PTR", 15: "MX", 16: "TXT", 28: "AAAA", 33: "SRV", 65: "HTTPS" } as Record<number, string>)[value]
    ?? `TYPE${value}`;
}

function dnsResponseCode(value: number): string {
  return ({ 0: "Success", 1: "FORMERR", 2: "SERVFAIL", 3: "NXDOMAIN", 4: "NOTIMP", 5: "REFUSED" } as Record<number, string>)[value]
    ?? `RCODE${value}`;
}

function resolvedAddressesLabel(response: network.DnsResponseSummary): string {
  return `${response.resolvedAddresses.join(", ")}${response.addressesTruncated ? ", …" : ""}`;
}

function splitLabel(value: string): string {
  return value.replace(/([a-z])([A-Z])/g, "$1 $2");
}

function formatTimestamp(value: string): string {
  const timestamp = new Date(String(value));
  return Number.isNaN(timestamp.getTime()) ? String(value) : timestamp.toLocaleTimeString();
}
