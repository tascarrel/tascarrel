import type {
  chats,
  common,
  config,
  workspaces,
} from "../../api/generated/index.ts";
import {
  formatCompactNumber,
  formatMoney,
  formatNumber,
} from "../chat/model/usage.ts";
import { useChatUsageReport } from "../chat/state.ts";

export function UsageReport({
  workspace,
  from,
  until,
  usageSettings,
}: {
  workspace: workspaces.WorkspaceName;
  from: common.Timestamp;
  until: common.Timestamp;
  usageSettings: config.WorkspaceUsageSettings;
}) {
  const state = useChatUsageReport(workspace, from, until);
  if (state.error) {
    return (
      <p
        className="mt-4 rounded-xl border border-red-500/20 bg-red-500/5 px-3 py-2 text-xs text-red-200"
        role="alert"
      >
        {state.error.message}
      </p>
    );
  }
  if (!state.value) {
    return <p className="mt-4 text-xs text-subtle">Loading usage…</p>;
  }

  const report = state.value;
  const totalTokens = integer(report.total.tokens.inputTokens)
    + integer(report.total.tokens.outputTokens);
  const pricedTurns = integer(report.total.pricedTurnCount);
  const turnCount = integer(report.total.turnCount);
  const provisionalTurns = integer(report.total.provisionalTurnCount);
  const primaryAgentTurns = integer(report.total.primaryAgentTurnCount);
  const rows = report.costCenters;

  return (
    <div className="mt-4">
      <dl className="grid gap-px overflow-hidden rounded-xl border border-ui-border bg-ui-border sm:grid-cols-4">
        <UsageMetric label="Tokens" value={formatCompactNumber(totalTokens)} />
        <UsageMetric
          label="Estimated cost"
          value={formatCosts(report.total.calculatedCosts)}
        />
        <UsageMetric label="Chats" value={formatNumber(integer(report.total.chatCount))} />
        <UsageMetric
          label="Pricing coverage"
          value={turnCount === 0n
            ? "—"
            : `${formatNumber(pricedTurns)} / ${formatNumber(turnCount)} turns`}
        />
      </dl>

      {rows.length ? (
        <div className="mt-4 overflow-x-auto rounded-xl border border-ui-border">
          <table className="w-full min-w-[760px] border-collapse text-left text-xs">
            <thead className="bg-surface-raised text-[10px] uppercase tracking-wide text-subtle">
              <tr>
                <th className="px-3 py-2 font-medium">Cost center</th>
                <th className="px-3 py-2 text-right font-medium">Input</th>
                <th className="px-3 py-2 text-right font-medium">Output</th>
                <th className="px-3 py-2 text-right font-medium">Cached</th>
                <th className="px-3 py-2 text-right font-medium">Reasoning</th>
                <th className="px-3 py-2 text-right font-medium">Estimated cost</th>
                <th className="px-3 py-2 text-right font-medium">Chats / turns</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-ui-border">
              {rows.map((row) => (
                <UsageRow
                  key={row.costCenterId ? `cost-center:${row.costCenterId}` : "unassigned"}
                  row={row}
                  usageSettings={usageSettings}
                />
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <p className="mt-4 rounded-xl border border-ui-border bg-surface/30 px-4 py-5 text-xs text-subtle">
          No recorded chat usage for this month.
        </p>
      )}

      <p className="mt-3 text-[10px] leading-5 text-subtle">
        Costs are local estimates based on the pricing snapshot stored with each turn; provider
        billing is authoritative. {pricedTurns < turnCount
          ? `${formatNumber(turnCount - pricedTurns)} turns have no calculated price. `
          : ""}
        {provisionalTurns > 0n
          ? `${formatNumber(provisionalTurns)} turns still have provisional usage. `
          : ""}
        {primaryAgentTurns > 0n
          ? `${formatNumber(primaryAgentTurns)} turns report only primary-agent usage.`
          : ""}
      </p>
    </div>
  );
}

function UsageMetric({ label, value }: { label: string; value: string }) {
  return (
    <div className="bg-surface/40 px-3 py-3">
      <dt className="text-[10px] text-subtle">{label}</dt>
      <dd className="mt-1 truncate text-sm font-semibold tabular-nums text-foreground" title={value}>
        {value}
      </dd>
    </div>
  );
}

function UsageRow({
  row,
  usageSettings,
}: {
  row: chats.ChatCostCenterUsage;
  usageSettings: config.WorkspaceUsageSettings;
}) {
  const configured = row.costCenterId
    ? usageSettings.costCenters?.[row.costCenterId]
    : undefined;
  const label = row.costCenterId
    ? configured?.name ?? row.costCenterId
    : "Unassigned";
  const pricedTurns = integer(row.usage.pricedTurnCount);
  const turns = integer(row.usage.turnCount);
  const cost = formatCosts(row.usage.calculatedCosts);

  return (
    <tr className="bg-surface/20 text-muted">
      <td className="px-3 py-2.5">
        <span className="font-medium text-foreground">{label}</span>
        {row.costCenterId && !configured ? (
          <span className="ml-2 text-[10px] text-amber-300">Unconfigured</span>
        ) : configured?.archived === true ? (
          <span className="ml-2 text-[10px] text-subtle">Archived</span>
        ) : null}
      </td>
      <UsageNumber value={row.usage.tokens.inputTokens} />
      <UsageNumber value={row.usage.tokens.outputTokens} />
      <UsageNumber value={row.usage.tokens.cacheReadInputTokens} />
      <UsageNumber value={row.usage.tokens.reasoningOutputTokens} />
      <td className="px-3 py-2.5 text-right tabular-nums">
        {cost}
        {pricedTurns < turns ? (
          <span className="block text-[10px] text-subtle">
            {formatNumber(pricedTurns)} / {formatNumber(turns)} turns priced
          </span>
        ) : null}
      </td>
      <td className="px-3 py-2.5 text-right tabular-nums">
        {formatNumber(integer(row.usage.chatCount))} / {formatNumber(turns)}
      </td>
    </tr>
  );
}

function UsageNumber({ value }: { value?: chats.ChatTokenUsage["inputTokens"] }) {
  return (
    <td className="px-3 py-2.5 text-right tabular-nums">
      {value === undefined ? "Unknown" : formatNumber(integer(value))}
    </td>
  );
}

function formatCosts(costs: readonly common.Money[]): string {
  if (!costs.length) return "Not priced";
  return costs
    .map((cost) => formatMoney(cost.currency, integer(cost.amount)))
    .join(" + ");
}

function integer(value: string | number | bigint): bigint {
  return BigInt(String(value));
}
