export function displayApprovalReference(reference: string): string {
  return reference.replace(/^refs\/(heads|tags)\//, "");
}

export function formatApprovalUpdateCount(count: number): string {
  return `${count} ${count === 1 ? "reference" : "references"}`;
}

export function approvalReferenceKind(reference: string): string {
  return reference.startsWith("refs/tags/") ? "Tag" : "Branch";
}
