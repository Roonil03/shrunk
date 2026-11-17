import type { ReactNode } from "react";

export default function StaggerText({
  children,
  delay = 0,
  className,
}: {
  children: ReactNode;
  delay?: number;
  className?: string;
}) {
  if (typeof children !== "string") return <>{children}</>;

  const words = children.split(" ");

  return (
    <span className={className} style={{ display: "inline-block" }}>
      {words.map((word, i) => (
        <span
          key={i}
          className="relative inline-block overflow-hidden"
          style={{ verticalAlign: "top" }}
        >
          <span
            className="word inline-block will-change-transform"
            style={{ animationDelay: `${delay + i * 50}ms` }}
          >
            {word}
            {i < words.length - 1 ? "\u00A0" : ""}
          </span>
        </span>
      ))}
    </span>
  );
}
