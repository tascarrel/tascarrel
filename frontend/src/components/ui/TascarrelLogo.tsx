import tascarrelLogo from "../../assets/tascarrel.svg";

/** Renders the Tascarrel brand mark alongside an adjacent accessible product name. */
export function TascarrelLogo({ className = "" }: { className?: string }) {
  return <img className={className} src={tascarrelLogo} alt="" />;
}
