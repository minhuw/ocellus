import { codeToHtml } from "shiki";
import { CodeCopyButton } from "./code-copy-button";

type CodeBlockProps = {
  children: string;
  language: "shell" | "yaml";
};

export async function CodeBlock({ children, language }: CodeBlockProps) {
  const html = await codeToHtml(children, {
    lang: language,
    theme: "github-dark",
  });

  return (
    <div className="copyable-code">
      <div
        className="shiki-code"
        dangerouslySetInnerHTML={{ __html: html }}
      />
      <CodeCopyButton code={children} />
    </div>
  );
}
