import { useEffect, useState } from "react";

import type { repositories } from "../../api/generated/index.ts";

export function useApprovalReviewDelay(
  approvalId: repositories.RepositoryApprovalId | undefined,
): boolean {
  const [reviewed, setReviewed] = useState(false);

  useEffect(() => {
    setReviewed(false);
    if (!approvalId) return;
    let timer: number | undefined;
    let visibleFrame: number | undefined;
    const firstFrame = window.requestAnimationFrame(() => {
      visibleFrame = window.requestAnimationFrame(() => {
        timer = window.setTimeout(() => setReviewed(true), APPROVAL_REVIEW_DELAY_MS);
      });
    });
    return () => {
      window.cancelAnimationFrame(firstFrame);
      if (visibleFrame !== undefined) window.cancelAnimationFrame(visibleFrame);
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [approvalId]);

  return reviewed;
}

export function displayApprovalReference(reference: string): string {
  return reference.replace(/^refs\/(heads|tags)\//, "");
}

export function formatApprovalUpdateCount(count: number): string {
  return `${count} ${count === 1 ? "reference" : "references"}`;
}

export function approvalReferenceKind(reference: string): string {
  return reference.startsWith("refs/tags/") ? "Tag" : "Branch";
}

const APPROVAL_REVIEW_DELAY_MS = 1_000;
