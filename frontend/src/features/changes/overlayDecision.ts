import { useEffect, useState } from "react";

import { hostApi } from "../../api/client.ts";
import type { shares, workspaces } from "../../api/generated/index.ts";

export type OverlayDecisionState = {
  pendingDecision?: shares.ShareOverlayApprovalDecision["tag"];
  resolving: boolean;
  resolution?: shares.ShareOverlayApprovalResolution;
  error?: string;
  setPendingDecision: (decision: shares.ShareOverlayApprovalDecision["tag"] | undefined) => void;
  resolve: () => Promise<void>;
};

export function useOverlayDecision(
  workspace: workspaces.WorkspaceName,
  approval: shares.ShareOverlayApprovalRequest,
): OverlayDecisionState {
  const [pendingDecision, setPendingDecision] = useState<shares.ShareOverlayApprovalDecision["tag"]>();
  const [resolving, setResolving] = useState(false);
  const [resolution, setResolution] = useState<shares.ShareOverlayApprovalResolution>();
  const [error, setError] = useState<string>();

  useEffect(() => {
    setPendingDecision(undefined);
    setResolving(false);
    setResolution(undefined);
    setError(undefined);
  }, [approval.id]);

  const resolve = async () => {
    if (!pendingDecision || resolving) return;
    setResolving(true);
    setError(undefined);
    try {
      const output = await hostApi.execute("shares_ResolveApproval", {
        workspace,
        approvalId: approval.id,
        decision: { tag: pendingDecision },
      });
      setResolution(output.result);
      setPendingDecision(undefined);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setResolving(false);
    }
  };

  return {
    pendingDecision,
    resolving,
    resolution,
    error,
    setPendingDecision,
    resolve,
  };
}
