import { createFileRoute } from "@tanstack/react-router";
import { m } from "@/paraglide/messages.js";

export const Route = createFileRoute("/_app/")({
  component: () => (
    <div className="grid h-full place-items-center px-6 text-read text-faint">{m.select_session()}</div>
  ),
});
