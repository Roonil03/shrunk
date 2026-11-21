import { useEffect, useRef } from "react";
import { cn } from "@/lib/utils";

interface StatsCounterProps {
  value: number;
  suffix?: string;
  decimals?: number;
  className?: string;
}

export default function StatsCounter({
  value,
  suffix = "",
  decimals = 0,
  className,
}: StatsCounterProps) {
  const ref = useRef<HTMLSpanElement>(null);
  const label = value.toFixed(decimals) + suffix;

  useEffect(() => {
    const el = ref.current;
    if (!el) return;

    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      el.textContent = label;
      return;
    }

    let raf = 0;
    let started = 0;
    const duration = 1200;
    const write = (n: number) => {
      el.textContent = n.toFixed(decimals) + suffix;
    };
    write(0);

    const tick = (t: number) => {
      if (!started) started = t;
      const progress = Math.min((t - started) / duration, 1);
      const eased = 1 - Math.pow(1 - progress, 3);
      write(value * eased);
      if (progress < 1) raf = requestAnimationFrame(tick);
    };

    const io = new IntersectionObserver(
      (entries) => {
        if (!entries.some((entry) => entry.isIntersecting)) return;
        io.disconnect();
        raf = requestAnimationFrame(tick);
      },
      { rootMargin: "-80px" }
    );
    io.observe(el);

    return () => {
      io.disconnect();
      cancelAnimationFrame(raf);
    };
  }, [value, decimals, suffix, label]);

  return (
    <span className={cn("relative inline-block", className)}>
      <span className="sr-only">{label}</span>
      <span aria-hidden="true" className="invisible">
        {label}
      </span>
      <span
        aria-hidden="true"
        ref={ref}
        className="absolute left-0 top-0 whitespace-nowrap"
      />
    </span>
  );
}
