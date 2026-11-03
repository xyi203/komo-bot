import type { ReactNode, SVGProps } from "react";
import { cn } from "@/lib/utils";

// 原型 index.html 里的图标集（24 网格、1.5 描边、圆头），逐个搬过来，外观与原型一致。
const PATHS = {
  "plus": (
    <><path d="M12 5v14M5 12h14"/></>
  ),
  "search": (
    <><circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/></>
  ),
  "target": (
    <><circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="5"/><circle cx="12" cy="12" r="1"/></>
  ),
  "leaf": (
    <><path d="M11 20A7 7 0 0 1 9.8 6.1C15.5 5 17 4.48 19 2c1 2 2 4.18 2 8 0 5.5-4.78 10-10 10Z"/><path d="M2 21c0-3 1.85-5.36 5.08-6C9.5 14.52 12 13 13 12"/></>
  ),
  "clock": (
    <><circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/></>
  ),
  "sliders": (
    <><path d="M20 5h-7M9 5H4M20 12h-9M7 12H4M20 19h-5M11 19H4M13 3v4M7 10v4M15 17v4"/></>
  ),
  "chev-up": (
    <><path d="m6 15 6-6 6 6"/></>
  ),
  "up": (
    <><path d="m6 11 6-6 6 6M12 19V5"/></>
  ),
  "down": (
    <><path d="m7 10 5 5 5-5"/></>
  ),
  "right": (
    <><path d="m10 7 5 5-5 5"/></>
  ),
  "think": (
    <><path d="M9 18h6M10 21h4M12 3a6 6 0 0 0-3.8 10.6c.5.4.8 1 .8 1.7V16h6v-.7c0-.7.3-1.3.8-1.7A6 6 0 0 0 12 3Z"/></>
  ),
  "term": (
    <><path d="m5 17 5-5-5-5M12 19h7"/></>
  ),
  "code": (
    <><path d="m8 7-5 5 5 5M16 7l5 5-5 5"/></>
  ),
  "eye": (
    <><path d="M2 12s3.6-7 10-7 10 7 10 7-3.6 7-10 7S2 12 2 12Z"/><circle cx="12" cy="12" r="3"/></>
  ),
  "branch": (
    <><circle cx="6" cy="5" r="2"/><circle cx="6" cy="19" r="2"/><circle cx="18" cy="7" r="2"/><path d="M6 7v10M18 9a8 8 0 0 1-8 8H8"/></>
  ),
  "details": (
    <><rect x="4" y="4" width="16" height="16" rx="3"/><path d="M8 9.5h.01M8 14.5h.01M11 9.5h5M11 14.5h5" strokeLinecap="round"/></>
  ),
  "diff": (
    <><rect x="4" y="4" width="16" height="16" rx="3"/><path d="M12 7.5v5M9.5 10h5M9.5 16h5" strokeLinecap="round"/></>
  ),
  "sidebar": (
    <><rect x="3" y="4" width="18" height="16" rx="2"/><path d="M9 4v16"/></>
  ),
  "x": (
    <><path d="M6 6l12 12M18 6 6 18"/></>
  ),
  "plug": (
    <><path d="M9 3v5M15 3v5M6 8h12v3a6 6 0 0 1-12 0V8ZM12 17v4"/></>
  ),
  "folder": (
    <><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></>
  ),
  "list": (
    <><path d="M9 6h11M9 12h11M9 18h11M4.5 6h.01M4.5 12h.01M4.5 18h.01"/></>
  ),
  "calendar": (
    <><rect x="4" y="5" width="16" height="15" rx="2"/><path d="M4 10h16M9 3v4M15 3v4"/></>
  ),
  "eye-off": (
    <><path d="M3 3l18 18M10.6 6.1A9.6 9.6 0 0 1 12 6c6.4 0 10 6 10 6a17 17 0 0 1-3.3 3.9M6.6 7.6C3.9 9.4 2 12 2 12s3.6 6 10 6a9.4 9.4 0 0 0 4.2-1"/><path d="M9.9 9.9a3 3 0 0 0 4.2 4.2"/></>
  ),
  "chat": (
    <><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/></>
  ),
  "archive": (
    <><rect x="3" y="4" width="18" height="5" rx="1"/><path d="M5 9v9a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V9M10 13h4"/></>
  ),
  "check": (
    <><path d="m5 12 5 5 9-10"/></>
  ),
  "copy": (
    <><rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2"/></>
  ),
  "left": (
    <><path d="m14 7-5 5 5 5"/></>
  ),
  "inbox": (
    <><path d="M4 13h4l1.5 3h5L16 13h4"/><path d="M5.5 5h13L21 13v5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-5z"/></>
  ),
  "gear": (
    <><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"/></>
  ),
  "key": (
    <><circle cx="8" cy="15" r="4"/><path d="m10.8 12.2 8.7-8.7M17 6l3 3M14.5 8.5l2 2"/></>
  ),
  "more": (
    <><circle cx="5" cy="12" r="1.3" fill="currentColor"/><circle cx="12" cy="12" r="1.3" fill="currentColor"/><circle cx="19" cy="12" r="1.3" fill="currentColor"/></>
  ),
  "file": (
    <><path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8zM14 3v5h5"/></>
  ),
  "edit": (
    <><path d="M4 20h4L19 9l-4-4L4 16v4zM14 6l4 4"/></>
  ),
  "trash": (
    <><path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"/></>
  ),
  "stop": (
    <><rect x="7" y="7" width="10" height="10" rx="1.5"/></>
  ),
} satisfies Record<string, ReactNode>;

export type IconName = keyof typeof PATHS;

/** 图标：描边跟随 `currentColor`；`solid` 用于实心的（停止键）。 */
export function Icon({
  name,
  solid = false,
  className,
  ...props
}: { name: IconName; solid?: boolean } & Omit<SVGProps<SVGSVGElement>, "name">) {
  return (
    <svg
      viewBox="0 0 24 24"
      aria-hidden="true"
      className={cn(
        "size-4 shrink-0 stroke-current [stroke-linecap:round] [stroke-linejoin:round]",
        solid ? "fill-current stroke-none" : "fill-none stroke-[1.5]",
        className,
      )}
      {...props}
    >
      {PATHS[name]}
    </svg>
  );
}
