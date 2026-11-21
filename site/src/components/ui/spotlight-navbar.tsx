import { useEffect, useRef } from "react";
import { cn } from "@/lib/utils";

export interface NavItem {
  label: string;
  href: string;
}

export interface SpotlightNavbarProps {
  items: NavItem[];
  className?: string;
  onItemClick?: (item: NavItem, index: number) => void;
  activeIndex?: number;
}

export function SpotlightNavbar({
  items,
  className,
  onItemClick,
  activeIndex = 0,
}: SpotlightNavbarProps) {
  const navRef = useRef<HTMLDivElement>(null);
  const spotlightRef = useRef<HTMLDivElement>(null);
  const ambienceRef = useRef<HTMLDivElement>(null);
  const itemRefs = useRef<Array<HTMLAnchorElement | null>>([]);

  useEffect(() => {
    const nav = navRef.current;
    const bar = ambienceRef.current;
    if (!nav || !bar) return;

    const move = () => {
      const item = itemRefs.current[activeIndex];
      if (!item) return;
      const navRect = nav.getBoundingClientRect();
      const itemRect = item.getBoundingClientRect();
      bar.style.width = `${Math.max(itemRect.width - 16, 20)}px`;
      bar.style.left = `${itemRect.left - navRect.left + 8}px`;
    };

    move();
    const observer = new ResizeObserver(move);
    observer.observe(nav);
    window.addEventListener("resize", move);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", move);
    };
  }, [activeIndex, items.length]);

  useEffect(() => {
    const nav = navRef.current;
    const glow = spotlightRef.current;
    if (!nav || !glow) return;

    const onMove = (event: MouseEvent) => {
      const rect = nav.getBoundingClientRect();
      glow.style.setProperty("--spotlight-x", `${event.clientX - rect.left}px`);
      glow.style.opacity = "1";
    };
    const onLeave = () => {
      glow.style.opacity = "0";
    };

    nav.addEventListener("mousemove", onMove);
    nav.addEventListener("mouseleave", onLeave);
    return () => {
      nav.removeEventListener("mousemove", onMove);
      nav.removeEventListener("mouseleave", onLeave);
    };
  }, []);

  return (
    <div className={cn("relative flex justify-center", className)}>
      <nav
        ref={navRef}
        className="spotlight-nav spotlight-nav-bg glass-border spotlight-nav-shadow relative h-11 overflow-hidden rounded-full transition-all duration-300"
      >
        <ul className="relative z-10 flex h-full items-center px-2">
          {items.map((item, idx) => (
            <li
              key={idx}
              className="relative flex h-full items-center justify-center"
            >
              <a
                ref={(node) => {
                  itemRefs.current[idx] = node;
                }}
                href={item.href}
                onClick={(event) => {
                  event.preventDefault();
                  onItemClick?.(item, idx);
                }}
                className={cn(
                  "rounded-full px-4 py-2 text-sm font-medium transition-colors duration-200 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/30",
                  activeIndex === idx
                    ? "text-white"
                    : "text-neutral-400 hover:text-white"
                )}
              >
                {item.label}
              </a>
            </li>
          ))}
        </ul>

        <div
          ref={spotlightRef}
          className="pointer-events-none absolute inset-0 z-[1] opacity-0 transition-opacity duration-300"
          style={{
            background:
              "radial-gradient(120px circle at var(--spotlight-x) 100%, rgba(255,255,255,0.14), transparent 50%)",
          }}
        />

        <div
          ref={ambienceRef}
          className="pointer-events-none absolute bottom-0 z-[2] h-[2px] rounded-full"
          style={{
            background: "linear-gradient(90deg, var(--mint), var(--blue))",
            transition:
              "left 300ms cubic-bezier(.2,.7,.3,1), width 300ms cubic-bezier(.2,.7,.3,1)",
          }}
        />
      </nav>
    </div>
  );
}
