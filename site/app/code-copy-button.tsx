"use client";

import { useState } from "react";

type CodeCopyButtonProps = {
  code: string;
};

export function CodeCopyButton({ code }: CodeCopyButtonProps) {
  const [copied, setCopied] = useState(false);

  async function copyCode() {
    await navigator.clipboard.writeText(code);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1600);
  }

  return (
    <button
      type="button"
      className="code-copy-button"
      onClick={() => void copyCode()}
    >
      {copied ? "Copied" : "Copy"}
    </button>
  );
}
